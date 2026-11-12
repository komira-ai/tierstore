//! A create-only write that names the SHA-256 of its body, and the HEAD that reads the digest
//! of an object back.
//!
//! The write sends the digest twice. As `x-amz-checksum-sha256` (base64), which a store that does
//! flexible checksums checks against the bytes it took, refusing the request (400 `BadDigest`, or
//! MinIO's `XAmzContentChecksumMismatch`) and storing nothing if they differ; for an object sent
//! as a multipart upload the checksum is on every part instead (see `multipart.rs`). And as the
//! object's metadata `x-amz-meta-sha256` (hex), which the store keeps as it is given: the
//! checksum a store keeps for an object made of parts is a checksum of the part checksums
//! (`COMPOSITE`), not the digest of the object, and a store that does not do flexible checksums
//! keeps none.
//!
//! [`S3Store::head_sha256`] asks for the stored checksum (`x-amz-checksum-mode: ENABLED`) and
//! gives both, so a caller whose create-only write was refused because the key exists can tell
//! whether the object there has the bytes it meant to write.

use bytes::Bytes;
use reqwest::{Method, StatusCode};

use super::multipart::{self, Rule, SHA256_META, base64_of};
use super::tagged::{PutAnswer, header, retrying};
use super::{S3Store, failure};
use crate::{BodyStream, ObjectKey, ObjectStoreError};

/// What a HEAD says of the SHA-256 of an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSha256 {
    /// The object's size in bytes.
    pub size: u64,
    /// The SHA-256 the store keeps for the whole object (`x-amz-checksum-sha256` of type
    /// `FULL_OBJECT`, or of no type stated); none for an object the store keeps no such checksum
    /// for: one written without a checksum, one whose checksum is of its parts (`COMPOSITE`), or
    /// any object of a store that does not do flexible checksums.
    pub checksum: Option<[u8; 32]>,
    /// The SHA-256 the object's metadata `x-amz-meta-sha256` records; none if it records none
    /// that is 64 hex digits.
    pub recorded: Option<[u8; 32]>,
}

/// The 32 bytes `text` is the base64 of, if it is.
fn from_base64(text: &str) -> Option<[u8; 32]> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    bytes.try_into().ok()
}

/// The 32 bytes `text` is the hex of, if it is.
fn from_hex(text: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(text, &mut out).ok()?;
    Some(out)
}

impl S3Store {
    /// Creates the object only if the key has none (`If-None-Match: *`), with `len` bytes taken
    /// from `body` as they are sent, which are to hash to `sha256` (see the module docs). Gives
    /// what the answer says of the object, as
    /// [`put_stream_new_answered`](Self::put_stream_new_answered) does.
    ///
    /// A body of at most [`S3Tuning::single_put_max`](super::S3Tuning::single_put_max) bytes is
    /// one PUT carrying `x-amz-checksum-sha256`; a longer one is a multipart upload whose parts
    /// carry theirs and whose completion carries `If-None-Match: *`. Both record `sha256` as the
    /// metadata `x-amz-meta-sha256`. This call does not hash the body as a whole: a store that
    /// checks no checksum, and a multipart upload, take a body that does not hash to `sha256` as
    /// long as each part is what was sent, so a caller that must not publish such a body checks
    /// it as it passes and ends `body` with an error, which stores nothing.
    ///
    /// # Errors
    /// [`ObjectStoreError::Unsupported`] unless the store claims
    /// [`Capabilities::conditional_put`](crate::Capabilities::conditional_put).
    /// [`ObjectStoreError::AlreadyExists`] if the key has an object.
    /// [`ObjectStoreError::Service`] if the store refuses the write, status 400 with the code
    /// `BadDigest` or `XAmzContentChecksumMismatch` among others for bytes that are not what
    /// their checksum says. The body's error, or [`ObjectStoreError::Body`] for a body of another
    /// length than `len`.
    pub async fn put_stream_new_sha256(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
        sha256: &[u8; 32],
    ) -> Result<PutAnswer, ObjectStoreError> {
        if !self.conn.capabilities.conditional_put {
            return Err(ObjectStoreError::Unsupported(
                "a create-only write on a store without conditional puts",
            ));
        }
        if self.sends_whole(len) {
            let headers = vec![
                ("if-none-match", "*".to_owned()),
                ("x-amz-checksum-sha256", base64_of(sha256)),
                (SHA256_META, hex::encode(sha256)),
            ];
            return self.put_whole_with(key, len, body, headers, true).await;
        }
        let rule = Rule::Create;
        let etag = multipart::put(&self.conn, key, len, body, rule, Some(sha256)).await?;
        Ok(PutAnswer {
            etag,
            modified_millis: None,
        })
    }

    /// The object's size and the SHA-256 the store and its metadata keep for it (see
    /// [`StoredSha256`]): a HEAD with `x-amz-checksum-mode: ENABLED`, sent again after a failure
    /// in transport as [`head`](Self::head) is.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    /// [`ObjectStoreError::Protocol`] if the answer lacks a size.
    pub async fn head_sha256(&self, key: &ObjectKey) -> Result<StoredSha256, ObjectStoreError> {
        retrying(&self.conn.tuning, move || self.head_sha256_once(key)).await
    }

    async fn head_sha256_once(&self, key: &ObjectKey) -> Result<StoredSha256, ObjectStoreError> {
        let headers = vec![("x-amz-checksum-mode", "ENABLED".to_owned())];
        let resp = self
            .conn
            .send(Method::HEAD, Some(key), &[], headers, Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Err(ObjectStoreError::NotFound(key.clone())),
            _ => return Err(failure(resp).await),
        }
        let size = header(&resp, "content-length")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| {
                ObjectStoreError::Protocol(format!("HEAD of {key}: missing or bad Content-Length"))
            })?;
        let whole = header(&resp, "x-amz-checksum-type")
            .is_none_or(|kind| kind.eq_ignore_ascii_case("FULL_OBJECT"));
        let checksum = header(&resp, "x-amz-checksum-sha256")
            .filter(|_| whole)
            .and_then(from_base64);
        let recorded = header(&resp, SHA256_META).and_then(from_hex);
        Ok(StoredSha256 {
            size,
            checksum,
            recorded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{from_base64, from_hex};

    /// Asserts that a stored checksum and a recorded digest are read only when they are exactly
    /// 32 bytes in their encoding: a composite checksum (`<base64>-<parts>`), a short value and
    /// metadata that is not hex are not a digest.
    #[test]
    fn only_a_whole_digest_is_read() {
        let digest = [7u8; 32];
        let b64 = super::base64_of(&digest);
        assert_eq!(from_base64(&b64), Some(digest));
        assert_eq!(from_base64(&format!("{b64}-2")), None);
        assert_eq!(from_base64("AAAA"), None);
        assert_eq!(from_hex(&hex::encode(digest)), Some(digest));
        assert_eq!(from_hex(&hex::encode([7u8; 31])), None);
        assert_eq!(from_hex(&"zz".repeat(32)), None);
    }
}
