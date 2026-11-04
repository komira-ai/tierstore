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

use super::tagged::{header, retrying};
use super::{Conn, MULTIPART_THRESHOLD, S3Store, failure, multipart, transport};
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
        retrying(move || self.open_read(key, range)).await
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
        match (resp.status(), range) {
            (StatusCode::OK, None) => {}
            (StatusCode::PARTIAL_CONTENT, Some(asked)) => {
                let length = resp.content_length();
                let length = length.ok_or_else(|| bad(key, "a 206 without Content-Length"))?;
                let said = header(&resp, "content-range");
                let held = usize::try_from(length).unwrap_or(usize::MAX);
                check_partial(said, asked, held).map_err(|why| bad(key, &why))?;
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
        }
        let etag = header(&resp, "etag").map(str::to_owned);
        let body = pieces(Arc::clone(&self.conn), resp);
        Ok(ObjectRead { etag, body })
    }

    /// Replaces the object only if its entity tag is `etag` (`If-Match`, the tag sent as it is),
    /// with `len` bytes taken from `body` as they are sent, and gives the tag of the new object as
    /// the store sends it.
    ///
    /// A body of at most [`MULTIPART_THRESHOLD`] bytes is gathered and sent as one PUT, as
    /// [`put_stream`](crate::ObjectStore::put_stream) does; a longer one is a multipart upload
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
        if len <= MULTIPART_THRESHOLD {
            let bytes = crate::gather(body, len).await?;
            return self.put_if_match(key, bytes, etag).await;
        }
        let done = multipart::put(&self.conn, key, len, body, Some(etag)).await?;
        done.ok_or_else(|| bad(key, "the completion carries no ETag"))
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
        let first = rest.next(multipart::PART_LEN).await?;
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
/// the end of the object) and the body must be that long.
pub(super) fn check_partial(
    content_range: Option<&str>,
    range: ByteRange,
    body_len: usize,
) -> Result<(), String> {
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
    Ok(())
}
