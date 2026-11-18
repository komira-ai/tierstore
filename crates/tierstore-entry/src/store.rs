//! [`FileEntryStore`]: the node cache's entries on Linux (section 2, 7).
//!
//! Layout: `<cache>/packs-v2/aa/bb/<hash>.pack` (the sparse data file) and `<hash>.jnl` (its
//! journal) beside it, where `<hash>` is the hex sha256 of the pack id and `aa`, `bb` its first two
//! bytes. The id includes `pack_root`, so a changed pack never reuses a file (8).
//!
//! **Locks.** The store's map of entries is locked while an entry is opened or removed (creating
//! or unlinking its files: metadata, no data I/O) and while a holder attaches (which may remap the
//! holder's counter memfd: an `mmap`, no disk I/O). It is never locked across a read, a write, a
//! sync or a punch of an entry, and the FUSE path never takes it (5.5). The order is: the map of
//! entries, then the map of holders, then an entry's own locks.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};
use tierstore_fetch_api::entry::{
    Entry, EntryError, EntryInfo, EntrySpec, EntryStore, HolderCounters,
};
use tierstore_fetch_api::ids::{HolderId, PackId, PackRoot};
use tierstore_fetch_api::page::CounterRegion;

use crate::entry::{EntryPaths, FileEntry};

/// The directory of the entries under the cache directory.
pub const PACKS_DIR: &str = "packs-v2";

/// Why a store could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The entries' directory could not be made.
    #[error("making {}: {source}", path.display())]
    Dir {
        /// The directory.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
}

/// The entries of one cache directory, each a [`FileEntry`].
#[derive(Debug)]
pub struct FileEntryStore {
    dir: PathBuf,
    entries: Mutex<BTreeMap<PackId, Arc<FileEntry>>>,
    holders: Mutex<BTreeMap<HolderId, Arc<dyn HolderCounters>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The hex sha256 that names `pack`'s files: of a version tag, the key's length and bytes, and
/// the root's kind and bytes, so no two ids give the same input.
#[must_use]
pub(crate) fn file_hash(pack: &PackId) -> String {
    let mut h = Sha256::new();
    h.update(b"tierstore-entry/1\0");
    h.update((pack.key.0.len() as u64).to_le_bytes());
    h.update(pack.key.0.as_bytes());
    match pack.root {
        PackRoot::Digest(d) => {
            h.update([0]);
            h.update(d);
        }
        PackRoot::Etag(g) => {
            h.update([1]);
            h.update(g.0);
        }
    }
    h.finalize().iter().fold(String::new(), |mut s, b| {
        // Writing to a String does not fail.
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl FileEntryStore {
    /// The store of the cache directory `cache`: its entries live in `cache/packs-v2`, which is
    /// made if it is missing. It starts with no entry open: an entry is opened (and locked) by
    /// [`EntryStore::open`].
    pub fn new(cache: &Path) -> Result<Self, StoreError> {
        let dir = cache.join(PACKS_DIR);
        std::fs::create_dir_all(&dir).map_err(|source| StoreError::Dir {
            path: dir.clone(),
            source,
        })?;
        Ok(Self {
            dir,
            entries: Mutex::new(BTreeMap::new()),
            holders: Mutex::new(BTreeMap::new()),
        })
    }

    /// The directory of the entries.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The files of `pack`'s entry.
    fn paths(&self, pack: &PackId) -> EntryPaths {
        let hash = file_hash(pack);
        let dir = self.dir.join(&hash[0..2]).join(&hash[2..4]);
        EntryPaths {
            data: dir.join(format!("{hash}.pack")),
            journal: dir.join(format!("{hash}.jnl")),
            dir,
        }
    }

    /// The entry of `pack` as its concrete type.
    #[must_use]
    pub fn file_entry(&self, pack: &PackId) -> Option<Arc<FileEntry>> {
        lock(&self.entries).get(pack).cloned()
    }
}

impl EntryStore for FileEntryStore {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        let mut entries = lock(&self.entries);
        if let Some(e) = entries.get(&spec.pack) {
            if e.size() != spec.size {
                return Err(EntryError::SizeMismatch {
                    pack: spec.pack.clone(),
                    size: spec.size,
                    existing: e.size(),
                });
            }
            return Ok(e.clone());
        }
        let paths = self.paths(&spec.pack);
        std::fs::create_dir_all(&paths.dir).map_err(|source| EntryError::Io {
            pack: spec.pack.clone(),
            op: "make entry directory",
            source,
        })?;
        let entry = Arc::new(FileEntry::open(&spec.pack, spec.size, paths)?);
        entries.insert(spec.pack.clone(), entry.clone());
        Ok(entry)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        lock(&self.entries)
            .get(pack)
            .map(|e| e.clone() as Arc<dyn Entry>)
    }

    /// Removes an unheld entry from the store and unlinks its data file and journal. A mount that
    /// still has the data descriptor keeps reading the old inode; the next [`EntryStore::open`] of
    /// the pack makes new files. The lock of the old data file is released when the last
    /// reference to the entry drops.
    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        let mut entries = lock(&self.entries);
        let Some(entry) = entries.get(pack) else {
            return Ok(false);
        };
        if entry.holders() > 0 {
            return Ok(false);
        }
        let paths = entry.paths().clone();
        entries.remove(pack);
        for path in [&paths.data, &paths.journal] {
            std::fs::remove_file(path).map_err(|source| EntryError::Io {
                pack: pack.clone(),
                op: "unlink entry file",
                source,
            })?;
        }
        Ok(true)
    }

    fn packs(&self) -> Vec<PackId> {
        lock(&self.entries).keys().cloned().collect()
    }

    fn entries(&self) -> Vec<EntryInfo> {
        lock(&self.entries)
            .values()
            .map(|e| EntryInfo {
                pack: e.pack().clone(),
                size: e.size(),
                last_use: e.last_use(),
                holders: e.holders(),
                counts: e.counts(),
            })
            .collect()
    }

    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        lock(&self.holders).entry(holder).or_insert(counters);
        Ok(())
    }

    /// Checks in the order of the trait (pack, holder, region), all under the map of entries, so
    /// neither a removal of the entry nor a [`EntryStore::drop_holder`] of the holder can come
    /// between the checks and the attachment.
    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        let entries = lock(&self.entries);
        let entry = entries
            .get(pack)
            .ok_or_else(|| EntryError::UnknownPack { pack: pack.clone() })?;
        let counters = lock(&self.holders)
            .get(&holder)
            .cloned()
            .ok_or(EntryError::UnknownHolder { holder })?;
        let bad = || EntryError::BadRegion {
            pack: pack.clone(),
            region,
        };
        let end = region
            .end()
            .filter(|_| region.is_valid() && region.groups == entry.group_count())
            .ok_or_else(bad)?;
        match entry.region_of(holder) {
            Some(known) if known == region => return Ok(()),
            Some(_) => return Err(bad()),
            None => {}
        }
        counters.ensure_len(end)?;
        entry.attach_region(holder, region, counters);
        Ok(())
    }

    /// Forgets the holder's counters first, so an [`EntryStore::attach`] that starts after can no
    /// longer find them, then its regions and holds on every entry.
    fn drop_holder(&self, holder: HolderId) -> u32 {
        lock(&self.holders).remove(&holder);
        let entries: Vec<Arc<FileEntry>> = lock(&self.entries).values().cloned().collect();
        // One entry per pack; a store does not hold 2^32 of them.
        entries.iter().filter(|e| e.forget_holder(holder)).count() as u32
    }
}
