//! Why a fill of a range left nothing in the cache.

use crate::Error;

use super::super::source::status_of;

/// Why [`LocalCache::fill_range`](super::super::LocalCache::fill_range) kept nothing. The text
/// is the word the caller reports.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// The cache does not keep the pack's key (no policy of `[local_cache]` says `cache = true`
    /// for it, or it is a reserved key).
    #[error("not kept")]
    NotKept,
    /// The range is empty, ends past what a number can say, or its digest is not the
    /// lower-case hex of a SHA-256.
    #[error("empty range or bad digest")]
    Empty,
    /// The range ends past the end of the pack.
    #[error("past the end")]
    Past,
    /// The upstream failed or has no such object; the number is the HTTP status that stands for
    /// the failure.
    #[error("upstream {0}")]
    Upstream(u16),
    /// The bytes do not hash to the digest the range was asked under.
    #[error("mismatch")]
    Mismatch,
    /// The upstream gave other than the bytes the range has.
    #[error("short")]
    Short,
    /// The range does not fit the budget, or would take the cache past its critical mark.
    #[error("no space")]
    NoSpace,
    /// No permit of the pool of `fill_socket_concurrency` came free in time.
    #[error("busy")]
    Busy,
    /// This process does not hold the lock of the cache directory, or lost it during the fill.
    #[error("read-only")]
    ReadOnly,
    /// The cache directory could not be written.
    #[error("io")]
    Io,
    /// The entry was removed or replaced while the range was read (a write or a delete through
    /// the cache, or another version of the pack).
    #[error("changed")]
    Changed,
    /// The task of the fill panicked or was cancelled.
    #[error("aborted")]
    Aborted,
}

impl From<Error> for RangeError {
    fn from(error: Error) -> Self {
        Self::Upstream(status_of(&error))
    }
}

impl From<std::io::Error> for RangeError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}
