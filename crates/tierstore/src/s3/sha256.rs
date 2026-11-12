//! [`Backend::put_stream_sha256`](crate::Backend::put_stream_sha256) of the S3 backend: the
//! checked body sent with its digest for the store to check and to record, and the digest a HEAD
//! reads back compared when the key exists.
//!
//! What the store checks (a store with flexible checksums, as MinIO and Amazon S3 are): a single
//! PUT against `x-amz-checksum-sha256` of the whole body, and each part of a multipart upload
//! against its own; the store keeps the checksum of a single PUT (`FULL_OBJECT`) and, for an
//! object of parts, a checksum of the part checksums (`COMPOSITE`). The digest of the whole
//! object is therefore recorded as its metadata `x-amz-meta-sha256` on both, and the body is
//! checked here as it passes, so that a multipart upload, or a store that checks nothing, is
//! never completed over bytes that are not the digest.
//!
//! On a 412 the object there is the one this put meant to make when it is `len` bytes and its
//! stored whole-object checksum is the digest, or, when the store keeps no whole-object checksum
//! for it, when its recorded digest is. A stored checksum that differs decides against it,
//! whatever the metadata says.

use objstore_s3::s3::{StoredSha256, part_size};
use objstore_s3::{ObjectKey, ObjectStoreError};

use super::{S3Backend, map_error, object_key, service_code, streamed};
use crate::put_sha256::checked;
use crate::{Error, PartBody, PutOutcome, TreeDigest};

/// The codes with which a store refuses a body that is not what its checksum says: S3's, and
/// MinIO's.
const DIGEST_CODES: [&str; 2] = ["BadDigest", "XAmzContentChecksumMismatch"];

/// The put (module docs).
pub(super) async fn put(
    backend: &S3Backend,
    key: &str,
    len: u64,
    body: PartBody<'_>,
    sha256: TreeDigest,
) -> Result<PutOutcome, Error> {
    let object = object_key(key)?;
    if part_size(len).is_none() {
        return Err(Error::TooLarge(key.to_owned()));
    }
    let store = &backend.store;
    let digest = sha256.as_bytes();
    let body = checked(key, len, sha256, body);
    let sending = |len, source| store.put_stream_new_sha256(&object, len, source, digest);
    let sent = streamed(body, Some(len), backend.chunks_waiting, sending).await?;
    match sent {
        Ok(_) => Ok(PutOutcome::Created),
        Err(ObjectStoreError::AlreadyExists(_)) => compare(backend, &object, len, digest).await,
        Err(error) => Err(refused(error, key)),
    }
}

/// What a refusal of the write means: a body the store found not to be its checksum is
/// [`Error::BadDigest`], one too large for it [`Error::TooLarge`], and anything else what it is
/// for any call.
fn refused(error: ObjectStoreError, key: &str) -> Error {
    let code = service_code(&error);
    if DIGEST_CODES.contains(&code) {
        return Error::BadDigest(key.to_owned());
    }
    if code == "EntityTooLarge" {
        return Error::TooLarge(key.to_owned());
    }
    map_error(error, key)
}

/// The answer to a write refused because the key exists: [`PutOutcome::Existed`] if the object is
/// the one the write meant to make (module docs), [`Error::AlreadyExists`] if not, and
/// [`Error::Conflict`] if it was deleted before the HEAD.
async fn compare(
    backend: &S3Backend,
    object: &ObjectKey,
    len: u64,
    digest: &[u8; 32],
) -> Result<PutOutcome, Error> {
    let key = object.as_str();
    let stored = match backend.store.head_sha256(object).await {
        Ok(stored) => stored,
        Err(ObjectStoreError::NotFound(_)) => return Err(Error::Conflict(key.to_owned())),
        Err(other) => return Err(map_error(other, key)),
    };
    if same(&stored, len, digest) {
        Ok(PutOutcome::Existed)
    } else {
        Err(Error::AlreadyExists(key.to_owned()))
    }
}

/// Whether `stored` is `len` bytes whose digest is `digest`, by the store's whole-object
/// checksum if it keeps one, and by the recorded digest if not.
fn same(stored: &StoredSha256, len: u64, digest: &[u8; 32]) -> bool {
    let named = match &stored.checksum {
        Some(checksum) => Some(checksum),
        None => stored.recorded.as_ref(),
    };
    stored.size == len && named == Some(digest)
}

#[cfg(test)]
#[path = "sha256_tests.rs"]
mod tests;
