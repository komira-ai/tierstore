//! The conditions of a write: how the `If-Match` and `If-None-Match` of a put and of the
//! completion of a multipart upload decide what the backend is asked, and the completion itself.
//!
//! - A put with neither, or with `If-None-Match: *` alone, is a plain or a create-only put (see
//!   [`creates_only`](super::creates_only)).
//! - A put of the full key with `If-Match` and no `If-None-Match` replaces the object only if it
//!   still has that ETag: the body streams to [`Backend::put_stream_if_match`], whose store makes
//!   the compare and the replace one step (S3 itself, for an S3 store), so of two writers that
//!   name the same ETag one replaces the object and the other gets 412. A key that holds no
//!   object is 404 `NoSuchKey`, as S3 answers. The answer carries the ETag of the new object.
//! - The completion of an upload with `If-Match` and no `If-None-Match`, by the full key, is
//!   [`Backend::complete_upload_if_match`]: 412 for another ETag, 404 `NoSuchKey` where no object
//!   has the key, 404 `NoSuchUpload` where no such upload is open.
//! - `If-Match` beside `If-None-Match`, and an `If-None-Match` other than `*`, are 501.
//!
//! A backend that does not replace on a tag answers [`Error::Unsupported`] with the reason, and
//! the client is told that reason in the 501 (the two-tier store does so: the tag of its object
//! moves when the object moves between its tiers).

use std::sync::Arc;

use bytes::Bytes;
use hyper::StatusCode;
use hyper::header::HeaderMap;

use crate::{Backend, Error};

use super::super::Server;
use super::super::reply::{Answer, S3Error, xml_answer};
use super::super::request::{Query, header};
use super::super::xml;
use super::{Writes, detached, refuse_reserved, upload_error};

/// How a put of an object is stored as its body streams, for the key `writes` says and the
/// conditions of `headers`: `None` for a put whose conditions this front does not serve (which
/// is refused, see `put_conditional`). `If-None-Match: *` makes the full key's put create-only,
/// and `If-Match` alone makes it replace on that ETag ([`Writes::Swaps`], without its quotes).
/// Any other key keeps its own way: the checks of its role refused `If-Match` before this.
pub(super) fn streamed<'a>(headers: &'a HeaderMap, writes: Writes<'a>) -> Option<Writes<'a>> {
    match (
        header(headers, "if-match"),
        header(headers, "if-none-match"),
    ) {
        (None, None) => Some(writes),
        (None, Some("*")) if matches!(writes, Writes::Full) => Some(Writes::Creates),
        (None, Some("*")) => Some(writes),
        (Some(tag), None) if matches!(writes, Writes::Full) => Some(Writes::Swaps(unquoted(tag))),
        _ => None,
    }
}

/// An ETag as a header gives it, without the quotes around it; the backends name tags so.
fn unquoted(tag: &str) -> &str {
    tag.trim().trim_matches('"')
}

/// How the completion of an upload makes its object.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Completes {
    /// Create-only: a key that exists is refused (412).
    Creates,
    /// S3's plain completion, which replaces a key that exists where the backend replaces
    /// ([`Backend::complete_upload_replacing`]).
    Replaces,
    /// Only if the object in the key has this ETag (`If-Match`), given without quotes.
    Swaps(String),
}

/// How a completion with `headers`, by the key `writes` says, makes its object. The full key's
/// completion replaces, creates with an `If-None-Match`, and replaces on the tag with `If-Match`
/// alone; any other key creates (a scoped key never replaces).
///
/// # Errors
/// 501 `NotImplemented` for `If-Match` beside `If-None-Match` from the full key.
pub(super) fn completes(headers: &HeaderMap, writes: Writes<'_>) -> Result<Completes, S3Error> {
    if !matches!(writes, Writes::Full) {
        return Ok(Completes::Creates);
    }
    match (
        header(headers, "if-match"),
        header(headers, "if-none-match"),
    ) {
        (None, None) => Ok(Completes::Replaces),
        (None, Some(_)) => Ok(Completes::Creates),
        (Some(tag), None) => Ok(Completes::Swaps(unquoted(tag).to_owned())),
        (Some(_), Some(_)) => Err(both()),
    }
}

/// The refusal of a put whose conditions [`streamed`] does not serve: `If-Match` beside
/// `If-None-Match`, or an `If-None-Match` other than `*`.
pub(super) fn refused(headers: &HeaderMap) -> S3Error {
    if header(headers, "if-match").is_some() {
        return both();
    }
    S3Error::unsupported("only If-None-Match: * is supported")
}

/// The refusal of `If-Match` and `If-None-Match` in one request.
fn both() -> S3Error {
    S3Error::unsupported("If-Match and If-None-Match are not supported together")
}

/// The answer to a write that replaces on a tag and failed: a backend that does not do it says
/// why, and the client is told ([`Error::Unsupported`] is otherwise given a fixed text, since the
/// error of a store may name a host).
pub(super) fn swap_error(error: Error) -> S3Error {
    match error {
        Error::Unsupported(why) => S3Error::unsupported(why),
        other => S3Error::from(other),
    }
}

/// The answer to a completion that replaces on a tag and failed: the backend names the key in
/// [`Error::NotFound`] when no object has it (404 `NoSuchKey`), and the upload when it is not
/// open (404 `NoSuchUpload`).
fn completion_error(error: Error, key: &str) -> S3Error {
    match error {
        Error::NotFound(named) if named == key => S3Error::from(Error::NotFound(named)),
        Error::Unsupported(why) => S3Error::unsupported(why),
        other => upload_error(other),
    }
}

impl Server {
    /// CompleteMultipartUpload: the parts named, which must be listed in ascending order of their
    /// numbers, are joined into the object, as `completes` says. The call is carried to its end on
    /// a task of its own (see [`detached`]).
    pub(super) async fn complete_upload(
        &self,
        query: &Query,
        key: &str,
        body: Bytes,
        completes: Completes,
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
        let swaps = matches!(completes, Completes::Swaps(_));
        let call = async move { complete(&*backend, &name, &upload, &wanted, completes).await };
        let etag = detached(call).await.map_err(|error| {
            if swaps {
                completion_error(error, key)
            } else {
                upload_error(error)
            }
        })?;
        let etag = format!("\"{etag}\"");
        let body = xml::completed(&self.config.bucket, key, &etag);
        Ok(xml_answer(StatusCode::OK, body))
    }
}

/// The backend's call for a completion, as `completes` says.
async fn complete(
    backend: &dyn Backend,
    key: &str,
    id: &str,
    parts: &[(u32, String)],
    completes: Completes,
) -> Result<String, Error> {
    match completes {
        Completes::Swaps(tag) => backend.complete_upload_if_match(key, id, parts, &tag).await,
        Completes::Replaces => backend.complete_upload_replacing(key, id, parts).await,
        Completes::Creates => backend.complete_upload(key, id, parts).await,
    }
}
