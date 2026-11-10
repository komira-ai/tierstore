//! Pins: the trees that ask the cache to keep their blobs.
//!
//! A tree of a mounted virtual build file system (`tierstore fs`) asks the cache, by the
//! operations below, not to evict the blobs it has opened. The cache keeps, for each object,
//! the trees that pin it, in the index, in memory; the unit is the tree of a mount, so two runs
//! of one tree share a pin. The operations are for the cache's holder of the lock only: a
//! read-only cache answers [`PinError::ReadOnly`], and the caller asks again of the one that
//! holds it.
//!
//! - [`LocalCache::pin`] pins the blobs, named by the SHA-256 of their bytes: the blob a fill for
//!   a tree kept (`_tiering/fill/<sha256>`), else the object `cas/l/<sha256>`. A blob the cache does not hold is not pinned (it is counted as absent: the caller
//!   pins it again once it has been read). A pin that would take the pinned bytes over
//!   [`CacheConfig::pin_limit`] is refused, and the blob is counted as used, so that it is kept
//!   by recency only: pins buy speed, and running out of them must not stop a job.
//! - [`LocalCache::unpin`] takes away every pin of a tree.
//! - [`LocalCache::pins_live`] takes away every pin of another mount and of a tree not in the
//!   list: the pins never outlive their mount or their attachment.
//!
//! A pinned object is skipped by the eviction pass and counted in `pin_skips`; it stays charged
//! to the budget. Removals that are not eviction (the scrub finding bad bytes, a revalidation, a
//! write-through delete) remove a pinned object and its pins: correctness wins over a pin.
//!
//! # The file
//!
//! The pins are in the file `<path>/pins`, JSON, which the holder of the lock rewrites by
//! rename, at most once a second (at a [`LocalCache::tick`], which [`LocalCache::hold`] calls
//! every `lock_interval`), and once more when [`LocalCache::hold`] ends, which is the cache
//! giving up the lock. A cache that takes the lock reads it, after its scan, and keeps the pins
//! whose blobs it holds, up to the limit. A file that cannot be read or parsed is ignored and
//! said on stderr: the pins are a speed-up, and the caller sends them again. The scan of the
//! directory reads `obj/` and `.stage/` only, so a release that has no pins starts over a
//! directory that has this file, and leaves it where it is.

use std::io;
use std::path::Path;

use thiserror::Error;

use super::index::Index;
use super::layout::is_digest;
use super::source::blob_name;
use super::pin_set::{Owner, Pinned, PinsFile};
use super::{CacheConfig, Inner, LocalCache, locked};

/// The name of the pins file in the cache directory.
const PINS: &str = "pins";

/// The name the file is written under before it is renamed to [`PINS`].
const PINS_NEW: &str = "pins.new";

/// Why a pin operation was not done.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PinError {
    /// This cache does not hold the lock of its directory, so it keeps no pins.
    #[error("this cache does not hold the lock of its directory")]
    ReadOnly,
    /// A blob is named by something that is not the lower-case hex of a SHA-256.
    #[error("{0:?} is not the lower-case hex of a SHA-256")]
    NotADigest(String),
}

/// What [`LocalCache::pin`] did with the blobs of a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinAnswer {
    /// The blobs now pinned by the tree, those it pinned before included.
    pub pinned: usize,
    /// The blobs refused because the pinned bytes would pass the limit; each was counted as used.
    pub refused: usize,
    /// The blobs the cache does not hold.
    pub absent: usize,
}

impl Inner {
    /// Fails unless this cache may write to its directory.
    fn pins_writable(&self) -> Result<(), PinError> {
        if self.read_write() {
            Ok(())
        } else {
            Err(PinError::ReadOnly)
        }
    }

    /// See [`LocalCache::pin`].
    fn pin(&self, mount: &str, tree: &str, blobs: &[String]) -> Result<PinAnswer, PinError> {
        self.pins_writable()?;
        if let Some(bad) = blobs.iter().find(|blob| !is_digest(blob)) {
            return Err(PinError::NotADigest(bad.clone()));
        }
        let owner = Owner {
            mount: mount.to_owned(),
            tree: tree.to_owned(),
        };
        let (limit, now) = (self.config.pin_limit, self.clock.now());
        let mut answer = PinAnswer::default();
        let mut index = locked(&self.index);
        for sha256 in blobs {
            // A blob filled for a tree (`fill_blob`) is kept under its reserved key; else the name
            // is the object `cas/l/<sha256>`'s.
            let filled = blob_name(sha256);
            let hex = if index.get(&filled).is_some() {
                &filled
            } else {
                sha256
            };
            let size = index.get(hex).map(|entry| entry.size);
            match index.pins.pin(&owner, hex, size, limit) {
                Pinned::Yes => answer.pinned += 1,
                Pinned::Absent => answer.absent += 1,
                Pinned::Refused => {
                    answer.refused += 1;
                    let _ = index.used(hex, now);
                }
            }
        }
        Ok(answer)
    }

    /// Writes the pins file if it is due (see `PinSet::due`), or now if `force`. A cache that is
    /// read-only writes nothing. A write that fails is said on stderr and tried again at the next
    /// call.
    pub(super) fn save_pins(&self, force: bool) {
        if !self.read_write() {
            return;
        }
        let due = locked(&self.index).pins.due(self.clock.now(), force);
        let Some(file) = due else {
            return;
        };
        if let Err(why) = write_pins(&self.config.path, &file) {
            tracing::warn!("tierstore: local cache: the pins file was not written: {why}");
            locked(&self.index).pins.unsaved();
        }
    }
}

/// Writes `file` as `<root>/pins`, by a rename of a file written beside it.
fn write_pins(root: &Path, file: &PinsFile) -> io::Result<()> {
    let text = serde_json::to_vec(file).map_err(io::Error::other)?;
    let new = root.join(PINS_NEW);
    std::fs::write(&new, text)?;
    std::fs::rename(&new, root.join(PINS))
}

/// The pins file of `root`: none if there is none, and none, said on stderr, if it cannot be read
/// or parsed.
fn read_pins(root: &Path) -> PinsFile {
    let text = match std::fs::read(root.join(PINS)) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return PinsFile::default(),
        Err(why) => {
            tracing::warn!("tierstore: local cache: the pins file was not read: {why}");
            return PinsFile::default();
        }
    };
    serde_json::from_slice(&text).unwrap_or_else(|why| {
        tracing::warn!("tierstore: local cache: the pins file was ignored: {why}");
        PinsFile::default()
    })
}

/// Pins in `index` what the file of the cache directory holds, for the blobs `index` has and
/// as far as the limit of `config` goes.
pub(super) fn restore(config: &CacheConfig, index: &mut Index) {
    for tree in read_pins(&config.path).trees {
        let owner = Owner {
            mount: tree.mount,
            tree: tree.tree,
        };
        for hex in tree.blobs {
            let size = index.get(&hex).map(|entry| entry.size);
            let _ = index.pins.pin(&owner, &hex, size, config.pin_limit);
        }
    }
}

impl LocalCache {
    /// Pins the blobs `sha256` (lower-case hex, the names of `cas/l/` keys) for `tree` of the
    /// mount `mount`, as far as [`CacheConfig::pin_limit`] allows, and says what became of them.
    /// Pinning again what the tree pins is the same answer.
    ///
    /// # Errors
    /// [`PinError::ReadOnly`] if this cache does not hold the lock of its directory;
    /// [`PinError::NotADigest`] for a name that is not a digest, with nothing pinned.
    pub fn pin(&self, mount: &str, tree: &str, sha256: &[String]) -> Result<PinAnswer, PinError> {
        self.inner.pin(mount, tree, sha256)
    }

    /// Takes away every pin of `tree` of `mount`; gives the number of blobs it pinned.
    ///
    /// # Errors
    /// [`PinError::ReadOnly`] if this cache does not hold the lock of its directory.
    pub fn unpin(&self, mount: &str, tree: &str) -> Result<usize, PinError> {
        self.inner.pins_writable()?;
        let owner = Owner {
            mount: mount.to_owned(),
            tree: tree.to_owned(),
        };
        Ok(locked(&self.inner.index).pins.unpin(&owner))
    }

    /// Takes away every pin that is not of a tree in `trees` of the mount `mount`; gives the
    /// number of trees whose pins it took away.
    ///
    /// # Errors
    /// [`PinError::ReadOnly`] if this cache does not hold the lock of its directory.
    pub fn pins_live(&self, mount: &str, trees: &[String]) -> Result<usize, PinError> {
        self.inner.pins_writable()?;
        Ok(locked(&self.inner.index).pins.retain_live(mount, trees))
    }

    /// The bytes of the objects that are pinned: they are in the budget.
    #[must_use]
    pub fn pinned_bytes(&self) -> u64 {
        locked(&self.inner.index).pins.bytes()
    }
}
