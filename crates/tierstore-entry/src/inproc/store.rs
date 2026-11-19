//! [`InProcStore`]: the node cache's entries in process (5.6).
//!
//! **Locks.** As in the Linux store: the map of entries is locked while an entry is opened
//! (making its directory, opening and locking its data file, replaying its journal) or removed
//! (unlinking its files), and while a holder attaches; never across a read, a write, a sync or a
//! punch of a cell, and the read path never takes it (5.5). The order is: the map of entries, then
//! the map of holders, then an entry's own locks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tierstore_fetch_api::entry::{
    Entry, EntryError, EntryInfo, EntrySpec, EntryStore, HolderCounters,
};
use tierstore_fetch_api::ids::{HolderId, PackId};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_journal::{BootId, DirFiles, EntryFiles, FileKind};

use super::{InProcEntry, lock};
use crate::layout::{StoreError, entry_dir, packs_dir, region_end, unlink};

/// The stem of the file [`punches_inside_a_file`] tries a punch on, in the entries' directory.
/// It is not 64 hex digits, so it never names an entry.
pub(crate) const PROBE_STEM: &str = ".punch-probe";

/// The length of the probe file: two pages, the first of which is punched.
const PROBE_LEN: u64 = 8192;

/// Whether the volume of the directory `dir` releases a run inside a file: punches the first page
/// of a two-page probe file there ([`EntryFiles::punch_data`]), then removes the file. Any
/// failure (making the file, sizing it, punching) is `false`. On macOS, where no safe
/// `F_PUNCHHOLE` exists, it is `false`, and eviction takes whole entries only (5.6 (b)).
#[must_use]
pub fn punches_inside_a_file(dir: &Path) -> bool {
    let punched = DirFiles::open(dir, PROBE_STEM).and_then(|f| {
        f.set_len(FileKind::Data, PROBE_LEN)?;
        f.punch_data(0, PROBE_LEN / 2)
    });
    // The probe file is the store's own; a failure to remove it leaves one small file.
    let _ = std::fs::remove_file(dir.join(format!("{PROBE_STEM}{}", FileKind::Data.suffix())));
    punched.is_ok()
}

/// The entries of one cache directory in process, each an [`InProcEntry`]: the store of macOS
/// (5.6), where the NFS server runs in the tierstore process, and of any process that serves its
/// own reads (it runs on Linux too, where the conformance suite runs it).
#[derive(Debug)]
pub struct InProcStore {
    dir: PathBuf,
    boot: Option<BootId>,
    punch: bool,
    entries: Mutex<BTreeMap<PackId, Arc<InProcEntry>>>,
    holders: Mutex<BTreeMap<HolderId, Arc<dyn HolderCounters>>>,
}

impl InProcStore {
    /// The store of the cache directory `cache` in boot `boot` (`None` when the boot id could
    /// not be read: every restored cell is then `recheck`, section 8): its entries live in
    /// `cache/packs-v2`, which is made if it is missing, and whether eviction may punch is probed
    /// there once ([`punches_inside_a_file`]). It starts with no entry open: an entry is opened,
    /// locked and restored from its journal by [`EntryStore::open`].
    pub fn new(cache: &Path, boot: Option<BootId>) -> Result<Self, StoreError> {
        let dir = packs_dir(cache)?;
        let punch = punches_inside_a_file(&dir);
        Ok(Self {
            dir,
            boot,
            punch,
            entries: Mutex::new(BTreeMap::new()),
            holders: Mutex::new(BTreeMap::new()),
        })
    }

    /// The directory of the entries.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether entries punch evicted runs; `false` means eviction removes whole entries only
    /// (5.6 (b)).
    #[must_use]
    pub fn can_punch(&self) -> bool {
        self.punch
    }

    /// The entry of `pack` as its concrete type, for the read path ([`InProcEntry::read_at`]).
    #[must_use]
    pub fn entry(&self, pack: &PackId) -> Option<Arc<InProcEntry>> {
        lock(&self.entries).get(pack).cloned()
    }

    /// Closes every open entry ([`InProcEntry::close`]: a fold of its journal), as the process
    /// does before it exits; the first error, after every entry was tried. A store dropped
    /// without it is a process crash: the next open replays the journals.
    pub fn close(&self) -> Result<(), EntryError> {
        let entries: Vec<Arc<InProcEntry>> = lock(&self.entries).values().cloned().collect();
        let mut first = Ok(());
        for e in &entries {
            let closed = e.close();
            if first.is_ok() {
                first = closed;
            }
        }
        first
    }

    /// The store with eviction by punch turned off, as on a volume that cannot punch.
    #[cfg(test)]
    pub(crate) fn without_punch(mut self) -> Self {
        self.punch = false;
        self
    }
}

impl EntryStore for InProcStore {
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
        let (dir, stem) = entry_dir(&self.dir, &spec.pack);
        let (entry, _) = InProcEntry::open(spec, &dir, &stem, self.boot, self.punch)?;
        let entry = Arc::new(entry);
        entries.insert(spec.pack.clone(), entry.clone());
        Ok(entry)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        lock(&self.entries)
            .get(pack)
            .map(|e| e.clone() as Arc<dyn Entry>)
    }

    /// Removes an unheld entry from the store and unlinks its data file, journal and snapshot.
    /// A file already gone counts as unlinked; a failure leaves the entry listed, and a retry
    /// unlinks what is left. The lock of the old data file is released when the last reference to
    /// the entry drops; the next [`EntryStore::open`] of the pack makes new files.
    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        let mut entries = lock(&self.entries);
        let Some(entry) = entries.get(pack) else {
            return Ok(false);
        };
        if entry.holders() > 0 {
            return Ok(false);
        }
        for file in FileKind::ALL {
            unlink(&entry.path(file)).map_err(|source| EntryError::Io {
                pack: pack.clone(),
                op: "unlink entry file",
                source,
            })?;
        }
        entries.remove(pack);
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
        let end = region_end(region, entry.group_count()).ok_or_else(bad)?;
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
        let entries: Vec<Arc<InProcEntry>> = lock(&self.entries).values().cloned().collect();
        // One entry per pack; a store does not hold 2^32 of them.
        entries.iter().filter(|e| e.forget_holder(holder)).count() as u32
    }
}
