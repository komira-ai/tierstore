//! The S3 operations: each is calls to the backend and the answer S3 gives.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use hyper::body::Body;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Method, StatusCode};
use objstore_s3::key_rules;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinError;

use crate::journal::is_reserved;
use crate::{Error, Meta, Opened, ReadBody, Span, Window};

use super::Server;
use super::bucket;
use super::chunked::Framing;
use super::delete_objects::DELETE_BODY_LIMIT;
use super::handover::hand_over;
use super::list_token;
use super::listing::{self, Start, UploadsNext};
use super::presign::Grant;
use super::reply::{Answer, S3Error, answer, streamed_answer, xml_answer};
use super::request::{
    BoxError, Check, Query, Reader, Refused, Target, header, pieces, query_pairs,
};
use super::time::{http_date, parse_http_date};
use super::xml::{
    self, Listing, ListingV1, NULL_VERSION, PartListing, UploadListing, VersionListing,
};

/// The query parameters this front understands. A request with another is not served.
/// `delimiter` and `encoding-type` are parameters of ListObjectsV2, ListObjects (which also
/// has `marker`) and ListMultipartUploads, and with `fetch-owner=true` the objects of a listing carry the `Owner` that S3 adds then
/// ([`xml::OWNER`]).
/// `x-id` names the operation (`?x-id=PutObject`) and S3 ignores it; the AWS SDKs add it to
/// the calls of an object, so a request with it is served as one without.
const SUPPORTED: [&str; 22] = [
    "continuation-token",
    "delete",
    "delimiter",
    "encoding-type",
    "fetch-owner",
    "key-marker",
    "list-type",
    "marker",
    "max-keys",
    "max-parts",
    "max-uploads",
    "part-number-marker",
    "partNumber",
    "prefix",
    "start-after",
    "upload-id-marker",
    "uploadId",
    "uploads",
    "version-id-marker",
    "versionId",
    "versions",
    "x-id",
];

/// The query parameters of SigV4 query-string authentication, which only a presigned request
/// carries: the check of the signature has read them, and they are not parameters of a call.
const PRESIGN_PARAMS: [&str; 6] = [
    "X-Amz-Algorithm",
    "X-Amz-Credential",
    "X-Amz-Date",
    "X-Amz-Expires",
    "X-Amz-SignedHeaders",
    "X-Amz-Signature",
];

/// The query parameters of a read that set a header of the answer, each with the header it sets
/// (the `response-*` parameters of GetObject and HeadObject).
pub(super) const RESPONSE_OVERRIDES: [(&str, &str); 6] = [
    ("response-cache-control", "cache-control"),
    ("response-content-disposition", "content-disposition"),
    ("response-content-encoding", "content-encoding"),
    ("response-content-language", "content-language"),
    ("response-content-type", "content-type"),
    ("response-expires", "expires"),
];

/// The query parameters that make a request part of a multipart upload.
pub(super) const MULTIPART: [&str; 3] = ["partNumber", "uploadId", "uploads"];

/// The most keys in a page of a listing, and the number of parts of an upload.
pub(super) const PAGE_LIMIT: usize = 1000;
const PART_LIMIT: u32 = 10_000;

/// Whether a call is DeleteObjects: a `POST` to the bucket with `?delete`.
pub(super) fn deletes_many(method: &Method, target: &Target, query: &Query) -> bool {
    *method == Method::POST && *target == Target::Bucket && query.has("delete")
}

/// The most bytes the body of a call may have: the limit of the server, and for DeleteObjects
/// what 1000 keys fill ([`DELETE_BODY_LIMIT`]), since that body is held whole.
pub(super) fn body_limit(limit: usize, deleting: bool) -> usize {
    if deleting {
        limit.min(DELETE_BODY_LIMIT)
    } else {
        limit
    }
}

/// The range a `Range` header asks for, if it is one valid single range (`bytes=<first>-<last>`,
/// `bytes=<first>-` or `bytes=-<count>`); anything else is no range, and asks for the whole
/// object. What it covers of an object is for the read to say, which knows the size
/// ([`Span::resolve`]).
pub(crate) fn asked(range: Option<&str>) -> Option<Span> {
    let spec = range?.strip_prefix("bytes=")?;
    let (first, last) = spec.split_once('-')?;
    let number = |text: &str| text.parse::<u64>().ok();
    match (first, last) {
        ("", count) => Some(Span::Suffix(number(count)?)),
        (first, "") => Some(Span::From {
            first: number(first)?,
            last: None,
        }),
        (first, last) => {
            let (first, last) = (number(first)?, number(last)?);
            (first <= last).then_some(Span::From {
                first,
                last: Some(last),
            })
        }
    }
}

/// The headers that describe an object.
fn object_headers(meta: &Meta) -> Vec<(&'static str, String)> {
    vec![
        ("etag", format!("\"{}\"", meta.tag)),
        ("last-modified", http_date(meta.modified)),
        ("accept-ranges", "bytes".to_owned()),
        ("content-type", "application/octet-stream".to_owned()),
    ]
}

/// The answer to a read of `len` bytes: the chunks of `body` as they come, which end the
/// connection without a clean end unless they are exactly `len` bytes. (A body of no bytes is
/// held whole: there is nothing to stream.)
fn read_answer(
    status: StatusCode,
    headers: &[(&'static str, String)],
    len: u64,
    body: ReadBody,
) -> Answer {
    if len == 0 {
        return answer(status, headers, Bytes::new());
    }
    streamed_answer(status, headers, len, body)
}

/// A client may not name the keys the proxy keeps for itself.
fn refuse_reserved(key: &str) -> Result<(), S3Error> {
    if is_reserved(key) {
        return Err(S3Error::from(Error::Reserved(key.to_owned())));
    }
    Ok(())
}

/// A key that is stored must be one S3 accepts and one a listing can carry:
///
/// - S3's own rules (`objstore_s3::key_rules::s3_key`): over 1024 bytes is [`Error::InvalidKey`]
///   (400 `KeyTooLongError`, as S3 answers). Any other text is a key. (A `.` or `..` segment is
///   one too: an S3 tier refuses it itself, 400 `InvalidArgument` with a message that names the
///   rule, see `objstore_s3::key_rules::s3_client_key` and [`Error::InvalidSegment`].)
/// - A character a listing can carry: the answers of a listing are XML 1.0, which has no
///   character for U+0000-U+0008, U+000B, U+000C, U+000E-U+001F, U+FFFE and U+FFFF (tab, line
///   feed, carriage return, U+007F and the C1 controls are allowed). A client that does not ask
///   for `encoding-type=url` gets keys as they are, so a key with one would make every page that
///   lists it malformed for such a client; a listing with `encoding-type=url` could carry the
///   key, and the set of characters a key may have is the narrow one all the same.
fn refuse_unstorable(key: &str) -> Result<(), S3Error> {
    if key_rules::s3_key(key).is_err() {
        return Err(S3Error::from(Error::InvalidKey(key.to_owned())));
    }
    if key.chars().any(xml::forbids) {
        return Err(S3Error::invalid(
            "a key may not have a character XML 1.0 forbids (U+0000-U+0008, U+000B, U+000C, \
             U+000E-U+001F, U+FFFE, U+FFFF)",
        ));
    }
    Ok(())
}

/// The length of the data of a body, if the request says: the `x-amz-decoded-content-length` of
/// an `aws-chunked` body, otherwise the `Content-Length`.
fn body_len(parts: &Parts, check: &Check) -> Option<u64> {
    let decoded = check.framing().map(Framing::decoded);
    let declared = header(&parts.headers, "content-length");
    decoded.or_else(|| declared.and_then(|text| text.parse().ok()))
}

/// Whether the listing asked for `encoding-type=url`. S3 knows no other encoding type, and says
/// so for any other text; an empty one is none.
fn encoded(query: &Query) -> Result<bool, S3Error> {
    match query.get("encoding-type") {
        None | Some("") => Ok(false),
        Some("url") => Ok(true),
        Some(_) => Err(S3Error::invalid(
            "Invalid Encoding Method specified in Request",
        )),
    }
}

/// Refuses what this front does not do whatever the call is: a query parameter it does not
/// know, and a copy.
fn refuse_unsupported(parts: &Parts, query: &Query, presigned: bool) -> Result<(), S3Error> {
    let extra: &[&str] = if presigned { &PRESIGN_PARAMS } else { &[] };
    let overrides = RESPONSE_OVERRIDES.iter().map(|(param, _)| param);
    let allowed: Vec<&str> = SUPPORTED
        .iter()
        .chain(extra)
        .chain(overrides)
        .copied()
        .collect();
    if !query.only(&allowed) {
        return Err(S3Error::unsupported(
            "this query parameter is not supported",
        ));
    }
    if header(&parts.headers, "x-amz-copy-source").is_some() {
        return Err(S3Error::unsupported("copying an object is not supported"));
    }
    Ok(())
}

/// Refuses a GET or HEAD that carries a `response-*` parameter and is not signed (no
/// `Authorization` header and no SigV4 parameter in the query): S3 answers it 400
/// `InvalidRequest`, since a reader who is not known may not choose the headers of an answer.
pub(super) fn refuse_unsigned_overrides(parts: &Parts) -> Result<(), S3Error> {
    let signed = parts.headers.contains_key("authorization");
    let pairs = query_pairs(parts.uri.query()).unwrap_or_default();
    let presigned = pairs.iter().any(|(name, _)| name == "X-Amz-Algorithm");
    let overridden = pairs
        .iter()
        .any(|(name, _)| RESPONSE_OVERRIDES.iter().any(|(param, _)| param == name));
    if signed || presigned || !overridden {
        return Ok(());
    }
    Err(S3Error::new(
        StatusCode::BAD_REQUEST,
        "InvalidRequest",
        "Request specific response headers cannot be used for anonymous GET requests.",
    ))
}

/// The headers that the `response-*` parameters of `query` set, in the order of
/// [`RESPONSE_OVERRIDES`]. A parameter with an empty value sets nothing.
fn overrides(query: &Query) -> Result<Vec<(&'static str, HeaderValue)>, S3Error> {
    let mut set = Vec::new();
    for (param, name) in RESPONSE_OVERRIDES {
        let Some(text) = query.get(param).filter(|text| !text.is_empty()) else {
            continue;
        };
        let value = HeaderValue::from_str(text)
            .map_err(|_| S3Error::invalid(format!("{param} is not a valid header value")))?;
        set.push((name, value));
    }
    Ok(set)
}

/// `answer` with the headers of `set` in place of its own.
fn overridden(mut answer: Answer, set: Vec<(&'static str, HeaderValue)>) -> Answer {
    for (name, value) in set {
        answer
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
    answer
}

/// The size of a page a listing asked for in the parameter `name` (`max-keys` or
/// `max-uploads`), at most [`PAGE_LIMIT`]. `0` is a size: S3 answers it with an empty,
/// untruncated page. A text that is not a whole number from 0 to 2147483647 is `InvalidArgument`
/// with the words S3 uses, where `argument` is the name S3 gives the parameter (`maxKeys`).
fn page_size(query: &Query, name: &str, argument: &str) -> Result<usize, S3Error> {
    let Some(text) = query.get(name) else {
        return Ok(PAGE_LIMIT);
    };
    match text.parse::<i32>() {
        Ok(count) if count >= 0 => Ok(usize::try_from(count).unwrap_or(0).min(PAGE_LIMIT)),
        _ => Err(S3Error::invalid(format!(
            "Argument {argument} must be an integer between 0 and 2147483647"
        ))),
    }
}

/// Whether the header `name` of the request names `tag` among its entity tags (`*` names every
/// tag), if the request has the header.
fn names(parts: &Parts, name: &str, tag: &str) -> Option<bool> {
    let list = header(&parts.headers, name)?;
    let mut listed = list.split(',').map(|item| item.trim().trim_matches('"'));
    Some(listed.any(|item| ["*", tag].contains(&item)))
}

/// The time, in whole seconds, that the header `name` of the request holds, if it has the header
/// and it is an HTTP date. S3 ignores a date that is not one, and so does this.
fn date_of(parts: &Parts, name: &str) -> Option<u64> {
    parse_http_date(header(&parts.headers, name)?).map(|millis| millis / 1000)
}

/// The answer that the conditions of a read give in place of the object, if they do, in the order
/// of RFC 7232 (section 6): `PreconditionFailed` when `If-Match` does not name the tag of the
/// object, or, without an `If-Match`, when `If-Unmodified-Since` is before the time of the
/// object; then 304 when `If-None-Match` names the tag, or, without an `If-None-Match`, when
/// `If-Modified-Since` is not before the time of the object. A date is compared to the second,
/// as `Last-Modified` shows it, and one that is not a date is ignored. `None` when the read goes
/// on.
fn conditional(parts: &Parts, key: &str, meta: &Meta) -> Result<Option<Answer>, S3Error> {
    let modified = meta.modified / 1000;
    let unmodified_since = || date_of(parts, "if-unmodified-since").is_some_and(|at| modified > at);
    let failed = match names(parts, "if-match", &meta.tag) {
        Some(named) => !named,
        None => unmodified_since(),
    };
    if failed {
        return Err(S3Error::from(Error::PreconditionFailed(key.to_owned())));
    }
    let not_modified = match names(parts, "if-none-match", &meta.tag) {
        Some(named) => named,
        None => date_of(parts, "if-modified-since").is_some_and(|at| modified <= at),
    };
    if not_modified {
        let headers = object_headers(meta);
        return Ok(Some(answer(
            StatusCode::NOT_MODIFIED,
            &headers,
            Bytes::new(),
        )));
    }
    Ok(None)
}

/// Runs a call that changes the store to its end on a task of its own. The server drops the
/// future of a request when the client goes away, and a delete dropped after it removed the
/// object from one tier would leave it in the other. The completion of a multipart upload is
/// carried to its end too, so that the parts it joins are not left half joined. (A put does
/// the same on its own, see `put_streamed`.)
pub(super) async fn detached<T, F>(call: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: Future<Output = Result<T, Error>> + Send + 'static,
{
    joined(tokio::spawn(call).await)
}

/// What a call run on a task of its own came to: its result, or, if the task panicked or was
/// cancelled, a failure of the call.
fn joined<T>(done: Result<Result<T, Error>, JoinError>) -> Result<T, Error> {
    done.unwrap_or_else(|lost| {
        Err(Error::Other(format!(
            "a call to the backend did not finish: {lost}"
        )))
    })
}

/// Whether a put only creates, which is what it does when it has no `If-Match` and its
/// `If-None-Match` is absent or `*`: the body of such a put is streamed to the backend.
pub(super) fn creates_only(headers: &HeaderMap) -> bool {
    let conditions = (
        header(headers, "if-match"),
        header(headers, "if-none-match"),
    );
    matches!(conditions, (None, None | Some("*")))
}

/// Holds a call that names a version with `versionId` to what the bucket has. Objects are
/// immutable and the bucket is not versioned, so an object has one version, `null` (S3 names it
/// so in a bucket that was never versioned): a GET, a HEAD or a DELETE of an object may name it,
/// and any other id is `NoSuchVersion`. The key of the proxy's own is refused first, as it is
/// by every call. Any other call does not take the parameter.
fn named_version(method: &str, target: &Target, multipart: bool, id: &str) -> Result<(), S3Error> {
    match (method, target, multipart) {
        ("GET" | "HEAD" | "DELETE", Target::Object(key), false) => {
            refuse_reserved(key)?;
            if id == NULL_VERSION {
                Ok(())
            } else {
                Err(S3Error::no_such_version())
            }
        }
        _ => Err(S3Error::unsupported(
            "versionId is only supported on a GET, HEAD or DELETE of an object",
        )),
    }
}

/// The answer to a failed call on a multipart upload: where the backend has no such upload, it
/// is `NoSuchUpload`, not `NoSuchKey`.
fn upload_error(error: Error) -> S3Error {
    match error {
        Error::NotFound(_) => S3Error::no_such_upload(),
        other => S3Error::from(other),
    }
}

/// How the key of a request may write an object, which the server knows from the key and not from
/// the request.
#[derive(Clone, Copy)]
pub(super) enum Writes<'a> {
    /// Create-only: a key that exists is refused (412). The scoped keys, whose write right does not
    /// replace.
    Creates,
    /// Create-only in the hot tier alone, within what the grant of a presigned URL allows.
    Presigned(&'a Grant),
    /// The key of the whole front: a `PUT` with no `If-None-Match` is S3's plain put and replaces
    /// the key where the backend replaces
    /// ([`Backend::put_stream_replacing`](crate::Backend::put_stream_replacing)); `If-None-Match: *`
    /// creates.
    Full,
}

impl<'a> Writes<'a> {
    /// The grant of a presigned URL, if the request is one.
    fn grant(self) -> Option<&'a Grant> {
        match self {
            Self::Presigned(grant) => Some(grant),
            Self::Creates | Self::Full => None,
        }
    }
}

impl Server {
    /// Does what a request asks for, once it is signed. The body of a multipart part and that
    /// of a put with no `If-Match` and no `If-None-Match` other than `*` are streamed to the
    /// backend as they arrive; every other call reads its body first.
    pub(super) async fn dispatch<B>(
        &self,
        parts: &Parts,
        target: Target,
        query: &Query,
        reader: Reader<B>,
        check: Check,
        writes: Writes<'_>,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        // A request with no `Authorization` header is a presigned URL: its SigV4 parameters are
        // not parameters of the call.
        let method = parts.method.as_str();
        let setting = bucket::asks_setting(method, &target, query);
        let buckets = bucket::asks_buckets(method, &target);
        if setting.is_none() && !buckets {
            refuse_unsupported(parts, query, !parts.headers.contains_key("authorization"))?;
        }
        let deleting = deletes_many(&parts.method, &target, query);
        if query.has("delete") && !deleting {
            return Err(S3Error::unsupported("this operation is not supported"));
        }
        if deleting {
            return self.delete_objects(parts, reader, check).await;
        }
        let multipart = query.has_any(&MULTIPART);
        if let Some(id) = query.get("versionId") {
            named_version(method, &target, multipart, id)?;
        }
        if let ("PUT", Target::Object(key), true) = (method, &target, multipart) {
            let len = body_len(parts, &check);
            return self.upload_part(query, key, len, reader, check).await;
        }
        if let ("PUT", Target::Object(key), false) = (method, &target, multipart)
            && creates_only(&parts.headers)
        {
            let len = body_len(parts, &check);
            // `If-None-Match: *` asks to create, whatever the key may do.
            let writes = match writes {
                Writes::Full if parts.headers.contains_key("if-none-match") => Writes::Creates,
                other => other,
            };
            return self.put_streamed(key, len, reader, check, writes).await;
        }
        let body = reader.read_all(check).await?;
        if let Some(setting) = &setting {
            return self.setting(setting);
        }
        if buckets {
            return self.list_buckets(query);
        }
        match (method, target, multipart) {
            ("PUT", Target::Bucket, false) => self.create_bucket(&body),
            ("HEAD", Target::Bucket, false) => Ok(self.head_bucket()),
            ("DELETE", Target::Bucket, false) => self.delete_bucket().await,
            ("GET", Target::Bucket, false) if query.has("versions") => {
                self.list_versions(query).await
            }
            ("GET", Target::Bucket, false) => self.list_objects(query).await,
            ("GET", Target::Bucket, true) if query.has("uploads") => self.list_uploads(query).await,
            ("GET", Target::Object(key), true) if query.has("uploadId") => {
                self.list_parts(query, &key).await
            }
            ("GET", Target::Object(key), false) => self.get_object(parts, query, &key).await,
            ("HEAD", Target::Object(key), false) => self.head_object(parts, query, &key).await,
            ("PUT", Target::Object(key), false) => self.put_conditional(parts, &key).await,
            ("DELETE", Target::Object(key), false) => self.delete_object(&key).await,
            ("DELETE", Target::Object(key), true) => self.abort_upload(query, &key).await,
            ("POST", Target::Object(key), true) if query.has("uploads") => {
                self.start_upload(&key).await
            }
            ("POST", Target::Object(key), true) if query.has("uploadId") => {
                let replaces =
                    matches!(writes, Writes::Full) && !parts.headers.contains_key("if-none-match");
                self.complete_upload(query, &key, body, replaces).await
            }
            _ => Err(S3Error::unsupported("this operation is not supported")),
        }
    }

    /// One call to the backend, which gives the object's meta and the body as it arrives: the
    /// headers are made from the meta and the status from the window the body holds. A
    /// condition that is not met, or a range that is not satisfiable, drops the body unread.
    async fn get_object(&self, parts: &Parts, query: &Query, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let set = overrides(query)?;
        let span = asked(header(&parts.headers, "range"));
        let Opened { meta, window, body } = self.backend.open_read(key, span).await?;
        if let Some(early) = conditional(parts, key, &meta)? {
            return Ok(early);
        }
        let mut headers = object_headers(&meta);
        match window {
            Window::Whole => {
                let whole = read_answer(StatusCode::OK, &headers, meta.size, body);
                Ok(overridden(whole, set))
            }
            Window::Part(range) => {
                let (start, len) = (range.offset(), range.size());
                let (last, size) = (start + len - 1, meta.size);
                headers.push(("content-range", format!("bytes {start}-{last}/{size}")));
                let part = read_answer(StatusCode::PARTIAL_CONTENT, &headers, len, body);
                Ok(overridden(part, set))
            }
            Window::Unsatisfiable => Err(S3Error::invalid_range(meta.size)),
        }
    }

    async fn head_object(
        &self,
        parts: &Parts,
        query: &Query,
        key: &str,
    ) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let set = overrides(query)?;
        let meta = self.backend.head(key).await?;
        if let Some(early) = conditional(parts, key, &meta)? {
            return Ok(early);
        }
        let mut headers = object_headers(&meta);
        headers.push(("content-length", meta.size.to_string()));
        Ok(overridden(
            answer(StatusCode::OK, &headers, Bytes::new()),
            set,
        ))
    }

    /// A put that is not a plain create (see [`creates_only`]): `If-Match`, which is refused
    /// because an object cannot be replaced, or an `If-None-Match` other than `*`.
    async fn put_conditional(&self, parts: &Parts, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        match header(&parts.headers, "if-match") {
            Some(tag) => self.replace_object(key, tag).await,
            None => Err(S3Error::unsupported("only If-None-Match: * is supported")),
        }
    }

    /// Lets a streamed body in, before any byte of it is read: refuses a body of unknown length
    /// where the backend needs the length (411), one that says it is longer than the limit
    /// (413, so that nothing is stored and no upload is begun), and one that comes when the
    /// server is taking as many as it may and none ends within
    /// [`put_permit_wait`](super::ServerConfig::put_permit_wait) (503 `SlowDown`, with
    /// `Retry-After`). The checks of the head come first, so a request that is refused for them
    /// does not wait; the wait is on the permit alone and no byte of the body is read during it.
    /// Gives the permit that is held until the body is stored or refused.
    async fn admit(
        &self,
        len: Option<u64>,
        presign: Option<&Grant>,
    ) -> Result<OwnedSemaphorePermit, S3Error> {
        let limit = self.config.max_request_bytes;
        let cap = presign.map_or(u64::MAX, |grant| grant.max_object_bytes);
        let limit = limit.min(usize::try_from(cap).unwrap_or(usize::MAX));
        match len {
            None if self.backend.needs_length() => return Err(S3Error::length_required()),
            Some(declared) if declared > limit as u64 => return Err(S3Error::too_large(limit)),
            _ => {}
        }
        if let Ok(permit) = Arc::clone(&self.puts).try_acquire_owned() {
            return Ok(permit);
        }
        let wait = self.config.put_permit_wait;
        if wait.is_zero() {
            return Err(S3Error::busy());
        }
        match tokio::time::timeout(wait, Arc::clone(&self.puts).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(S3Error::busy()),
        }
    }

    /// Stores the object from the body of the request as it arrives: create-only, or for a
    /// plain put of the full key (`Writes::Full`) replacing a key that exists where the backend
    /// replaces. The body is not held: the pieces are checked against the hashes of the request as
    /// they pass, and go on through a short queue to the backend's streamed put, which makes the
    /// object visible only after the last piece. A body that is refused (a hash that does not match, a body
    /// over the limit, a stalled or cut one) ends the stream with an error, so the backend
    /// stores nothing, and `refused` has the answer to send. `len` is the `Content-Length` of
    /// the request, if it has one.
    ///
    /// The backend's call runs on a task of its own, so that a client that goes away after
    /// the whole body was handed over leaves the put to be carried to its end, with its
    /// entry; a client that goes away before that ends the stream with an error, and
    /// nothing is stored.
    async fn put_streamed<B>(
        &self,
        key: &str,
        len: Option<u64>,
        reader: Reader<B>,
        check: Check,
        writes: Writes<'_>,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        refuse_reserved(key)?;
        refuse_unstorable(key)?;
        let permit = self.admit(len, writes.grant()).await?;
        let hot_only = matches!(writes, Writes::Presigned(_));
        let claim = matches!(writes, Writes::Full).then(|| check.claim());
        let refused = Refused::default();
        let (carry, body) = hand_over(
            pieces(reader, check, refused.clone()),
            self.config.put_pieces_queued,
        );
        let (backend, name) = (Arc::clone(&self.backend), key.to_owned());
        let mut storing = tokio::spawn(async move {
            let _permit = permit;
            match claim {
                Some(claim) => backend.put_stream_replacing(&name, len, body, &claim).await,
                None if hot_only => backend.put_stream_hot_only(&name, len, body).await,
                None => backend.put_stream(&name, len, body).await,
            }
        });
        let early = tokio::select! {
            biased;
            done = &mut storing => Some(done),
            () = carry => None,
        };
        let stored = match early {
            Some(done) => done,
            None => storing.await,
        };
        let stored = joined(stored);
        if let Some(error) = refused.take() {
            return Err(error);
        }
        stored.map_err(S3Error::from)?;
        Ok(answer(StatusCode::OK, &[], Bytes::new()))
    }

    /// `If-Match`: an object cannot be replaced, so the answer is that the condition does not
    /// hold, or that the replacement is not done.
    async fn replace_object(&self, key: &str, tag: &str) -> Result<Answer, S3Error> {
        let meta = self.backend.head(key).await?;
        if meta.tag != tag.trim_matches('"') {
            return Err(S3Error::from(Error::PreconditionFailed(key.to_owned())));
        }
        Err(S3Error::unsupported(
            "objects are immutable and cannot be replaced",
        ))
    }

    async fn delete_object(&self, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let (backend, key) = (Arc::clone(&self.backend), key.to_owned());
        let deleted = detached(async move { backend.delete(&key).await }).await;
        deleted.map_err(S3Error::from)?;
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }

    /// ListObjectsV2 (`list-type=2`), or ListObjects, version 1, for a request with any other
    /// `list-type` or none (which is how S3 tells them apart): a page of the objects under
    /// `prefix`, after the token or `start-after` (version 2) or after `marker` (version 1).
    /// With a `delimiter` the keys that have it after the prefix are folded into common
    /// prefixes, and both kinds of entry count against `max-keys` ([`listing::page`]).
    async fn list_objects(&self, query: &Query) -> Result<Answer, S3Error> {
        let version2 = query.get("list-type") == Some("2");
        let prefix = query.get("prefix").unwrap_or_default();
        let delimiter = query.get("delimiter").filter(|text| !text.is_empty());
        let url = encoded(query)?;
        let max_keys = page_size(query, "max-keys", "maxKeys")?;
        // Without a delimiter every key is an entry: one read of the page and one key more
        // tells whether the listing goes on. With one, a prefix may hold any number of keys, and
        // they are read a page at a time.
        let batch = if delimiter.is_some() {
            PAGE_LIMIT + 1
        } else {
            max_keys + 1
        };
        let backend = self.backend.as_ref();
        let marker = query.get("marker").unwrap_or_default();
        let start = if version2 {
            list_token::start(query)?
        } else {
            (!marker.is_empty()).then(|| Start::After(marker.to_owned()))
        };
        let page = listing::page(backend, prefix, delimiter, start, max_keys, batch).await?;
        if !version2 {
            // The marker of the next page is the last key of this one, or the common prefix
            // that ends it (a marker inside a prefix passes the whole prefix).
            let next = page.next.as_ref().map(|start| match start {
                Start::After(text) | Start::PastPrefix(text) => text.as_str(),
            });
            let listing = ListingV1 {
                bucket: &self.config.bucket,
                prefix,
                delimiter,
                url,
                marker,
                max_keys,
                objects: &page.objects,
                prefixes: &page.prefixes,
                next,
            };
            return Ok(xml_answer(StatusCode::OK, listing.to_xml()));
        }
        let listing = Listing {
            bucket: &self.config.bucket,
            prefix,
            delimiter,
            url,
            start_after: query.get("start-after"),
            continuation_token: query.get("continuation-token"),
            max_keys,
            owner: query.get("fetch-owner") == Some("true"),
            objects: &page.objects,
            prefixes: &page.prefixes,
            next: page.next.as_ref().map(list_token::token),
        };
        Ok(xml_answer(StatusCode::OK, listing.to_xml()))
    }

    /// ListObjectVersions (`?versions`), for a bucket that is not versioned: the same walk as a
    /// listing of objects, starting after `key-marker`, and each object is its one version,
    /// `null`. The marker of the next page is the last key of the page and the id `null`, or,
    /// after a common prefix, the prefix and no id (as for the uploads). A
    /// `version-id-marker` needs a `key-marker`, and can only name `null`.
    async fn list_versions(&self, query: &Query) -> Result<Answer, S3Error> {
        let prefix = query.get("prefix").unwrap_or_default();
        let delimiter = query.get("delimiter").filter(|text| !text.is_empty());
        let url = encoded(query)?;
        let max_keys = page_size(query, "max-keys", "maxKeys")?;
        let key_marker = query.get("key-marker").unwrap_or_default();
        let id_marker = query.get("version-id-marker").unwrap_or_default();
        if !id_marker.is_empty() && key_marker.is_empty() {
            return Err(S3Error::invalid(
                "A version-id marker cannot be specified without a key marker.",
            ));
        }
        if !id_marker.is_empty() && id_marker != NULL_VERSION {
            return Err(S3Error::invalid("Invalid version id specified"));
        }
        let batch = if delimiter.is_some() {
            PAGE_LIMIT + 1
        } else {
            max_keys + 1
        };
        let start = (!key_marker.is_empty()).then(|| Start::After(key_marker.to_owned()));
        let backend = self.backend.as_ref();
        let page = listing::page(backend, prefix, delimiter, start, max_keys, batch).await?;
        let next = page.next.as_ref().map(|start| match start {
            Start::After(key) => (key.as_str(), NULL_VERSION),
            Start::PastPrefix(common) => (common.as_str(), ""),
        });
        let listing = VersionListing {
            bucket: &self.config.bucket,
            prefix,
            delimiter,
            url,
            key_marker,
            id_marker,
            max_keys,
            objects: &page.objects,
            prefixes: &page.prefixes,
            next,
        };
        Ok(xml_answer(StatusCode::OK, listing.to_xml()))
    }

    /// ListMultipartUploads: a page of the open uploads under `prefix`, after `key-marker`
    /// (and `upload-id-marker`, which S3 ignores without a key marker), at most `max-uploads`.
    /// The uploads of a key under the proxy's reserved prefixes are never listed.
    async fn list_uploads(&self, query: &Query) -> Result<Answer, S3Error> {
        let prefix = query.get("prefix").unwrap_or_default();
        let delimiter = query.get("delimiter").filter(|text| !text.is_empty());
        let url = encoded(query)?;
        let max_uploads = page_size(query, "max-uploads", "max-uploads")?;
        let key_marker = query.get("key-marker").unwrap_or_default();
        let id_marker = query.get("upload-id-marker").filter(|id| !id.is_empty());
        let start =
            (!key_marker.is_empty()).then(|| (key_marker.to_owned(), id_marker.map(str::to_owned)));
        // As for objects: without a delimiter every upload is an entry and one read is enough.
        let batch = if delimiter.is_some() {
            PAGE_LIMIT + 1
        } else {
            max_uploads + 1
        };
        let backend = self.backend.as_ref();
        let page = listing::uploads_page(backend, prefix, delimiter, start, max_uploads, batch);
        let page = page.await?;
        // After an upload the next page starts after its id; after a common prefix, after the
        // prefix and so after all that is under it, and there is no id.
        let next = page.next.as_ref().map(|next| match next {
            UploadsNext::After(key, id) => (key.as_str(), id.as_str()),
            UploadsNext::PastPrefix(common) => (common.as_str(), ""),
        });
        let listing = UploadListing {
            bucket: &self.config.bucket,
            prefix,
            delimiter,
            url,
            key_marker,
            id_marker: id_marker.unwrap_or_default(),
            max_uploads,
            uploads: &page.uploads,
            prefixes: &page.prefixes,
            next,
        };
        Ok(xml_answer(StatusCode::OK, listing.to_xml()))
    }

    /// ListParts (`GET /<key>?uploadId=`): a page of the parts stored in the upload, in ascending
    /// order of number, after `part-number-marker` (0 if there is none), at most `max-parts`.
    /// The backend is asked for one part more than the page, which tells whether the listing goes
    /// on; a page of no parts is not truncated. An upload the backend does not hold is
    /// `NoSuchUpload`, also for a page of no parts.
    async fn list_parts(&self, query: &Query, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId").unwrap_or_default();
        let max_parts = page_size(query, "max-parts", "max-parts")?;
        let marker = match query.get("part-number-marker") {
            None => 0,
            Some(text) => text
                .parse::<i32>()
                .ok()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    S3Error::invalid(
                        "Argument part-number-marker must be an integer between 0 and 2147483647",
                    )
                })?,
        };
        let after = (marker > 0).then_some(marker);
        let listing = self.backend.list_parts(key, id, after, max_parts + 1);
        let mut found = listing.await.map_err(upload_error)?;
        let more = max_parts > 0 && found.len() > max_parts;
        found.truncate(max_parts);
        let next = more.then(|| found.last().map(|part| part.number)).flatten();
        let listing = PartListing {
            bucket: &self.config.bucket,
            key,
            upload_id: id,
            marker,
            max_parts,
            parts: &found,
            next,
        };
        Ok(xml_answer(StatusCode::OK, listing.to_xml()))
    }

    async fn start_upload(&self, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        refuse_unstorable(key)?;
        let created = self.backend.create_upload(key).await;
        let id = created.map_err(upload_error)?;
        let body = xml::initiate(&self.config.bucket, key, &id);
        Ok(xml_answer(StatusCode::OK, body))
    }

    /// Stores a part from the body of the request as it arrives: the backend reads the pieces
    /// of the body from a stream, and the body is checked against its hashes as the pieces pass,
    /// so a part of any size is stored without being held. A part that is refused (a hash that
    /// does not match, a body that is too large or stalls) is not stored. `len` is the
    /// `Content-Length` of the request, if it has one, which the backend is told: a store that
    /// must know the length of a part before it is sent is written to as the part arrives.
    async fn upload_part<B>(
        &self,
        query: &Query,
        key: &str,
        len: Option<u64>,
        reader: Reader<B>,
        check: Check,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        refuse_reserved(key)?;
        let id = query.get("uploadId");
        let parse = |text: &str| text.parse::<u32>().ok();
        let number = query.get("partNumber").and_then(parse);
        let (Some(id), Some(number)) = (id, number) else {
            return Err(S3Error::invalid(
                "an upload id and a part number are needed",
            ));
        };
        if !(1..=PART_LIMIT).contains(&number) {
            return Err(S3Error::invalid("a part number is from 1 to 10000"));
        }
        let _permit = self.admit(len, None).await?;
        let refused = Refused::default();
        let body = pieces(reader, check, refused.clone());
        let backend = &self.backend;
        let storing = backend.upload_part_with_len(key, id, number, len, body);
        let stored = storing.await;
        if let Some(error) = refused.take() {
            return Err(error);
        }
        let etag = stored.map_err(upload_error)?;
        let headers = [("etag", format!("\"{etag}\""))];
        Ok(answer(StatusCode::OK, &headers, Bytes::new()))
    }

    async fn complete_upload(
        &self,
        query: &Query,
        key: &str,
        body: Bytes,
        replaces: bool,
    ) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId").unwrap_or_default();
        let wanted = xml::parse_complete(&body).filter(|parts| !parts.is_empty());
        let Some(wanted) = wanted else {
            let text = "The request body is not a list of parts.";
            return Err(S3Error::new(StatusCode::BAD_REQUEST, "MalformedXML", text));
        };
        let ascending = wanted.windows(2).all(|pair| pair[0].0 < pair[1].0);
        if !ascending {
            let text = "The parts must be listed in ascending order of their numbers.";
            return Err(S3Error::new(
                StatusCode::BAD_REQUEST,
                "InvalidPartOrder",
                text,
            ));
        }
        let (backend, name, upload) = (Arc::clone(&self.backend), key.to_owned(), id.to_owned());
        let call = async move {
            if replaces {
                backend
                    .complete_upload_replacing(&name, &upload, &wanted)
                    .await
            } else {
                backend.complete_upload(&name, &upload, &wanted).await
            }
        };
        let etag = detached(call).await.map_err(upload_error)?;
        let etag = format!("\"{etag}\"");
        let body = xml::completed(&self.config.bucket, key, &etag);
        Ok(xml_answer(StatusCode::OK, body))
    }

    async fn abort_upload(&self, query: &Query, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId").unwrap_or_default();
        let aborted = self.backend.abort_upload(key, id).await;
        aborted.map_err(upload_error)?;
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }
}
