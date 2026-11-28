//! The S3 operations: each is calls to the backend and the answer S3 gives.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use hyper::body::Body;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Method, StatusCode};
use objstore_s3::key_rules;
use tokio::task::JoinError;

use crate::journal::is_reserved;
use crate::{Error, Meta, Opened, PutKind, ReadBody, Span, Window};

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

mod admit;
mod conditional;
#[cfg(test)]
mod conditional_tests;
pub(super) mod copy;
#[cfg(test)]
mod copy_cache_tests;
#[cfg(test)]
mod copy_scoped_tests;
#[cfg(test)]
mod copy_tests;
pub(super) mod placed;
#[cfg(test)]
mod placed_tests;

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

/// The most bytes of a body that is read whole and is not a DeleteObjects document: a
/// `CompleteMultipartUpload` of the 10,000 parts S3 takes, each with its ETag and every checksum,
/// is under 5 MiB, and every other such body (a `CreateBucketConfiguration`, the empty body of a
/// copy, a read or a delete) is a few hundred bytes.
pub(super) const WHOLE_BODY_LIMIT: usize = 8 << 20;

/// The most bytes the body of a call may have. The limit of the server for a body that is
/// streamed to the backend (a put of an object that is not a copy, a part of an upload); what
/// 1000 keys fill for DeleteObjects ([`DELETE_BODY_LIMIT`]); and [`WHOLE_BODY_LIMIT`] for every
/// other body, which is held whole.
pub(super) fn body_limit(limit: usize, deleting: bool, parts: &Parts, target: &Target) -> usize {
    let streamed = parts.method == Method::PUT
        && matches!(target, Target::Object(_))
        && header(&parts.headers, "x-amz-copy-source").is_none();
    match (deleting, streamed) {
        (true, _) => limit.min(DELETE_BODY_LIMIT),
        (false, true) => limit,
        (false, false) => limit.min(WHOLE_BODY_LIMIT),
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
/// know, and a copy that is not a `PUT` of an object signed with a key (a presigned URL has no
/// right to read the source; see `copy`). A request with no `Authorization` header is presigned.
fn refuse_unsupported(parts: &Parts, query: &Query, target: &Target) -> Result<(), S3Error> {
    let presigned = !parts.headers.contains_key("authorization");
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
    let copies = parts.method == Method::PUT && matches!(target, Target::Object(_)) && !presigned;
    if header(&parts.headers, "x-amz-copy-source").is_some() && !copies {
        return Err(S3Error::unsupported(
            "a copy is a PUT of an object, signed with a key",
        ));
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

/// Whether a put only creates: it has no `If-Match`, and its `If-None-Match` is absent or `*`
/// (the rule of a scoped key). Which puts stream their body, and how, is decided by
/// [`conditional::streamed`].
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
    /// The full key's `PUT` with `If-Match` alone: replaces the object only if it has this tag
    /// ([`Backend::put_stream_if_match`](crate::Backend::put_stream_if_match)).
    Swaps(&'a str),
}

impl<'a> Writes<'a> {
    /// The grant of a presigned URL, if the request is one.
    fn grant(self) -> Option<&'a Grant> {
        match self {
            Self::Presigned(grant) => Some(grant),
            Self::Creates | Self::Full | Self::Swaps(_) => None,
        }
    }
}

impl Server {
    /// Does what a request asks for, once it is signed. The body of a multipart part and that
    /// of a put whose conditions are served (see [`conditional::streamed`]) are streamed to the
    /// backend as they arrive; a put whose conditions are not served is refused unread; every
    /// other call reads its body first, whole ([`whole_body`](Self::whole_body)).
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
        let method = parts.method.as_str();
        let setting = bucket::asks_setting(method, &target, query);
        let buckets = bucket::asks_buckets(method, &target);
        if setting.is_none() && !buckets {
            refuse_unsupported(parts, query, &target)?;
        }
        self.refuse_placed_write(method, &target, query)?;
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
        if let ("PUT", Target::Object(key)) = (method, &target)
            && let Some(source) = header(&parts.headers, "x-amz-copy-source")
        {
            // A copy has no body; it is read, so that its signed hash is checked.
            self.whole_body(reader, check).await?;
            return self
                .copy(parts, query, key, source, multipart, writes)
                .await;
        }
        if let ("PUT", Target::Object(key), true) = (method, &target, multipart) {
            let len = body_len(parts, &check);
            return self.upload_part(query, key, len, reader, check).await;
        }
        if let ("PUT", Target::Object(key), false) = (method, &target, multipart)
            && let Some(writes) = conditional::streamed(&parts.headers, writes)
        {
            let len = body_len(parts, &check);
            let put = self.put_streamed(key, len, reader, check, writes).await;
            return self.counted_put(put);
        }
        if let ("PUT", Target::Object(key), false) = (method, &target, multipart) {
            // A put whose conditions are not served is refused before its body is read.
            refuse_reserved(key)?;
            return Err(conditional::refused(&parts.headers));
        }
        // The body, and the permit that bounds the bodies held whole, until the answer is made.
        let (body, _held) = self.whole_body(reader, check).await?;
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
            ("DELETE", Target::Object(key), false) => self.delete_object(&key).await,
            ("DELETE", Target::Object(key), true) => self.abort_upload(query, &key).await,
            ("POST", Target::Object(key), true) if query.has("uploads") => {
                self.start_upload(&key).await
            }
            ("POST", Target::Object(key), true) if query.has("uploadId") => {
                let completes = conditional::completes(&parts.headers, writes)?;
                self.complete_upload(query, &key, body, completes).await
            }
            _ => Err(S3Error::unsupported("this operation is not supported")),
        }
    }

    /// One call to the backend, which gives the object's meta and the body as it arrives: the
    /// headers are made from the meta and the status from the window the body holds. A
    /// condition that is not met, or a range that is not satisfiable, drops the body unread.
    /// The read holds a permit of the server's reads until the body of its answer is over or
    /// dropped (`bounds`): a GET that finds none free is answered 503 `SlowDown` unopened.
    async fn get_object(&self, parts: &Parts, query: &Query, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let permit = self.read_permit()?;
        let mut read = self.read_object(parts, query, key).await?;
        read.body_mut().hold_permit(permit);
        Ok(read)
    }

    /// [`get_object`](Self::get_object), once it holds its permit.
    async fn read_object(
        &self,
        parts: &Parts,
        query: &Query,
        key: &str,
    ) -> Result<Answer, S3Error> {
        if let Some(placed) = self.read_placed(parts, query, key, true).await? {
            return Ok(placed);
        }
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
        if let Some(placed) = self.read_placed(parts, query, key, false).await? {
            return Ok(placed);
        }
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

    /// Stores the object from the body of the request as it arrives: create-only, or for a
    /// plain put of the full key (`Writes::Full`) replacing a key that exists where the backend
    /// replaces, or for `Writes::Swaps` replacing the object only if it has the tag, which
    /// answers with the ETag of the new object. The body is not held: the pieces are checked
    /// against the hashes of the request as they pass, and go on through a short queue to the
    /// backend's streamed put, which makes the object visible only after the last piece. A body
    /// that is refused (a hash that does not match, a body over the limit, a stalled or cut one)
    /// ends the stream with an error, so the backend
    /// stores nothing, and `refused` has the answer to send. `len` is the `Content-Length` of
    /// the request, if it has one. The digests the request named go with the put, for the
    /// store to check the body against.
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
        let swap = match writes {
            Writes::Swaps(tag) => Some(tag.to_owned()),
            _ => None,
        };
        let digests = check.digests();
        let refused = Refused::default();
        let (carry, body) = hand_over(
            pieces(reader, check, refused.clone()),
            self.config.put_pieces_queued,
        );
        let (backend, name) = (Arc::clone(&self.backend), key.to_owned());
        let mut storing = tokio::spawn(async move {
            let _permit = permit;
            match (swap, claim) {
                (Some(tag), _) => backend
                    .put_stream_if_match(&name, len, body, &tag)
                    .await
                    .map(Some),
                (None, claim) => {
                    let kind = match &claim {
                        Some(claim) => PutKind::Replaces(claim),
                        None if hot_only => PutKind::CreatesHotOnly,
                        None => PutKind::Creates,
                    };
                    let put = backend.put_stream_digested(&name, len, body, kind, digests);
                    put.await.map(|_| None)
                }
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
        let tag = stored.map_err(|error| match writes {
            Writes::Swaps(_) => conditional::swap_error(error),
            _ => S3Error::from(error),
        })?;
        let headers: Vec<_> = tag
            .map(|tag| ("etag", format!("\"{tag}\"")))
            .into_iter()
            .collect();
        Ok(answer(StatusCode::OK, &headers, Bytes::new()))
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
        let backend = &self.keys();
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
        let backend = &self.keys();
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
        let (id, number) = copy::part_of(query)?;
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

    async fn abort_upload(&self, query: &Query, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId").unwrap_or_default();
        let aborted = self.backend.abort_upload(key, id).await;
        aborted.map_err(upload_error)?;
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }
}
