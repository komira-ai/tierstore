//! [`DevFs`]: the hot device seen as the [`Fs`] of `tierstore-index`, so the index's recovery
//! (10.2 steps 1 to 4) reads the same files the hot tier writes, through the same device.

use std::io;
use std::sync::Arc;

use tierstore_index::fs::Fs;

use crate::dev::{HotDev, dir_of};

/// The hot device as an [`Fs`].
#[derive(Clone)]
pub struct DevFs {
    dev: Arc<dyn HotDev>,
}

impl DevFs {
    /// The [`Fs`] of `dev`.
    pub fn new(dev: Arc<dyn HotDev>) -> Self {
        Self { dev }
    }

    /// Makes the directory entries of `path` durable, from its own directory up to the root.
    fn sync_dirs(&self, path: &str) -> io::Result<()> {
        let mut dir = dir_of(path);
        loop {
            self.dev.fsync_dir(dir)?;
            if dir.is_empty() {
                return Ok(());
            }
            dir = dir_of(dir);
        }
    }
}

impl std::fmt::Debug for DevFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevFs").finish_non_exhaustive()
    }
}

impl Fs for DevFs {
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        self.dev.list(dir)
    }

    fn len(&self, path: &str) -> io::Result<u64> {
        self.dev.len(path)
    }

    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        self.dev.read_at(path, offset, len)
    }

    fn append(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        let at = match self.dev.len(path) {
            Ok(len) => len,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.dev.create(path)?;
                0
            }
            Err(e) => return Err(e),
        };
        self.dev.write_at(path, at, bytes)
    }

    fn truncate(&self, path: &str, len: u64) -> io::Result<()> {
        self.dev.set_len(path, len)
    }

    fn sync(&self, path: &str) -> io::Result<()> {
        self.dev.fdatasync(path)?;
        self.sync_dirs(path)
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        self.dev.unlink(path)?;
        self.sync_dirs(path)
    }
}
