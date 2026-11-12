//! Multipart upload: how [`S3Store`](super::S3Store) writes an object larger than one
//! request should carry, a part at a time.
//!
//! CreateMultipartUpload, then UploadPart for each part in order (each held in memory
//! only while it is sent), then CompleteMultipartUpload naming every part's ETag. Any
//! failure before the object is complete (the body's, a part's, the completion's,
//! including a conditional write refused at completion) aborts the upload with
//! AbortMultipartUpload before the error is returned, so no incomplete upload is left
//! holding storage. A write dropped mid-upload (its caller went away) aborts it from a
//! task of its own. An abort that itself fails is logged; a bucket lifecycle rule
//! (`AbortIncompleteMultipartUpload`) is the backstop for those and for a crash.
//!
//! An upload that names the SHA-256 of the whole object ([`put`] given `sha256`) is created with
//! `x-amz-checksum-algorithm: SHA256` and the digest as the object's metadata `x-amz-meta-sha256`
//! (hex); each part carries `x-amz-checksum-sha256`, the base64 SHA-256 of the part, which the
//! store checks against the bytes it took, and the completion names each part's checksum. The
//! store keeps a checksum of the part checksums for such an object (`COMPOSITE`), which is not
//! the SHA-256 of its bytes, so the metadata is what records that.
//!
//! S3's rules, which [`part_size`] keeps: every part but the last is at least
//! [`MIN_PART_BYTES`], none is over [`MAX_PART_BYTES`], and an object has at most
//! [`MAX_PARTS`] parts.

use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use reqwest::{Method, Response, StatusCode};
use sha2::{Digest as _, Sha256};

use super::{Conn, failure, transport, xml};
use crate::body::{Parts, Unsized};
use crate::{BodyStream, ObjectKey, ObjectStoreError};

/// The smallest part S3 takes, except the last: 5 MiB.
pub const MIN_PART_BYTES: u64 = 5 << 20;

/// The largest part S3 takes: 5 GiB, the limit of one PUT.
pub const MAX_PART_BYTES: u64 = 5 << 30;

/// The most parts one object may have.
pub const MAX_PARTS: u64 = 10_000;

/// The part size of any object that fits in [`MAX_PARTS`] of them (156.25 GiB):
/// 16 MiB, what one part holds in memory while it is sent.
pub const PART_BYTES: u64 = 16 << 20;

/// The longest object written with one PUT: S3's limit, 5 GiB, which is the default of
/// [`S3Tuning::single_put_max`](super::S3Tuning::single_put_max). The body of a PUT is
/// streamed, never held whole, so the length of an object that is sent whole is no cost in
/// memory. A longer one is a multipart upload.
pub const SINGLE_PUT_MAX: u64 = MAX_PART_BYTES;

const MIB: u64 = 1 << 20;

// The default part is one S3 takes.
const _: () = assert!(PART_BYTES >= MIN_PART_BYTES && PART_BYTES <= MAX_PART_BYTES);

/// The part size of an object of `len` bytes: [`PART_BYTES`], or for an object too
/// large for [`MAX_PARTS`] parts of that size, the least whole number of MiB that fits
/// it in [`MAX_PARTS`] parts. `None` when even [`MAX_PART_BYTES`] parts do not (an
/// object over 10,000 parts of 5 GiB, about 48.8 TiB).
#[must_use]
pub fn part_size(len: u64) -> Option<u64> {
    part_size_of(len, PART_BYTES)
}

/// [`part_size`] for a store whose part size is `base` (kept between [`MIN_PART_BYTES`] and
/// [`MAX_PART_BYTES`]) rather than [`PART_BYTES`].
#[must_use]
pub fn part_size_of(len: u64, base: u64) -> Option<u64> {
    let least = len.div_ceil(MAX_PARTS).div_ceil(MIB).checked_mul(MIB)?;
    let size = least.max(base.clamp(MIN_PART_BYTES, MAX_PART_BYTES));
    (size <= MAX_PART_BYTES).then_some(size)
}

/// The size of the parts of a body of a length not known: the store's `part_bytes`, kept to the
/// sizes S3 takes.
pub(super) fn part_len(tuning: &super::S3Tuning) -> usize {
    tuning.part_bytes.clamp(MIN_PART_BYTES, MAX_PART_BYTES) as usize
}

/// What the completion of a multipart upload asks of the key.
#[derive(Clone, Copy)]
pub(super) enum Rule<'a> {
    /// A new object: `If-None-Match: *` where the store claims conditional writes.
    Create,
    /// The object, whether the key has one or not: no condition on the completion.
    Replace,
    /// The replacement of the object that has this entity tag (`If-Match`).
    Match(&'a str),
}

/// The metadata header that records the SHA-256 of an object, as 64 lower-case hex digits: sent
/// with a PUT and a CreateMultipartUpload that name the digest, and read back by a HEAD.
pub(super) const SHA256_META: &str = "x-amz-meta-sha256";

/// The base64 of `digest`, as S3 takes and gives an `x-amz-checksum-sha256`.
pub(super) fn base64_of(digest: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// Writes `body`, `len` bytes, under `key` as a multipart upload that the completion makes an
/// object by `rule`, with a SHA-256 checksum on every part and `sha256` as the object's metadata
/// when given (see the module docs). Gives the `ETag` of the object, quotes included, if the
/// completion carries one.
pub(super) async fn put(
    conn: &Arc<Conn>,
    key: &ObjectKey,
    len: u64,
    body: BodyStream,
    rule: Rule<'_>,
    sha256: Option<&[u8; 32]>,
) -> Result<Option<String>, ObjectStoreError> {
    let size = part_size_of(len, conn.tuning.part_bytes).ok_or(ObjectStoreError::Unsupported(
        "an object larger than 10,000 parts of 5 GiB",
    ))?;
    let upload = create(conn, key, sha256).await?;
    let sent = send(&upload, len, size, body, rule).await;
    settle(upload, sent).await
}

/// Writes `first` and `rest`, a body of a length not known, as a multipart upload that replaces
/// the object that has the entity tag `etag` (`If-Match` on the completion). Each part (of
/// the store's `part_bytes`, the last shorter) is sent as soon as it is full and the body has more, so
/// at most one part and one piece of the body are held, and the object is replaced only by the
/// completion, after the whole body was read. `first` is the first part, which the caller read
/// to find that the body is longer than one part.
/// Gives the `ETag` of the object, quotes included, if the completion carries one.
pub(super) async fn put_unsized(
    conn: &Arc<Conn>,
    key: &ObjectKey,
    first: Bytes,
    rest: Unsized,
    etag: &str,
) -> Result<Option<String>, ObjectStoreError> {
    let upload = create(conn, key, None).await?;
    let sent = send_unsized(&upload, first, rest, etag).await;
    settle(upload, sent).await
}

/// Ends `upload` as `sent` says: nothing more to do if it completed, an abort if it failed.
async fn settle(
    upload: Upload,
    sent: Result<Option<String>, ObjectStoreError>,
) -> Result<Option<String>, ObjectStoreError> {
    match sent {
        Ok(etag) => {
            upload.finished();
            Ok(etag)
        }
        Err(e) => {
            upload.abort().await;
            Err(e)
        }
    }
}

/// `first`, then every part of `rest` while it has bytes, then the completion. A part number
/// past [`MAX_PARTS`] is the store's to refuse, which aborts the upload.
async fn send_unsized(
    upload: &Upload,
    first: Bytes,
    mut rest: Unsized,
    etag: &str,
) -> Result<Option<String>, ObjectStoreError> {
    let len = part_len(&upload.conn.tuning);
    let mut sent = vec![upload_part(upload, 1, first).await?];
    while rest.more().await? {
        let part = rest.next(len).await?;
        sent.push(upload_part(upload, sent.len() + 1, part).await?);
    }
    complete(upload, &sent, Rule::Match(etag)).await
}

/// An upload in progress. Dropped while open, it is aborted from a task of its own.
struct Upload {
    conn: Arc<Conn>,
    key: ObjectKey,
    id: String,
    open: bool,
    /// Every part carries its SHA-256 checksum (the upload was created with the algorithm).
    checksummed: bool,
}

impl Upload {
    /// The upload completed: nothing to abort.
    fn finished(mut self) {
        self.open = false;
    }

    /// Aborts the upload; a failure is logged (the lifecycle rule is the backstop).
    /// Still open until the abort ended: a write dropped while it is on the wire (its
    /// caller went away) is aborted again by [`Drop`], from a task of its own.
    async fn abort(mut self) {
        abort(&self.conn, &self.key, &self.id).await;
        self.open = false;
    }
}

impl Drop for Upload {
    /// Aborts an upload still open from a task of its own; outside a runtime it is left
    /// for the bucket's lifecycle rule.
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let (conn, key, id) = (
            Arc::clone(&self.conn),
            self.key.clone(),
            std::mem::take(&mut self.id),
        );
        let runtime = tokio::runtime::Handle::try_current().ok();
        runtime.map(|r| r.spawn(async move { abort(&conn, &key, &id).await }));
    }
}

/// CreateMultipartUpload: the id of the new upload.
pub(super) async fn create_id(conn: &Conn, key: &ObjectKey) -> Result<String, ObjectStoreError> {
    create_id_with(conn, key, Vec::new()).await
}

/// CreateMultipartUpload with `headers`: the id of the new upload.
async fn create_id_with(
    conn: &Conn,
    key: &ObjectKey,
    headers: Vec<(&'static str, String)>,
) -> Result<String, ObjectStoreError> {
    let query = [("uploads", String::new())];
    let resp = conn
        .send(Method::POST, Some(key), &query, headers, Bytes::new())
        .await?;
    if resp.status() != StatusCode::OK {
        return Err(failure(resp).await);
    }
    let body = resp.bytes().await.map_err(transport)?;
    xml::upload_id(&body)
}

/// CreateMultipartUpload, as an upload that aborts itself if it is dropped open; one whose parts
/// carry SHA-256 checksums, and that records `sha256` as the object's metadata, if it is given.
async fn create(
    conn: &Arc<Conn>,
    key: &ObjectKey,
    sha256: Option<&[u8; 32]>,
) -> Result<Upload, ObjectStoreError> {
    let headers = match sha256 {
        Some(digest) => vec![
            ("x-amz-checksum-algorithm", "SHA256".to_owned()),
            (SHA256_META, hex::encode(digest)),
        ],
        None => Vec::new(),
    };
    let id = create_id_with(conn, key, headers).await?;
    Ok(Upload {
        conn: Arc::clone(conn),
        key: key.clone(),
        id,
        open: true,
        checksummed: sha256.is_some(),
    })
}

/// Every part, in order, then the completion.
async fn send(
    upload: &Upload,
    len: u64,
    size: u64,
    body: BodyStream,
    rule: Rule<'_>,
) -> Result<Option<String>, ObjectStoreError> {
    let mut parts = Parts::new(body, len);
    let mut sent = Vec::new();
    // `Parts` answers `None` only after the body has ended with exactly `len` bytes,
    // so an upload is never completed over a body that failed at its end.
    while let Some(part) = parts.next(size).await? {
        let number = sent.len() + 1;
        sent.push(upload_part(upload, number, part).await?);
    }
    complete(upload, &sent, rule).await
}

/// A part the store took: its ETag, and the base64 SHA-256 checksum it was sent with, if the
/// upload's parts carry one.
type SentPart = (String, Option<String>);

/// UploadPart: answers the part's ETag, and its checksum if the upload's parts carry one.
async fn upload_part(
    upload: &Upload,
    number: usize,
    part: Bytes,
) -> Result<SentPart, ObjectStoreError> {
    let query = [
        ("partNumber", number.to_string()),
        ("uploadId", upload.id.clone()),
    ];
    let checksum = upload
        .checksummed
        .then(|| base64_of(&Sha256::digest(&part)));
    let headers = checksum
        .iter()
        .map(|sum| ("x-amz-checksum-sha256", sum.clone()))
        .collect();
    let resp = upload
        .conn
        .send(Method::PUT, Some(&upload.key), &query, headers, part)
        .await?;
    if resp.status() != StatusCode::OK {
        return Err(failure(resp).await);
    }
    Ok((part_etag(&resp, &upload.key, number)?, checksum))
}

/// The ETag in the answer to the upload of part `number` of `key`.
pub(super) fn part_etag(
    resp: &Response,
    key: &ObjectKey,
    number: impl fmt::Display,
) -> Result<String, ObjectStoreError> {
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    etag.ok_or_else(|| {
        ObjectStoreError::Protocol(format!("part {number} of {key} answered without an ETag"))
    })
}

/// CompleteMultipartUpload by `rule`: create-only as `put_new` is when the store claims it,
/// replacing the object that has a tag (`If-Match`), or with no condition. S3 may answer 200 with
/// an `<Error>` body: that is a failure too.
async fn complete(
    upload: &Upload,
    parts: &[SentPart],
    rule: Rule<'_>,
) -> Result<Option<String>, ObjectStoreError> {
    let request = xml::complete_request(parts);
    let (conn, key, id) = (&upload.conn, &upload.key, &upload.id);
    match rule {
        Rule::Match(etag) => complete_replacing(conn, key, id, request, etag).await,
        Rule::Create => complete_with(conn, key, id, request).await,
        Rule::Replace => complete_plain(conn, key, id, request).await,
    }
}

/// The `ETag` (quotes included, if the answer names one) of the object a 200 answer to a
/// completion says was made, or the failure it carries.
async fn completed(resp: Response) -> Result<Option<String>, ObjectStoreError> {
    let body = resp.bytes().await.map_err(transport)?;
    xml::complete_answer(&body)
}

/// CompleteMultipartUpload of the upload `id` of `key` with this request body, create-only as
/// `complete` is. Gives the `ETag` of the object, quotes included, if the answer has one.
pub(super) async fn complete_with(
    conn: &Conn,
    key: &ObjectKey,
    id: &str,
    request: String,
) -> Result<Option<String>, ObjectStoreError> {
    let mut headers = vec![("content-type", "application/xml".to_owned())];
    if conn.capabilities.conditional_put {
        headers.push(("if-none-match", "*".to_owned()));
    }
    let query = [("uploadId", id.to_owned())];
    let body = Bytes::from(request);
    let resp = conn
        .send(Method::POST, Some(key), &query, headers, body)
        .await?;
    match resp.status() {
        StatusCode::OK => completed(resp).await,
        // Asked only of a store that claims conditional writes.
        StatusCode::PRECONDITION_FAILED => Err(ObjectStoreError::AlreadyExists(key.clone())),
        _ => Err(failure(resp).await),
    }
}

/// CompleteMultipartUpload of the upload `id` of `key` with no condition: the object is made
/// whether the key holds one or not, as a plain PUT is. Gives the `ETag` of the object, quotes
/// included, if the answer has one.
async fn complete_plain(
    conn: &Conn,
    key: &ObjectKey,
    id: &str,
    request: String,
) -> Result<Option<String>, ObjectStoreError> {
    let headers = vec![("content-type", "application/xml".to_owned())];
    let query = [("uploadId", id.to_owned())];
    let body = Bytes::from(request);
    let resp = conn
        .send(Method::POST, Some(key), &query, headers, body)
        .await?;
    match resp.status() {
        StatusCode::OK => completed(resp).await,
        _ => Err(failure(resp).await),
    }
}

/// CompleteMultipartUpload of the upload `id` of `key` that replaces the object only if it has
/// the entity tag `etag` (`If-Match`, sent as it is): another tag is a 412
/// [`ObjectStoreError::Service`], no object is [`ObjectStoreError::NotFound`] (the 404 whose code
/// is `NoSuchKey`), and the upload stays open.
pub(super) async fn complete_replacing(
    conn: &Conn,
    key: &ObjectKey,
    id: &str,
    request: String,
    etag: &str,
) -> Result<Option<String>, ObjectStoreError> {
    let headers = vec![
        ("content-type", "application/xml".to_owned()),
        ("if-match", etag.to_owned()),
    ];
    let query = [("uploadId", id.to_owned())];
    let body = Bytes::from(request);
    let resp = conn
        .send(Method::POST, Some(key), &query, headers, body)
        .await?;
    match resp.status() {
        StatusCode::OK => completed(resp).await,
        StatusCode::NOT_FOUND => match failure(resp).await {
            ObjectStoreError::Service { code, .. } if code == "NoSuchKey" => {
                Err(ObjectStoreError::NotFound(key.clone()))
            }
            other => Err(other),
        },
        _ => Err(failure(resp).await),
    }
}

/// AbortMultipartUpload; a failure is logged. An upload the store no longer knows
/// (completed after all, or aborted already) needs nothing more.
async fn abort(conn: &Conn, key: &ObjectKey, id: &str) {
    if let Err(e) = try_abort(conn, key, id).await {
        tracing::warn!(%key, error = %e, "a multipart upload could not be aborted; the bucket's lifecycle rule must remove it");
    }
}

async fn try_abort(conn: &Conn, key: &ObjectKey, id: &str) -> Result<(), ObjectStoreError> {
    let query = [("uploadId", id.to_owned())];
    let resp = conn
        .send(Method::DELETE, Some(key), &query, Vec::new(), Bytes::new())
        .await?;
    match resp.status() {
        StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
        _ => Err(failure(resp).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// Catches: a part size that breaks S3's rules at the edges: over 10,000 parts (an
    /// object one byte past what 16 MiB parts hold), a part under 5 MiB that is not the
    /// last, a part over 5 GiB, or an object that does fit refused. Every size it gives
    /// is a whole number of MiB, so parts line up with nothing smaller.
    #[test]
    fn part_sizes_keep_s3s_rules_at_the_boundaries() {
        assert_eq!(part_size(1), Some(PART_BYTES));
        assert_eq!(part_size(PART_BYTES + 1), Some(PART_BYTES));
        let most_at_16_mib = PART_BYTES * MAX_PARTS;
        assert_eq!(part_size(most_at_16_mib), Some(PART_BYTES));
        assert_eq!(part_size(most_at_16_mib + 1), Some(PART_BYTES + MIB));
        // Over 5 GiB, S3's single-PUT limit: still 16 MiB parts.
        assert_eq!(part_size(5 * GIB + 1), Some(PART_BYTES));
        let most = MAX_PART_BYTES * MAX_PARTS;
        assert_eq!(part_size(most), Some(MAX_PART_BYTES));
        assert_eq!(part_size(most + 1), None);
        assert_eq!(part_size(u64::MAX), None);
        for len in [
            PART_BYTES + 1,
            5 * GIB + 1,
            most_at_16_mib + 1,
            1 << 40,
            most - 1,
            most,
        ] {
            let size = part_size(len).expect("fits");
            assert!(len.div_ceil(size) <= MAX_PARTS, "{len}: too many parts");
            assert!((MIN_PART_BYTES..=MAX_PART_BYTES).contains(&size), "{len}");
            assert_eq!(size % MIB, 0, "{len}");
        }
    }
}
