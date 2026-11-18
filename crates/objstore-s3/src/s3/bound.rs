//! The write of an object under a key bound to its content: the S3 store's
//! [`ObjectStore::put_stream_content_bound`](crate::ObjectStore::put_stream_content_bound).
//!
//! One HEAD (`x-amz-checksum-mode: ENABLED`) judges what the key holds. No object: a create-only
//! write that names the digest ([`S3Store::put_stream_new_sha256`]). An object of the length
//! written whose stored checksum (or, if the store keeps none for it, its recorded digest) is the
//! digest, or is not there: kept, nothing sent. Any other object is damaged and is replaced by a
//! write on its entity tag (`If-Match`) that names the digest as the create-only write does, a
//! single PUT or a multipart upload whose completion carries the condition; a 412 or a 404 there
//! means another writer changed the key after the HEAD, and nothing is written.
//!
//! The digest is on the write so that the store refuses a body that is not it. SeaweedFS 4.54
//! (`weed/s3api/s3api_object_handlers_put.go`, `putToFiler`) checks a PUT's `Content-MD5` and its
//! `x-amz-checksum-sha256` against the bytes it read, after the upload of the chunks and before
//! the filer entry is made, and refuses a difference with 400 `BadDigest`, deleting the chunks.
//! Without either header it keeps what it read: a PUT without `Content-Type` has its first read
//! taken by the MIME sniff (`mimeDetect` in `s3api_object_handlers.go`), which drops the read's
//! error, so a body cut short before its first byte reads as an empty one and is kept as a
//! 0-byte object, which a create-only write can then never replace. The repair is for that
//! object, and for any other a store kept short.

use crate::{BodyStream, ContentBoundPut, ObjectKey, ObjectStoreError};

use super::checksum::digest_headers;
use super::multipart::{self, Rule};
use super::{S3Store, StoredSha256};

impl S3Store {
    /// The write (module docs).
    pub(super) async fn put_content_bound(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
        sha256: &[u8; 32],
    ) -> Result<ContentBoundPut, ObjectStoreError> {
        if !self.conn.capabilities.conditional_put {
            return Err(ObjectStoreError::Unsupported(
                "a content-bound write on a store without conditional puts",
            ));
        }
        let (stored, etag) = match self.head_sha256_tagged(key).await {
            Ok(found) => found,
            Err(ObjectStoreError::NotFound(_)) => {
                return match self.put_stream_new_sha256(key, len, body, sha256).await {
                    Ok(_) => Ok(ContentBoundPut::Created),
                    Err(ObjectStoreError::AlreadyExists(_)) => Ok(ContentBoundPut::Existed),
                    Err(error) => Err(error),
                };
            }
            Err(error) => return Err(error),
        };
        if holds(&stored, len, sha256) {
            return Ok(ContentBoundPut::Existed);
        }
        let etag = etag.ok_or_else(|| {
            ObjectStoreError::Protocol(format!(
                "HEAD of {key}: no ETag to replace the damaged object on"
            ))
        })?;
        if self.sends_whole(len) {
            let mut headers = vec![("if-match", etag.clone())];
            headers.extend(digest_headers(sha256));
            self.put_whole_with(key, len, body, headers, false).await?;
        } else {
            let rule = Rule::Match(&etag);
            multipart::put(&self.conn, key, len, body, rule, Some(sha256)).await?;
        }
        let size = stored.size;
        tracing::warn!(%key, size, len, %etag, "replaced a damaged content-bound object");
        Ok(ContentBoundPut::Replaced { size })
    }
}

/// Whether `stored` is the object of `len` bytes whose SHA-256 is `sha256`, as far as the store
/// says: its length is `len`, and the digest it names, by its stored whole-object checksum if it
/// keeps one and by its recorded digest if not, is `sha256` or is not there.
fn holds(stored: &StoredSha256, len: u64, sha256: &[u8; 32]) -> bool {
    let named = stored.checksum.or(stored.recorded);
    stored.size == len && named.is_none_or(|digest| digest == *sha256)
}

#[cfg(test)]
mod tests {
    use super::holds;
    use crate::s3::StoredSha256;

    fn stored(size: u64, checksum: Option<[u8; 32]>, recorded: Option<[u8; 32]>) -> StoredSha256 {
        StoredSha256 {
            size,
            checksum,
            recorded,
        }
    }

    /// Asserts which objects are kept: the length written and the digest named or none; and that
    /// a stored checksum decides over the recorded digest, so another length, another checksum
    /// (whatever the metadata records) and, with no checksum, another recorded digest are each
    /// damaged.
    #[test]
    fn an_object_is_kept_only_at_its_length_and_digest() {
        let (good, other) = ([1u8; 32], [2u8; 32]);
        assert!(holds(&stored(5, None, None), 5, &good));
        assert!(holds(&stored(5, Some(good), None), 5, &good));
        assert!(holds(&stored(5, None, Some(good)), 5, &good));
        assert!(holds(&stored(5, Some(good), Some(other)), 5, &good));
        assert!(!holds(&stored(0, None, None), 5, &good));
        assert!(!holds(&stored(6, Some(good), Some(good)), 5, &good));
        assert!(!holds(&stored(5, Some(other), Some(good)), 5, &good));
        assert!(!holds(&stored(5, None, Some(other)), 5, &good));
    }
}
