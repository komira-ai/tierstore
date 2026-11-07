//! The calls of [`S3Store`] that never hold an object whole: a read that gives the answer's body
//! as the pieces it arrives in ([`S3Store::get_stream`]), and a conditional replacement that sends
//! the new body as it is given ([`S3Store::put_stream_if_match`], and [`S3Store::put_stream_if_match_unsized`]
//! for a body whose length is not known).
//!
//! A read is one GET, whole or ranged. The answer's headers must arrive within `request_timeout`,
//! and then each piece of the body within `request_timeout` of the one before, so a body of any
//! size may take as long as it needs while it keeps arriving, as a ranged read does. The request
//! is sent again after a failure in transport before the response headers arrived; once valid
//! headers have arrived the read is given, and a failure after that, even before the first body
//! byte, is the end of the body, an error, and the caller starts the read again. The entity tag
//! is that of the answer's own headers, so the bytes and the tag belong to one object.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reqwest::{Response, StatusCode};

use super::tagged::{PutAnswer, header, retrying};
use super::{Conn, S3Store, SINGLE_PUT_MAX, failure, multipart, times, transport};
use crate::body::Unsized;
use crate::{BodyStream, ByteRange, ObjectKey, ObjectStoreError};

/// The pieces of the body of a read, in order. A piece that is an error ends the body: the
/// bytes before it were not all that was asked for.
pub type ObjectBody = BoxStream<'static, Result<Bytes, ObjectStoreError>>;

/// An object, or a range of it, being read: the tag of the answer and the body as it arrives.
pub struct ObjectRead {
    /// The entity tag of the answer, as the store sends it (quotes included), if it sends one:
    /// the tag of the object the body is read from.
    pub etag: Option<String>,
    /// The size of the whole object, not of the bytes read: the `Content-Length` of an answer to
    /// a read of the whole object, the total of the `Content-Range` of an answer to a ranged one.
    /// `None` if a whole read's answer carries no `Content-Length`.
    pub size: Option<u64>,
    /// When the object was last written, in milliseconds since the Unix epoch, from the answer's
    /// `Last-Modified`; `None` if the answer carries none or one that is not a date.
    pub modified_millis: Option<u64>,
    /// The bytes, in pieces. It ends without an error only when the whole body came: the HTTP
    /// client fails a body that is shorter than the `Content-Length` of the answer.
    pub body: ObjectBody,
}

/// The pieces of the body of `resp`, until it ends or fails. The HTTP client enforces the
/// `Content-Length` the answer states, so a body cut short is a transport error and not an end.
fn pieces(conn: Arc<Conn>, resp: Response) -> ObjectBody {
    let step = |state: Option<(Arc<Conn>, Response)>| async move {
        let (conn, mut resp) = state?;
        let next = match conn.within(resp.chunk()).await {
            Ok(Ok(next)) => next,
            Ok(Err(error)) => return Some((Err(transport(error)), None)),
            Err(stalled) => return Some((Err(stalled), None)),
        };
        #[cfg(test)]
        super::read_limit_tests::took_body(next.as_ref().map_or(0, Bytes::len));
        next.map(|piece| (Ok(piece), Some((conn, resp))))
    };
    stream::unfold(Some((conn, resp)), step).boxed()
}

impl S3Store {
    /// Reads the object, or `range` of it (cut at the end of the object), as the pieces of the
    /// answer's body.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    /// [`ObjectStoreError::InvalidRange`] if the store answers 416, as it does to a range that
    /// starts at or past the end. [`ObjectStoreError::Protocol`] if a ranged read is answered with
    /// the whole object or with a `Content-Range` other than the one asked for. Errors of the body
    /// come as the last piece of [`ObjectRead::body`].
    pub async fn get_stream(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<ObjectRead, ObjectStoreError> {
        retrying(&self.conn.tuning, move || self.open_read(key, range)).await
    }

    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<ObjectRead, ObjectStoreError> {
        let headers = range
            .map(|asked| format!("bytes={}-{}", asked.offset(), asked.last()))
            .map(|value| vec![("range", value)])
            .unwrap_or_default();
        let resp = self.conn.send_read(key, headers).await?;
        let size = match (resp.status(), range) {
            (StatusCode::OK, None) => resp.content_length(),
            (StatusCode::PARTIAL_CONTENT, Some(asked)) => {
                let length = resp.content_length();
                let length = length.ok_or_else(|| bad(key, "a 206 without Content-Length"))?;
                let said = header(&resp, "content-range");
                let held = usize::try_from(length).unwrap_or(usize::MAX);
                let total = check_partial(said, asked, held).map_err(|why| bad(key, &why))?;
                Some(total)
            }
            (StatusCode::OK, Some(_)) => {
                let why = "answered 200 to a ranged read: the store ignored Range";
                return Err(bad(key, why));
            }
            (StatusCode::NOT_FOUND, _) => return Err(ObjectStoreError::NotFound(key.clone())),
            (StatusCode::RANGE_NOT_SATISFIABLE, Some(asked)) => {
                return Err(ObjectStoreError::InvalidRange {
                    key: key.clone(),
                    range: asked,
                });
            }
            (StatusCode::PARTIAL_CONTENT, None) => {
                return Err(bad(key, "answered 206 to a read of the whole object"));
            }
            _ => return Err(self.conn.within(failure(resp)).await?),
        };
        let etag = header(&resp, "etag").map(str::to_owned);
        let modified_millis = header(&resp, "last-modified").and_then(times::http_date_millis);
        let body = pieces(Arc::clone(&self.conn), resp);
        Ok(ObjectRead {
            etag,
            size,
            modified_millis,
            body,
        })
    }

    /// Replaces the object only if its entity tag is `etag` (`If-Match`, the tag sent as it is),
    /// with `len` bytes taken from `body` as they are sent, and gives the tag of the new object as
    /// the store sends it.
    ///
    /// A body of at most [`S3Tuning::single_put_max`](super::S3Tuning::single_put_max) bytes is
    /// streamed as one PUT, as [`put_stream`](crate::ObjectStore::put_stream) does; a longer one is a multipart upload
    /// whose completion carries `If-Match`, so the object is replaced only in the last request,
    /// after the whole body was read, and the upload is aborted on any failure. A body that fails
    /// or has another length than `len` replaces nothing. The write is sent once.
    ///
    /// # Errors
    /// As [`put_if_match`](Self::put_if_match), and the body's error, or
    /// [`ObjectStoreError::Body`] for a body of another length than `len`.
    pub async fn put_stream_if_match(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
        etag: &str,
    ) -> Result<String, ObjectStoreError> {
        let done = if self.sends_whole(len) {
            let condition = Some(("if-match", etag.to_owned()));
            self.put_whole(key, len, body, condition).await?
        } else {
            multipart::put(&self.conn, key, len, body, Some(etag)).await?
        };
        done.ok_or_else(|| bad(key, "the answer carries no ETag"))
    }

    /// Creates the object, unmarked, only if the key has none, with `len` bytes taken from `body`
    /// as they are sent, and gives the tag of the new object as the store sends it (the `ETag` of
    /// the answer to the write that made it, as [`put_new_tagged`](Self::put_new_tagged) does).
    ///
    /// A body of at most [`S3Tuning::single_put_max`](super::S3Tuning::single_put_max) bytes is
    /// streamed as one PUT with `If-None-Match: *`; a longer one is a multipart upload whose completion carries it, so the
    /// object appears only in the last request, after the whole body was read, and the upload is
    /// aborted on any failure. A body that fails or has another length than `len` creates
    /// nothing.
    ///
    /// # Errors
    /// As [`put_new_tagged`](Self::put_new_tagged), and the body's error, or
    /// [`ObjectStoreError::Body`] for a body of another length than `len`.
    pub async fn put_stream_new_tagged(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<String, ObjectStoreError> {
        let done = if self.sends_whole(len) {
            self.put_whole(key, len, body, self.create_only()).await?
        } else {
            multipart::put(&self.conn, key, len, body, None).await?
        };
        done.ok_or_else(|| bad(key, "the answer carries no ETag"))
    }

    /// Whether a body of `len` bytes is sent as one PUT: it is no longer than the store's
    /// `single_put_max`, which is kept to what S3 takes in one.
    pub(super) fn sends_whole(&self, len: u64) -> bool {
        len <= self.single_put_max()
    }

    /// The longest body, in bytes, that is sent as one PUT: the store's `single_put_max`, kept to
    /// what S3 takes in one.
    #[must_use]
    pub fn single_put_max(&self) -> u64 {
        self.conn.tuning.single_put_max.min(SINGLE_PUT_MAX)
    }

    /// The condition of a create-only write: `If-None-Match: *`, if the store claims conditional
    /// writes.
    pub(super) fn create_only(&self) -> Option<(&'static str, String)> {
        let conditional = self.conn.capabilities.conditional_put;
        conditional.then(|| ("if-none-match", "*".to_owned()))
    }

    /// One PUT of `len` bytes taken from `body` as they are sent, never held whole, with
    /// `condition` (`If-Match` or `If-None-Match`) if given. Gives the `ETag` of the answer, quotes
    /// included, if it has one. Sent once. The request is signed `UNSIGNED-PAYLOAD` (see
    /// `parts.rs`, whose watchdog and rules for the body it shares): a body that fails or has
    /// another length than `len` stores nothing, since the store takes a request whose body has
    /// its `Content-Length` bytes as whole.
    pub(super) async fn put_whole(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
        condition: Option<(&'static str, String)>,
    ) -> Result<Option<String>, ObjectStoreError> {
        let answer = self.put_whole_answered(key, len, body, condition).await?;
        Ok(answer.etag)
    }

    /// [`put_whole`](Self::put_whole), giving what the answer says of the object: its `ETag` and
    /// its `Last-Modified`, each if the answer has it.
    pub(super) async fn put_whole_answered(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
        condition: Option<(&'static str, String)>,
    ) -> Result<PutAnswer, ObjectStoreError> {
        let create_only = condition
            .as_ref()
            .is_some_and(|(name, _)| *name == "if-none-match");
        let headers = condition.into_iter().collect();
        let resp = self
            .conn
            .send_streamed(key, &[], headers, len, body)
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(PutAnswer {
                etag: header(&resp, "etag").map(str::to_owned),
                modified_millis: header(&resp, "last-modified").and_then(times::http_date_millis),
            }),
            StatusCode::NOT_FOUND => Err(ObjectStoreError::NotFound(key.clone())),
            StatusCode::PRECONDITION_FAILED if create_only => {
                Err(ObjectStoreError::AlreadyExists(key.clone()))
            }
            _ => Err(self.conn.within(failure(resp)).await?),
        }
    }

    /// [`put_stream_new_tagged`](Self::put_stream_new_tagged) for a caller that can do without the
    /// tag: gives what the answer to the write that made the object says (a PUT's answer, or the
    /// completion of the multipart upload, whose answer has an `ETag` and no `Last-Modified`),
    /// and an answer with no `ETag` is not an error (the object is made).
    ///
    /// # Errors
    /// As [`put_stream_new_tagged`](Self::put_stream_new_tagged), except that no `ETag` is not a
    /// [`ObjectStoreError::Protocol`].
    pub async fn put_stream_new_answered(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<PutAnswer, ObjectStoreError> {
        if !self.conn.capabilities.conditional_put {
            return Err(ObjectStoreError::Unsupported(
                "a create-only write on a store without conditional puts",
            ));
        }
        if self.sends_whole(len) {
            return self
                .put_whole_answered(key, len, body, self.create_only())
                .await;
        }
        let etag = multipart::put(&self.conn, key, len, body, None).await?;
        Ok(PutAnswer {
            etag,
            modified_millis: None,
        })
    }

    /// [`put_stream_if_match`](Self::put_stream_if_match) for a body whose length is not known,
    /// which is never held whole: the first [`PART_BYTES`](super::PART_BYTES) of it are read, and if the body ends
    /// there it is one PUT with `If-Match`; if it has more, it is a multipart upload whose parts
    /// of [`PART_BYTES`](super::PART_BYTES) are each sent as soon as they are full (so the upload moves while the
    /// body is still being given), completed with `If-Match` after the whole body was read, and
    /// aborted on any failure. At most one part and one piece of the body are held. The object is
    /// replaced only by the last request; the write is sent once.
    ///
    /// # Errors
    /// As [`put_stream_if_match`](Self::put_stream_if_match), and the body's error.
    pub async fn put_stream_if_match_unsized(
        &self,
        key: &ObjectKey,
        body: BodyStream,
        etag: &str,
    ) -> Result<String, ObjectStoreError> {
        let mut rest = Unsized::new(body);
        let first = rest.next(multipart::part_len(&self.conn.tuning)).await?;
        if !rest.more().await? {
            return self.put_if_match(key, first, etag).await;
        }
        let done = multipart::put_unsized(&self.conn, key, first, rest, etag).await?;
        done.ok_or_else(|| bad(key, "the completion carries no ETag"))
    }
}

/// A protocol error of a read or write of `key`.
fn bad(key: &ObjectKey, why: &str) -> ObjectStoreError {
    ObjectStoreError::Protocol(format!("{key}: {why}"))
}

/// The protocol error of a body that is longer than the `limit` bytes asked for.
pub(super) fn too_long(limit: u64) -> ObjectStoreError {
    ObjectStoreError::Protocol(format!(
        "the body is longer than the {limit} bytes asked for"
    ))
}

/// Checks a 206 answer: `Content-Range` must name exactly the bytes asked for (cut at
/// the end of the object) and the body must be that long. Gives the size of the whole object,
/// the total of the `Content-Range`.
pub(super) fn check_partial(
    content_range: Option<&str>,
    range: ByteRange,
    body_len: usize,
) -> Result<u64, String> {
    let cr = content_range.ok_or("206 without Content-Range")?;
    let parsed = cr.strip_prefix("bytes ").and_then(|rest| {
        let (span, total) = rest.split_once('/')?;
        let (first, last) = span.split_once('-')?;
        Some((
            first.parse::<u64>().ok()?,
            last.parse::<u64>().ok()?,
            total.parse::<u64>().ok()?,
        ))
    });
    let Some((first, last, total)) = parsed else {
        return Err(format!("Content-Range {cr:?} does not parse"));
    };
    let want_last = range.last().min(total.saturating_sub(1));
    if first != range.offset() || last != want_last || last < first {
        return Err(format!(
            "asked for bytes {}-{} and got Content-Range {cr:?}",
            range.offset(),
            range.last()
        ));
    }
    if body_len as u64 != last - first + 1 {
        return Err(format!(
            "Content-Range {cr:?} but the body has {body_len} bytes"
        ));
    }
    Ok(total)
}
