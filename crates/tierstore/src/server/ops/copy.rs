//! CopyObject and UploadPartCopy: a `PUT` of an object, or of a part of an upload, that carries
//! `x-amz-copy-source` and no body (`docs/design/overwrite.md`, section 5.5).
//!
//! - **The source** is `<bucket>/<key>` (a leading `/` allowed, the key percent-encoded), and may
//!   name the one version an object has, `?versionId=null`; another version is `NoSuchVersion`,
//!   another bucket `NoSuchBucket`, and a text of another form 400 `InvalidArgument`. A key the
//!   proxy keeps for itself is refused on either side, as for any call, and so is a source under
//!   the placement of a tree (501: its bytes are not an object the backend holds); a destination
//!   under one is refused as any write there is (409).
//! - **A copy of an object onto itself** is 400 `InvalidRequest`, with S3's text: tierstore keeps
//!   no metadata a copy could change. The metadata and tagging directives may be `COPY` or
//!   `REPLACE`, which change nothing (no metadata is kept), and any other value is 400
//!   `InvalidArgument`, as on S3.
//! - **The destination** is written as a put is by the key of the request: the key of the whole
//!   front replaces (a plain put), unless the request has `If-None-Match: *`, and every other key
//!   creates only (412 for a key that exists). `If-Match`, and an `If-None-Match` other than `*`,
//!   are 501, as they are on a put.
//! - **The source conditions** (`x-amz-copy-source-if-match`, `-if-none-match`,
//!   `-if-modified-since`, `-if-unmodified-since`) are S3's: an entity tag list must name the
//!   source's tag (`*` names any) or must not; without the tag condition of its pair, a time
//!   condition is tested against the source's time of last write, to the second, and a date that
//!   is not an HTTP date is ignored. One that fails is 412 `PreconditionFailed` and nothing is
//!   written. They are tested against the HEAD of the source before anything is written (so a
//!   copy that fails them reads nothing), and again where the copy is made: by the store, which is
//!   sent them, or against the meta of the one read of the source.
//! - **A copy the store makes itself.** A plain copy (the front's own key, no `If-None-Match`)
//!   and a copy into a part are first offered to the backend
//!   ([`Backend::copy_in_store`](crate::Backend::copy_in_store)): an S3 store copies within its
//!   bucket and no byte passes through tierstore; a local cache passes the copy to its upstream
//!   and forgets the key the copy writes.
//! - **Otherwise the source is read and put.** The source is read as one object, by one
//!   `open_read`, so the copy is of one version even if the source is overwritten while it is
//!   copied. The read is opened inside the landing, when the put first asks for bytes, so a put
//!   that is refused before it reads its body (a create-only put of a key that exists, through a
//!   cache that asks first) holds no read of the source. The put has the source's length, so an S3
//!   tier streams it, and holds a permit of the server's puts. A source whose read is not the size
//!   its HEAD gave (it was replaced in between) ends the body with `SlowDown`, and nothing is
//!   written; the client retries. A copy of an object runs to its end on a task of its own, as a
//!   put does.
//! - **The answer** is a `CopyObjectResult` or a `CopyPartResult` with the `ETag` of what the copy
//!   made and its time: the backend's answer where it gives one, a HEAD of the destination after a
//!   put that gives none, and the server's clock where the answer has no time.
//! - **A copy into a part** (`?partNumber=&uploadId=`) is from the whole source or from the bytes
//!   `x-amz-copy-source-range: bytes=<first>-<last>` names; a range of another form, or one that
//!   ends past the source, is 400 `InvalidArgument`. A part may be copied from the key of its own
//!   upload. A source the backend does not hold is `NoSuchKey`, and an upload it does not hold
//!   `NoSuchUpload`.

use std::sync::Arc;

use futures::{StreamExt as _, TryStreamExt as _, stream};
use hyper::StatusCode;
use hyper::header::HeaderMap;
use hyper::http::request::Parts;
use tokio::sync::OwnedSemaphorePermit;

use super::{PART_LIMIT, Writes, detached, refuse_reserved, refuse_unstorable, upload_error};
use crate::server::Server;
use crate::server::reply::{Answer, S3Error, xml_answer};
use crate::server::request::{Query, decode, header};
use crate::server::time::parse_http_date;
use crate::server::xml::{self, NULL_VERSION};
use crate::{
    Backend, ByteRange, CopyTarget, Error, Meta, PartBody, PutAnswer, PutClaim, SourceConditions,
    Span, StoreCopy,
};

/// S3's text for a copy of an object onto itself that changes nothing.
const ONTO_ITSELF: &str = "This copy request is illegal because it is trying to copy an object to \
                           itself without changing the object's metadata, storage class, website \
                           redirect location or encryption attributes.";

/// The form of `x-amz-copy-source`, for the answer to a text of another form.
const SOURCE_FORM: &str =
    "Copy Source must mention the source bucket and key: sourcebucket/sourcekey";

/// The form of `x-amz-copy-source-range`, for the answer to a text of another form.
const RANGE_FORM: &str = "The x-amz-copy-source-range value must be of the form bytes=first-last \
                          where first and last are the zero-based offsets of the first and last \
                          bytes to copy";

/// The bucket and the key `x-amz-copy-source` names, decoded (module docs).
///
/// # Errors
/// 400 `InvalidArgument` for a text that names no bucket and key, or that names a parameter
/// other than `versionId`; 404 `NoSuchVersion` for a version other than `null`; 400
/// `InvalidURI` for a text that is not percent-encoded UTF-8.
pub(in crate::server) fn copy_source(text: &str) -> Result<(String, String), S3Error> {
    let (path, version) = text.split_once('?').unwrap_or((text, ""));
    if !version.is_empty() {
        match version.strip_prefix("versionId=") {
            Some(NULL_VERSION) => {}
            Some(_) => return Err(S3Error::no_such_version()),
            None => return Err(S3Error::invalid(SOURCE_FORM)),
        }
    }
    let decoded = decode(path).ok_or_else(S3Error::invalid_uri)?;
    let rest = decoded.strip_prefix('/').unwrap_or(&decoded);
    match rest.split_once('/') {
        Some((bucket, key)) if !bucket.is_empty() && !key.is_empty() => {
            Ok((bucket.to_owned(), key.to_owned()))
        }
        _ => Err(S3Error::invalid(SOURCE_FORM)),
    }
}

/// The upload id and the part number a call on a part names.
///
/// # Errors
/// 400 `InvalidArgument` for a call that lacks either, or a number outside 1 to 10000.
pub(super) fn part_of(query: &Query) -> Result<(&str, u32), S3Error> {
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
    Ok((id, number))
}

/// The source conditions of a copy, as the request has them.
fn conditions_of(headers: &HeaderMap) -> SourceConditions {
    let named = |name: &str| header(headers, name).map(str::to_owned);
    SourceConditions {
        if_match: named("x-amz-copy-source-if-match"),
        if_none_match: named("x-amz-copy-source-if-none-match"),
        if_modified_since: named("x-amz-copy-source-if-modified-since"),
        if_unmodified_since: named("x-amz-copy-source-if-unmodified-since"),
    }
}

/// Whether the source conditions hold of the object `meta` (module docs).
fn holds(conditions: &SourceConditions, meta: &Meta) -> bool {
    let modified = meta.modified / 1000;
    let names = |list: &str| {
        let mut listed = list.split(',').map(|item| item.trim().trim_matches('"'));
        listed.any(|item| ["*", meta.tag.as_str()].contains(&item))
    };
    let date = |text: &Option<String>| {
        let millis = text.as_deref().and_then(parse_http_date);
        millis.map(|millis| millis / 1000)
    };
    let matched = match &conditions.if_match {
        Some(list) => names(list),
        None => date(&conditions.if_unmodified_since).is_none_or(|at| modified <= at),
    };
    let fresh = match &conditions.if_none_match {
        Some(list) => !names(list),
        None => date(&conditions.if_modified_since).is_none_or(|at| modified > at),
    };
    matched && fresh
}

/// Refuses a metadata or a tagging directive other than `COPY` and `REPLACE`.
fn directives(headers: &HeaderMap) -> Result<(), S3Error> {
    let named = [
        ("x-amz-metadata-directive", "Unknown metadata directive."),
        ("x-amz-tagging-directive", "Unknown tagging directive."),
    ];
    for (name, refusal) in named {
        let value = header(headers, name);
        if value.is_some_and(|value| !["COPY", "REPLACE"].contains(&value)) {
            return Err(S3Error::invalid(refusal));
        }
    }
    Ok(())
}

/// Whether a copy of an object replaces its destination (`true`) or creates it only, as the key
/// of the request and its conditions on the destination say (module docs).
///
/// # Errors
/// 501 for `If-Match`, and for an `If-None-Match` other than `*`.
fn replacing(writes: Writes<'_>, headers: &HeaderMap) -> Result<bool, S3Error> {
    let conditions = (
        header(headers, "if-match"),
        header(headers, "if-none-match"),
    );
    match (writes, conditions) {
        (_, (Some(_), _)) => Err(S3Error::unsupported(
            "a copy with If-Match is not supported",
        )),
        (Writes::Full, (None, None)) => Ok(true),
        (_, (None, None | Some("*"))) => Ok(false),
        (_, (None, Some(_))) => Err(S3Error::unsupported("only If-None-Match: * is supported")),
    }
}

/// The bytes `x-amz-copy-source-range` names, first and last, if the request has the header.
///
/// # Errors
/// 400 `InvalidArgument` for a text that is not `bytes=<first>-<last>` with `first <= last`.
fn copy_range(headers: &HeaderMap) -> Result<Option<(u64, u64)>, S3Error> {
    let Some(text) = header(headers, "x-amz-copy-source-range") else {
        return Ok(None);
    };
    let number = |text: &str| text.parse::<u64>().ok();
    let bounds = text.strip_prefix("bytes=").and_then(|spec| {
        let (first, last) = spec.split_once('-')?;
        Some((number(first)?, number(last)?))
    });
    match bounds {
        Some((first, last)) if first <= last => Ok(Some((first, last))),
        _ => Err(S3Error::invalid(RANGE_FORM)),
    }
}

/// The bytes of `source` for the put of a copy (module docs): `len` bytes of `span` of it, from
/// one read opened when the put first asks for a byte, whose object must meet `conditions`.
fn read_source(
    backend: Arc<dyn Backend>,
    source: String,
    conditions: SourceConditions,
    span: Option<Span>,
    len: u64,
) -> PartBody<'static> {
    let opening = async move {
        let opened = backend.open_read(&source, span).await?;
        if !holds(&conditions, &opened.meta) {
            return Err(Error::PreconditionFailed(source));
        }
        if opened.served() != len {
            let text = format!("{source}: the source was replaced while the copy began");
            return Err(Error::SlowDown(text));
        }
        Ok::<_, Error>(opened.body)
    };
    stream::once(opening).try_flatten().boxed()
}

/// The copy of an object, with what it owns so that it runs on a task of its own.
struct ObjectCopy {
    backend: Arc<dyn Backend>,
    source: String,
    meta: Meta,
    conditions: SourceConditions,
    key: String,
    replaces: bool,
}

impl ObjectCopy {
    /// Makes the copy: by the store when it makes it, otherwise by a read and a put, holding the
    /// permit of a put while it writes. Gives what the copy made.
    async fn run(self, _permit: OwnedSemaphorePermit) -> Result<PutAnswer, Error> {
        let (backend, size) = (&self.backend, self.meta.size);
        if self.replaces {
            let copy = StoreCopy {
                source: &self.source,
                source_size: size,
                conditions: &self.conditions,
                key: &self.key,
                target: CopyTarget::Object,
            };
            if let Some(made) = backend.copy_in_store(&copy).await? {
                return Ok(made);
            }
        }
        let (source, conditions) = (self.source.clone(), self.conditions.clone());
        let body = read_source(Arc::clone(backend), source, conditions, None, size);
        let answered = if self.replaces {
            let claim = PutClaim::default();
            let put = backend.put_stream_replacing(&self.key, Some(size), body, &claim);
            put.await?
        } else {
            backend.put_stream(&self.key, Some(size), body).await?;
            None
        };
        if let Some(answer) = answered {
            return Ok(answer);
        }
        let meta = backend.head(&self.key).await?;
        Ok(PutAnswer {
            tag: meta.tag,
            modified: Some(meta.modified),
        })
    }
}

/// What a failed copy into a part means: a source the backend does not hold is `NoSuchKey`, and
/// anything else what it is for a part.
fn part_error(error: Error, source: &str) -> S3Error {
    match error {
        Error::NotFound(name) if name == source => S3Error::from(Error::NotFound(name)),
        other => upload_error(other),
    }
}

impl Server {
    /// A `PUT` of the object `key` that carries `x-amz-copy-source` (`source`, as it came): a copy
    /// into the object, or into a part of an upload of it if the call is on a part (module docs).
    pub(super) async fn copy(
        &self,
        parts: &Parts,
        query: &Query,
        key: &str,
        source: &str,
        multipart: bool,
        writes: Writes<'_>,
    ) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let (bucket, from) = copy_source(source)?;
        if bucket != self.config.bucket {
            return Err(S3Error::no_such_bucket(&bucket));
        }
        refuse_reserved(&from)?;
        if self.placed(&from) {
            return Err(S3Error::unsupported(
                "a copy of a file of a placed tree is not supported",
            ));
        }
        let conditions = conditions_of(&parts.headers);
        if multipart {
            return self.copy_part(parts, query, key, &from, conditions).await;
        }
        refuse_unstorable(key)?;
        if from == key {
            return Err(S3Error::invalid_request(ONTO_ITSELF));
        }
        directives(&parts.headers)?;
        let replaces = replacing(writes, &parts.headers)?;
        let meta = self.backend.head(&from).await?;
        if !holds(&conditions, &meta) {
            return Err(S3Error::from(Error::PreconditionFailed(from)));
        }
        let permit = self.permit().await?;
        let copy = ObjectCopy {
            backend: Arc::clone(&self.backend),
            source: from,
            meta,
            conditions,
            key: key.to_owned(),
            replaces,
        };
        let made = detached(copy.run(permit)).await?;
        Ok(self.copied("CopyObjectResult", &made))
    }

    /// UploadPartCopy: the part the query names, of the upload of `key`, from the source `from`.
    async fn copy_part(
        &self,
        parts: &Parts,
        query: &Query,
        key: &str,
        from: &str,
        conditions: SourceConditions,
    ) -> Result<Answer, S3Error> {
        let (upload_id, number) = part_of(query)?;
        let asked = copy_range(&parts.headers)?;
        let meta = self.backend.head(from).await?;
        if !holds(&conditions, &meta) {
            return Err(S3Error::from(Error::PreconditionFailed(from.to_owned())));
        }
        let range = match asked {
            None => None,
            Some((first, last)) if last < meta.size => {
                Some(ByteRange::new(first, last - first + 1))
            }
            Some(_) => {
                return Err(S3Error::invalid(format!(
                    "Range specified is not valid for source object of size: {}",
                    meta.size
                )));
            }
        };
        let copy = StoreCopy {
            source: from,
            source_size: meta.size,
            conditions: &conditions,
            key,
            target: CopyTarget::Part {
                upload_id,
                number,
                range,
            },
        };
        let offered = self.backend.copy_in_store(&copy).await;
        if let Some(made) = offered.map_err(|error| part_error(error, from))? {
            return Ok(self.copied("CopyPartResult", &made));
        }
        let _permit = self.permit().await?;
        let len = copy.moved();
        let span = range.map(|range| Span::From {
            first: range.offset(),
            last: Some(range.offset() + range.size() - 1),
        });
        let backend = Arc::clone(&self.backend);
        let body = read_source(backend, from.to_owned(), conditions.clone(), span, len);
        let stored = self
            .backend
            .upload_part_with_len(key, upload_id, number, Some(len), body)
            .await;
        let tag = stored.map_err(|error| part_error(error, from))?;
        let made = PutAnswer {
            tag,
            modified: None,
        };
        Ok(self.copied("CopyPartResult", &made))
    }

    /// The answer to a copy: the document `root` with the tag of what the copy made and its time,
    /// or the time of the server's clock if the backend gave none.
    fn copied(&self, root: &str, made: &PutAnswer) -> Answer {
        let modified = made.modified.unwrap_or_else(|| self.config.clock.now());
        let body = xml::copied(root, &format!("\"{}\"", made.tag), modified);
        xml_answer(StatusCode::OK, body)
    }
}
