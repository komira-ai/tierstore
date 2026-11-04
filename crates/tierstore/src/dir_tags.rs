//! The content tag of an object of a directory tier, and the time its file was written.

use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use super::TAG_LEN;

/// The SHA-256 of `body`.
pub(super) fn sha256(body: &[u8]) -> [u8; TAG_LEN] {
    finish(Sha256::new().chain_update(body))
}

/// The digest `hasher` has computed.
pub(super) fn finish(hasher: Sha256) -> [u8; TAG_LEN] {
    let digest = hasher.finalize();
    let mut tag = [0_u8; TAG_LEN];
    tag.copy_from_slice(digest.as_ref());
    tag
}

/// When the file was last written, in milliseconds since the Unix epoch.
pub(super) fn modified_of(meta: &std::fs::Metadata) -> u64 {
    let since = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    since.map_or(0, |elapsed| elapsed.as_millis() as u64)
}
