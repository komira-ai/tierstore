//! CopyObject and UploadPartCopy: a copy the store makes of an object of its own bucket, with no
//! byte of it passing through the client.
//!
//! Both are a PUT with no body that names the source in `x-amz-copy-source` (`/<bucket>/<key>`,
//! the key percent-encoded as the path of a request is), with the source's conditions
//! ([`CopyConditions`]) as the store's own `x-amz-copy-source-if-*` headers, so the store tests them
//! against the object it copies, in the same step. CopyObject writes the key with no condition on
//! it, as a plain PUT does: a key that exists is replaced. UploadPartCopy writes one part of an open
//! upload from the whole source or from a range of it (`x-amz-copy-source-range`).
//!
//! The store may take a while before it answers a copy of many bytes: the request is limited, as a
//! PUT of a body is, to the request timeout and the time the least body rate gives the bytes the
//! copy moves (see `Conn::timeout_for`). S3 may answer 200 and then put an `<Error>` in the body,
//! when the copy fails after the answer started; that is a failure too, as for a completion.

use bytes::Bytes;
use reqwest::{Method, Response, StatusCode};
use roxmltree::Document;

use super::sigv4::{EMPTY_SHA256, encode_key_path};
use super::{Conn, S3Store, failed, failure, times, transport, xml};
use crate::{ByteRange, ObjectKey, ObjectStoreError};

/// The conditions a copy sets on its source, each the text of its header as it is to be sent
/// (`x-amz-copy-source-if-match` and the rest). The store copies only if every condition named
/// holds of the source; `None` names none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyConditions {
    /// `x-amz-copy-source-if-match`: the entity tags of which the source must have one.
    pub if_match: Option<String>,
    /// `x-amz-copy-source-if-none-match`: the entity tags the source must not have.
    pub if_none_match: Option<String>,
    /// `x-amz-copy-source-if-modified-since`: an HTTP date the source must be written after.
    pub if_modified_since: Option<String>,
    /// `x-amz-copy-source-if-unmodified-since`: an HTTP date the source must not be written after.
    pub if_unmodified_since: Option<String>,
}

/// What a copy names of its source: the key, its size, and its conditions.
#[derive(Clone, Copy, Debug)]
pub struct CopySource<'a> {
    /// The key of the source, in the store's bucket.
    pub key: &'a ObjectKey,
    /// The bytes the copy moves (the source's size, or the length of the range of a part), which
    /// sets how long the request may take.
    pub size: u64,
    /// The conditions on the source.
    pub conditions: &'a CopyConditions,
}

/// The answer to a copy: the entity tag of the object or the part it made, quotes included, and
/// its time of last write in milliseconds since the Unix epoch, if the answer has one that is a
/// date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyAnswer {
    /// The `ETag` of the answer, as the store sends it.
    pub etag: String,
    /// The `LastModified` of the answer, in milliseconds since the Unix epoch.
    pub modified_millis: Option<u64>,
}

impl S3Store {
    /// CopyObject: makes `key` a copy of the object `source.key` of the store's bucket, replacing
    /// an object `key` has, if the source's conditions hold.
    ///
    /// # Errors
    /// [`ObjectStoreError::Service`] if the store refuses: status 404 `NoSuchKey` for a source
    /// that does not exist, status 412 for a condition that does not hold (nothing is written),
    /// status 400 for a source over the store's limit for one copy (5 GiB on S3), also when the
    /// store answers 200 and then an `<Error>`; [`ObjectStoreError::Transport`] if the request
    /// does not complete; [`ObjectStoreError::Protocol`] if the answer is not a
    /// `CopyObjectResult` that names an `ETag`.
    pub async fn copy_object(
        &self,
        source: &CopySource<'_>,
        key: &ObjectKey,
    ) -> Result<CopyAnswer, ObjectStoreError> {
        let headers = copy_headers(&self.conn, source);
        let resp = send_copy(&self.conn, key, &[], headers, source.size).await?;
        copied(resp, "CopyObjectResult", key).await
    }

    /// UploadPartCopy: stores the part `number` of the upload `upload_id` of `key` from the bytes
    /// `range` of the object `source.key` (all of it for `None`), if the source's conditions hold.
    /// The answer's tag is the part's, as a completion names it.
    ///
    /// # Errors
    /// As [`copy_object`](Self::copy_object), and an upload the store does not hold is status 404
    /// `NoSuchUpload`; a range past the end of the source is status 400 `InvalidArgument`.
    pub async fn upload_part_copy(
        &self,
        source: &CopySource<'_>,
        key: &ObjectKey,
        upload_id: &str,
        number: u32,
        range: Option<ByteRange>,
    ) -> Result<CopyAnswer, ObjectStoreError> {
        let mut headers = copy_headers(&self.conn, source);
        if let Some(range) = range {
            let text = format!("bytes={}-{}", range.offset(), range.last());
            headers.push(("x-amz-copy-source-range", text));
        }
        let query = [
            ("partNumber", number.to_string()),
            ("uploadId", upload_id.to_owned()),
        ];
        let moved = range.map_or(source.size, ByteRange::size);
        let resp = send_copy(&self.conn, key, &query, headers, moved).await?;
        copied(resp, "CopyPartResult", key).await
    }
}

/// The headers that name the source of a copy and its conditions.
fn copy_headers(conn: &Conn, source: &CopySource<'_>) -> Vec<(&'static str, String)> {
    let path = encode_key_path(source.key.as_str());
    let mut headers = vec![("x-amz-copy-source", format!("/{}/{path}", conn.bucket))];
    let conditions = source.conditions;
    let named = [
        ("x-amz-copy-source-if-match", &conditions.if_match),
        ("x-amz-copy-source-if-none-match", &conditions.if_none_match),
        (
            "x-amz-copy-source-if-modified-since",
            &conditions.if_modified_since,
        ),
        (
            "x-amz-copy-source-if-unmodified-since",
            &conditions.if_unmodified_since,
        ),
    ];
    let set = named
        .into_iter()
        .filter_map(|(name, value)| Some((name, value.clone()?)));
    headers.extend(set);
    headers
}

/// Signs and sends the PUT of a copy, which has no body, limited as a whole to the time a body
/// of the `moved` bytes would be given (the store reads and writes them before it answers).
async fn send_copy(
    conn: &Conn,
    key: &ObjectKey,
    query: &[(&str, String)],
    headers: Vec<(&'static str, String)>,
    moved: u64,
) -> Result<Response, ObjectStoreError> {
    let timeout = conn.timeout_for(usize::try_from(moved).unwrap_or(usize::MAX));
    conn.failing_over(|keys| {
        let req = conn
            .signed_payload(
                &keys,
                Method::PUT,
                Some(key),
                query,
                headers.clone(),
                EMPTY_SHA256,
            )
            .timeout(timeout);
        async move { req.send().await.map_err(failed) }
    })
    .await
}

/// What the answer to a copy says: the store's error for any status but 200, and for a 200 the
/// document `root`, or the `<Error>` the store sent in its place.
async fn copied(
    resp: Response,
    root: &str,
    key: &ObjectKey,
) -> Result<CopyAnswer, ObjectStoreError> {
    if resp.status() != StatusCode::OK {
        return Err(failure(resp).await);
    }
    let body: Bytes = resp.bytes().await.map_err(transport)?;
    copy_answer(&body, root, key)
}

/// A 200 answer to a copy: success only if it is the document `root` (`CopyObjectResult` or
/// `CopyPartResult`) and names an `ETag`.
pub(super) fn copy_answer(
    body: &[u8],
    root: &str,
    key: &ObjectKey,
) -> Result<CopyAnswer, ObjectStoreError> {
    let text = std::str::from_utf8(body).unwrap_or_default();
    let doc = Document::parse(text).ok();
    let element = doc.as_ref().map(Document::root_element);
    let name = element.map(|element| element.tag_name().name());
    if name == Some("Error") {
        let (code, message) = xml::error_code_message(body);
        return Err(ObjectStoreError::Service {
            status: 200,
            code,
            message,
        });
    }
    let found = element.filter(|_| name == Some(root));
    let child = |wanted: &str| {
        found.and_then(|element| {
            element
                .children()
                .find(|node| node.is_element() && node.tag_name().name() == wanted)
                .and_then(|node| node.text())
                .map(str::trim)
        })
    };
    let Some(etag) = child("ETag").filter(|etag| !etag.is_empty()) else {
        return Err(ObjectStoreError::Protocol(format!(
            "copy to {key}: the answer is not a {root} that names an ETag: {:?}",
            text.chars().take(200).collect::<String>()
        )));
    };
    Ok(CopyAnswer {
        etag: etag.to_owned(),
        modified_millis: child("LastModified").and_then(times::iso8601_millis),
    })
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
