//! [`EvictorLock`]: the directory lock that makes exactly one process the evictor (section 7).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

/// The evictor lock's file name, under the entries' directory `packs-v2` of the cache.
pub const EVICTOR_LOCK: &str = ".lock";

/// The entries' directory of a cache (the same name as the Linux entry store's).
const PACKS_DIR: &str = "packs-v2";

/// Why the evictor lock was not taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The entries' directory or the lock file could not be made or opened.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// Another process (or another open of the file in this one) holds the lock, or the lock
    /// could not be taken.
    #[error("locking {}: {source}", path.display())]
    Locked {
        /// The lock file.
        path: PathBuf,
        /// The error (`EWOULDBLOCK` when another holder has it).
        #[source]
        source: io::Error,
    },
}

/// `flock(LOCK_EX)` on `<cache>/packs-v2/.lock`, held while the value lives. Only the process
/// that holds it may make an [`Evictor`](crate::Evictor): a second one gets
/// [`LockError::Locked`] at once (the lock is never waited for) and evicts nothing.
#[derive(Debug)]
pub struct EvictorLock {
    path: PathBuf,
    _file: File,
}

impl EvictorLock {
    /// Takes the lock of the cache in `cache_dir`, making `packs-v2` and the lock file if they
    /// are missing.
    pub fn acquire(cache_dir: &Path) -> Result<Self, LockError> {
        let dir = cache_dir.join(PACKS_DIR);
        std::fs::create_dir_all(&dir).map_err(|source| LockError::Io {
            op: "making",
            path: dir.clone(),
            source,
        })?;
        let path = dir.join(EVICTOR_LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockError::Io {
                op: "opening",
                path: path.clone(),
                source,
            })?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self { path, _file: file }),
            Err(e) => Err(LockError::Locked {
                path,
                source: e.into(),
            }),
        }
    }

    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
