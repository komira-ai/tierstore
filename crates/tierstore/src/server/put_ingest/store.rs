//! The put of one committed file: opened without following a link, checked to be a regular file
//! with one link, read once in chunks and hashed as it passes to the cache's create-only put that
//! names its SHA-256 ([`put_stream_sha256`](crate::Backend::put_stream_sha256)), the put the
//! fill socket makes for a commit of the put directory. A body that is not the declared digest
//! (a truncated or a changed file) ends with an error before its last byte, and the upstream
//! stores nothing of it.

use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::Errno;
use tokio::io::AsyncRead;

use crate::server::fill_socket::put::chunks;
use crate::server::local_cache::LocalCache;
use crate::{Backend as _, Error, PutOutcome, TreeDigest};

/// Why a committed file is refused: nothing was stored, and the file is removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum Refusal {
    /// No digest was declared for it (its `.sha256-<hex>.part` name was never seen).
    NoDigest,
    /// Its bytes are not those of the declared digest: a truncated, longer or changed file.
    NotItsDigest,
    /// The key holds other bytes already.
    OtherBytes,
    /// The upstream refused it for its size.
    TooLarge,
    /// A symbolic link, a hard link or not a regular file.
    NotPlain(&'static str),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDigest => f.write_str("no digest was declared for it"),
            Self::NotItsDigest => f.write_str("its bytes are not those of the declared digest"),
            Self::OtherBytes => f.write_str("the key holds other bytes"),
            Self::TooLarge => f.write_str("the upstream refused it for its size"),
            Self::NotPlain(what) => write!(f, "it is {what}"),
        }
    }
}

/// What the put of a file came to.
#[derive(Debug)]
pub(super) enum Outcome {
    /// The object is stored (made now, or there with the same bytes already).
    Stored(PutOutcome),
    /// Refused: nothing was stored.
    Refused(Refusal),
    /// The file is gone: nothing to do.
    Gone,
    /// The file cannot be opened or read now (the reason): it is left, and tried again.
    Unreadable(String),
    /// The put failed for a reason a later try may not meet (the upstream did not answer, or
    /// refused): the file is left, and tried again.
    Again(Error),
}

/// The file at `path`, opened without following a link and checked to be a regular file with
/// one link, and its size; or the outcome that ends its put.
fn open(path: &Path) -> Result<(File, u64), Outcome> {
    // `NONBLOCK` so that a FIFO put in its place does not hold the ingest; a regular file reads
    // as it would without it.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = openat(CWD, path, flags, Mode::empty()).map_err(|why| match why {
        Errno::NOENT => Outcome::Gone,
        Errno::LOOP => Outcome::Refused(Refusal::NotPlain("a symbolic link")),
        why => Outcome::Unreadable(std::io::Error::from(why).to_string()),
    })?;
    let file = File::from(fd);
    let meta = file
        .metadata()
        .map_err(|why| Outcome::Unreadable(why.to_string()))?;
    if !meta.is_file() {
        return Err(Outcome::Refused(Refusal::NotPlain("not a regular file")));
    }
    if std::os::unix::fs::MetadataExt::nlink(&meta) != 1 {
        return Err(Outcome::Refused(Refusal::NotPlain("a hard link")));
    }
    Ok((file, meta.len()))
}

/// Puts the file at `path` as the object `key`, whose bytes must hash to `sha256`.
pub(super) async fn put_file(
    cache: &LocalCache,
    key: &str,
    sha256: TreeDigest,
    path: &Path,
) -> Outcome {
    match open(path) {
        Ok((file, size)) => {
            put_body(cache, key, sha256, tokio::fs::File::from_std(file), size).await
        }
        Err(outcome) => outcome,
    }
}

/// Puts the `size` bytes `file` gives as the object `key`, whose bytes must hash to `sha256`.
pub(super) async fn put_body<R>(
    cache: &LocalCache,
    key: &str,
    sha256: TreeDigest,
    file: R,
    size: u64,
) -> Outcome
where
    R: AsyncRead + Unpin + Send,
{
    let broken = AtomicBool::new(false);
    let body = chunks(file, &broken);
    match cache.put_stream_sha256(key, size, body, sha256).await {
        Ok(outcome) => Outcome::Stored(outcome),
        Err(error) if broken.load(Ordering::Relaxed) => Outcome::Unreadable(error.to_string()),
        Err(Error::BadDigest(_)) => Outcome::Refused(Refusal::NotItsDigest),
        Err(Error::AlreadyExists(_)) => Outcome::Refused(Refusal::OtherBytes),
        Err(Error::TooLarge(_)) => Outcome::Refused(Refusal::TooLarge),
        Err(error) => Outcome::Again(error),
    }
}
