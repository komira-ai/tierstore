//! What the core keeps of a tree in memory, and how a request reaches it with no shared lock.
//!
//! The trees in memory are a map by tree number that is replaced whole ([`Trees`]): a request
//! reads the map as it is, and a change copies it, changes the copy and swaps it in, under a
//! lock that only the changes take. No request that names a tree in memory waits for a change or
//! for another request.
//!
//! The count of the lookups of a tree's inodes ([`Lookups`]) is one word that every change of
//! is a compare-and-swap of: the count, whether a forget has outrun it (the tree is *uncounted*,
//! `core/memory.rs`), and a mark that the tree is being taken out of memory. A forget that takes
//! a tree out first sets the mark, only if the count is zero and the tree is not uncounted, and
//! then removes the tree from the map; the sweep for idle trees sets it whatever the count
//! (`core/memory.rs`). A lookup that finds the mark does not count: it puts the tree back under
//! the lock of the changes (`Trees::put_back`), so that a lookup is never lost.
//!
//! The blobs a tree has asked the cache to pin are a set split by the blob's digest
//! ([`Pinned`]), so that the opens of one tree do not queue on one lock.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use arc_swap::ArcSwap;

use crate::tree_manifest::TreeDigest;

use super::super::locked;
use super::super::tree::Tree;
use super::read::FileBytes;

/// The mark of a tree that is being taken out of memory, in a [`Lookups`].
const REMOVED: u64 = 1 << 63;

/// The mark of a tree that the kernel forgot more lookups of than were counted, in a [`Lookups`].
const UNCOUNTED: u64 = 1 << 62;

/// The bits of a [`Lookups`] that hold the count.
#[cfg(any(target_os = "linux", test))]
const COUNT: u64 = UNCOUNTED - 1;

/// How many sets the blobs of a tree that are pinned are split in.
const PIN_SHARDS: usize = 16;

/// The lookups of a tree's inodes that the kernel has not forgotten, as far as this process
/// counted them, with the marks of the module docs.
#[derive(Debug, Default)]
pub(super) struct Lookups(AtomicU64);

impl Lookups {
    /// The lookups counted.
    #[cfg(test)]
    pub(super) fn count(&self) -> u64 {
        self.0.load(Ordering::Acquire) & COUNT
    }

    /// Whether the kernel forgot more lookups than were counted.
    #[cfg(test)]
    pub(super) fn uncounted(&self) -> bool {
        self.0.load(Ordering::Acquire) & UNCOUNTED != 0
    }

    /// Whether the tree is marked as taken out of memory.
    pub(super) fn removed(&self) -> bool {
        self.0.load(Ordering::Acquire) & REMOVED != 0
    }

    /// Counts one more lookup, unless the tree is marked as removed; whether it was counted.
    pub(super) fn raise(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                (now & REMOVED == 0).then_some(now + 1)
            })
            .is_ok()
    }

    /// Counts one more lookup and clears the mark of removal: for the tree that is put back.
    pub(super) fn raise_and_keep(&self) {
        let _ = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                Some((now & !REMOVED) + 1)
            });
    }

    /// Takes away `nlookup` lookups. With `counting`, a forget of more than were counted marks the
    /// tree uncounted. Nothing is done to a tree marked as removed, which a forget that follows
    /// its removal does not find either; whether anything was.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn forget(&self, nlookup: u64, counting: bool) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                if now & REMOVED != 0 {
                    return None;
                }
                let counted = now & COUNT;
                let outrun = if counting && nlookup > counted {
                    UNCOUNTED
                } else {
                    0
                };
                Some((now & UNCOUNTED) | outrun | counted.saturating_sub(nlookup))
            })
            .is_ok()
    }

    /// Whether no lookup is counted and the tree is not uncounted or marked as removed: the state
    /// a forget takes a tree out of memory from.
    pub(super) fn is_forgotten(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }

    /// Marks the tree as removed if it has no lookup counted and is not uncounted; whether it did.
    pub(super) fn remove_if_forgotten(&self) -> bool {
        self.0
            .compare_exchange(0, REMOVED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Marks the tree as removed whatever it has counted, counted or uncounted, unless it is
    /// marked already; whether it did. The count is kept: a lookup that puts the tree back adds
    /// to it.
    pub(super) fn remove(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                (now & REMOVED == 0).then_some(now | REMOVED)
            })
            .is_ok()
    }
}

/// The blobs of a tree that it has asked the cache to pin, in sets split by the digest of the
/// blob.
#[derive(Debug, Default)]
pub(super) struct Pinned([Mutex<HashSet<TreeDigest>>; PIN_SHARDS]);

impl Pinned {
    /// The set that holds `digest`.
    fn shard(&self, digest: &TreeDigest) -> &Mutex<HashSet<TreeDigest>> {
        &self.0[usize::from(digest.as_bytes()[0]) % PIN_SHARDS]
    }

    /// Whether the blob `digest` is pinned for the tree.
    pub(super) fn contains(&self, digest: &TreeDigest) -> bool {
        locked(self.shard(digest)).contains(digest)
    }

    /// Records that the blob `digest` is pinned.
    pub(super) fn insert(&self, digest: TreeDigest) {
        let _ = locked(self.shard(&digest)).insert(digest);
    }

    /// Forgets every pin, one set at a time.
    pub(super) fn clear(&self) {
        self.0.iter().for_each(|shard| locked(shard).clear());
    }

    /// The blobs pinned.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.iter().map(|shard| locked(shard).len()).sum()
    }
}

/// A tree that is in memory.
#[derive(Debug)]
pub(super) struct Loaded {
    pub(super) digest: TreeDigest,
    pub(super) tree: Tree,
    /// The lookups of its inodes the kernel has not forgotten yet, as far as this process
    /// counted them, and whether the kernel forgot more: then no forget takes the tree out of
    /// memory (`core/memory.rs`).
    pub(super) lookups: Lookups,
    /// When a request last named the tree or one of its inodes, on the core's clock.
    pub(super) used: AtomicU64,
    /// The blobs this tree has asked the cache to pin.
    pub(super) pinned: Pinned,
    /// Whether a lookup in a directory of the tree, or a listing of one, was served while it is in
    /// memory (`Core::looked_into`).
    pub(super) looked_into: AtomicBool,
    /// For a placed tree, the span of the pack each file it opened was given, by the file's blob:
    /// where it begins, and the pack's file it was read from while that is open
    /// ([`Loaded::held_span`]).
    spans: Mutex<HashMap<TreeDigest, (u64, Weak<File>)>>,
}

impl Loaded {
    /// The tree `digest`, which was last named at `used`, with no lookup counted and no blob
    /// pinned.
    pub(super) fn new(digest: TreeDigest, tree: Tree, used: u64) -> Self {
        Self {
            digest,
            tree,
            lookups: Lookups::default(),
            used: AtomicU64::new(used),
            pinned: Pinned::default(),
            looked_into: AtomicBool::new(false),
            spans: Mutex::new(HashMap::new()),
        }
    }

    /// Keeps the span `bytes` the file of the blob `digest` was given, for [`Loaded::held_span`].
    pub(super) fn keep_span(&self, digest: &TreeDigest, bytes: &FileBytes) {
        let _ = locked(&self.spans).insert(*digest, (bytes.base, Arc::downgrade(&bytes.file)));
    }

    /// The `size` bytes of the file of the blob `digest`, from the span it was given, while the
    /// pack's file that span was read from is open: the node said that file holds the span, and
    /// the bytes of an open file stay as they are (a pack of the cache is filled in place, never
    /// cut), so the node need not be asked again. None for a span never given or a file closed.
    pub(super) fn held_span(&self, digest: &TreeDigest, size: u64) -> Option<FileBytes> {
        let (base, file) = locked(&self.spans).get(digest).cloned()?;
        Some(FileBytes {
            file: file.upgrade()?,
            base,
            size,
        })
    }
}

/// The trees in memory, by tree number.
#[derive(Debug, Default)]
pub(super) struct Trees {
    map: ArcSwap<HashMap<u64, Arc<Loaded>>>,
    /// Held by a change of the map, for the copy and the swap: no request that reads takes it.
    changes: Mutex<()>,
}

impl Trees {
    /// The tree `number`, if it is in memory.
    pub(super) fn get(&self, number: u64) -> Option<Arc<Loaded>> {
        self.map.load().get(&number).map(Arc::clone)
    }

    /// How many trees are in memory.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.map.load().len()
    }

    /// The trees in memory, by number, as they are now.
    pub(super) fn all(&self) -> Vec<(u64, Arc<Loaded>)> {
        let map = self.map.load();
        map.iter()
            .map(|(number, loaded)| (*number, Arc::clone(loaded)))
            .collect()
    }

    /// Swaps in `changed`, a copy of the map that the caller changed under the lock of the changes.
    /// The map that was there is given back, so that the trees that left with it are freed after
    /// the lock is released.
    fn replace(&self, changed: HashMap<u64, Arc<Loaded>>) -> Arc<HashMap<u64, Arc<Loaded>>> {
        self.map.swap(Arc::new(changed))
    }

    /// Keeps `fresh` as the tree `number`, unless a tree is in memory under the number already;
    /// the tree that is in memory. A `fresh` that is not kept is dropped with no lock held.
    pub(super) fn keep(&self, number: u64, fresh: Loaded) -> Arc<Loaded> {
        match self.get(number) {
            Some(there) => there,
            None => self.insert(number, fresh),
        }
    }

    /// Keeps `fresh` as the tree `number` under the lock of the changes, unless a tree is there
    /// by then; the tree that is in memory.
    fn insert(&self, number: u64, fresh: Loaded) -> Arc<Loaded> {
        let (kept, old) = {
            let _changes = locked(&self.changes);
            let current = self.map.load_full();
            if let Some(there) = current.get(&number) {
                return Arc::clone(there);
            }
            let kept = Arc::new(fresh);
            let mut changed = HashMap::clone(&current);
            let _ = changed.insert(number, Arc::clone(&kept));
            (kept, self.replace(changed))
        };
        drop(old);
        kept
    }

    /// Puts `loaded`, the tree `number` that a lookup holds, back if it was taken out of memory
    /// since the lookup took it, and counts the lookup; or counts it on the tree that is in memory
    /// now, which may be `loaded` with its removal not done yet, or another read of the tree.
    pub(super) fn put_back(&self, number: u64, loaded: &Arc<Loaded>) {
        let old = {
            let _changes = locked(&self.changes);
            let current = self.map.load_full();
            if let Some(there) = current.get(&number) {
                there.lookups.raise_and_keep();
                return;
            }
            loaded.lookups.raise_and_keep();
            let mut changed = HashMap::clone(&current);
            let _ = changed.insert(number, Arc::clone(loaded));
            self.replace(changed)
        };
        drop(old);
    }

    /// Takes `loaded`, the tree `number`, out of the map if it is the tree there and is still
    /// marked as removed (a lookup that counted on it since cleared the mark); the map it was
    /// taken out of, so that the tree is freed after every lock is released.
    pub(super) fn take_out(
        &self,
        number: u64,
        loaded: &Arc<Loaded>,
    ) -> Option<Arc<HashMap<u64, Arc<Loaded>>>> {
        let _changes = locked(&self.changes);
        let current = self.map.load_full();
        let there = current
            .get(&number)
            .is_some_and(|there| Arc::ptr_eq(there, loaded));
        if !there || !loaded.lookups.removed() {
            return None;
        }
        let mut changed = HashMap::clone(&current);
        let _ = changed.remove(&number);
        Some(self.replace(changed))
    }
}

#[cfg(test)]
#[path = "loaded_tests.rs"]
mod tests;
