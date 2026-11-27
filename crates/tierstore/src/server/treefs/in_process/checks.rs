//! The checks of the words of the core's requests, before the cache is asked: the keys of the
//! upstream objects come from the trees, and the keys and paths of a put from the names a job
//! gives in the put directory. A request that fails one is answered `refused` and the words of
//! its [`Refusal`].

use crate::journal::is_reserved;
use crate::server::local_cache::layout::is_digest;

/// The longest key of an S3 object, in bytes.
const MAX_KEY: usize = 1024;

/// Why a request is refused. The text follows `refused ` in the answer.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::server::treefs) enum Refusal {
    /// A digest that is not sixty-four lower-case hex digits.
    #[error("not a sha256 digest")]
    Digest,
    /// A blob of no bytes: an empty file needs no fill.
    #[error("size is zero")]
    Empty,
    /// The blob ends past what 64 bits count.
    #[error("offset and size pass 2^64")]
    Overflow,
    /// A key that is empty or longer than an S3 key.
    #[error("not a usable key")]
    Key,
    /// A key under a prefix the proxy keeps for itself.
    #[error("reserved key")]
    Reserved,
    /// A path that is not absolute.
    #[error("not an absolute path")]
    Path,
    /// A placement id that is not thirty-two lower-case hex digits.
    #[error("not a placement id")]
    Placement,
}

/// Why the object key `text` cannot be written: empty, longer than an S3 key, or under a prefix
/// the proxy keeps for itself.
fn key(text: &str) -> Result<(), Refusal> {
    if text.is_empty() || text.len() > MAX_KEY {
        return Err(Refusal::Key);
    }
    if is_reserved(text) {
        return Err(Refusal::Reserved);
    }
    Ok(())
}

/// Why a put of the file at `path` as the object `target` is refused, once its digest is read: a
/// key that is not usable ([`key`]), a path that is not absolute.
#[cfg(any(target_os = "linux", test))]
pub(in crate::server::treefs) fn check_put(
    target: &str,
    path: &std::path::Path,
) -> Result<(), Refusal> {
    key(target)?;
    if !path.is_absolute() {
        return Err(Refusal::Path);
    }
    Ok(())
}

/// Why a fill or a span of the blob `sha256` of `size` bytes at `offset` of the object `from` is
/// refused: a digest that is not one, a blob of no bytes, one that ends past 2^64, a key that is
/// not usable ([`key`]).
pub(in crate::server::treefs) fn check_blob(
    (sha256, size): (&str, u64),
    (from, offset): (&str, u64),
) -> Result<(), Refusal> {
    if !is_digest(sha256) {
        return Err(Refusal::Digest);
    }
    if size == 0 {
        return Err(Refusal::Empty);
    }
    if offset.checked_add(size).is_none() {
        return Err(Refusal::Overflow);
    }
    key(from)
}

#[cfg(test)]
#[path = "checks_tests.rs"]
mod tests;
