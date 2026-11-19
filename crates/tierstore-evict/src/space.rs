//! [`CacheDevice`]: the free space of the cache device, for admission's `enospc` refusal.

use std::io;
use std::path::PathBuf;

/// The free space of the file system the cache lives on.
pub trait CacheDevice: Send + Sync {
    /// The bytes an unprivileged writer can still allocate.
    fn free_bytes(&self) -> io::Result<u64>;
}

/// The free space `statvfs` reports for a path on the cache device (the cache directory).
#[derive(Debug, Clone)]
pub struct Statvfs {
    path: PathBuf,
}

impl Statvfs {
    /// The device of `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl CacheDevice for Statvfs {
    fn free_bytes(&self) -> io::Result<u64> {
        let st = rustix::fs::statvfs(&self.path)?;
        Ok(st.f_bavail.saturating_mul(st.f_frsize))
    }
}
