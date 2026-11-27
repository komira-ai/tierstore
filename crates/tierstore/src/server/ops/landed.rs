//! The `ETag` of the answer to a put, as S3 answers it: the tag a HEAD or a GET of the object
//! then gives, quoted (the tier's tag, see `object_headers` in `ops`), so that a client that
//! writes and then makes a condition of what it wrote (`If-Match` with the `ETag` of the put)
//! needs no HEAD of its own.
//!
//! The backend says the tag in its answer to the write where it can: an S3 store (the `ETag` of
//! its answer to the PUT, or to the completion of the multipart upload it sent the body as), the
//! hot tier of a proxy, and a cache that writes through, which passes on its upstream's answer.
//! Where it cannot (an object that a proxy landed in its cold tier, whose tag a HEAD of the hot
//! tier does not give; a tier whose answer names no tag), one HEAD of the key after the write
//! asks. A HEAD that fails leaves the answer without the header: the object is stored, and a put
//! that succeeded is not answered as one that failed, which a client would send again.

use crate::{Backend, PutAnswer};

#[cfg(test)]
#[path = "landed_tests.rs"]
mod tests;

/// The `ETag` header of the answer to a put of `key` that `backend` stored and answered with
/// `answered`; none if it gave no tag and the HEAD after it failed.
pub(super) async fn etag(
    backend: &dyn Backend,
    key: &str,
    answered: Option<PutAnswer>,
) -> Vec<(&'static str, String)> {
    let tag = match answered {
        Some(answer) => answer.tag,
        None => match backend.head(key).await {
            Ok(meta) => meta.tag,
            Err(error) => {
                tracing::warn!("tierstore: the put of {key} is answered with no ETag: {error}");
                return Vec::new();
            }
        },
    };
    vec![("etag", format!("\"{tag}\""))]
}
