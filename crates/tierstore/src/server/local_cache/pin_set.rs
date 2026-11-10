//! The set of pins, in memory: which trees pin which blobs, and how many bytes that is.
//!
//! A pin is held for a tree of a mount by the owner `(mount id, tree)`; a blob is pinned while
//! any owner pins it, and its bytes count once however many trees pin it. The set knows nothing
//! of the objects but their sizes, which the caller gives when it pins; the index calls
//! [`PinSet::forget`] when it removes an object and [`PinSet::resize`] when it replaces one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The least time, in milliseconds, between two writes of the file.
pub(super) const SAVE_EVERY: u64 = 1000;

/// The holder of a pin: a tree attached on a mount.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Owner {
    /// The boot id of the mount.
    pub mount: String,
    /// The digest of the tree.
    pub tree: String,
}

/// What a request to pin one blob came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pinned {
    /// The blob is pinned by the owner (it was, or is now).
    Yes,
    /// Pinning it would take the pinned bytes over the limit.
    Refused,
    /// The cache does not hold the blob.
    Absent,
}

/// A blob that is pinned: its size, and the owners.
#[derive(Debug)]
struct Pin {
    size: u64,
    owners: BTreeSet<Arc<Owner>>,
}

/// The pins of the cache.
#[derive(Debug, Default)]
pub(super) struct PinSet {
    by_blob: HashMap<String, Pin>,
    by_owner: BTreeMap<Arc<Owner>, BTreeSet<String>>,
    /// The bytes of the blobs that any owner pins.
    bytes: u64,
    /// Whether the set changed since [`PinSet::saved`].
    dirty: bool,
    /// When the file was last written, on the cache's clock in milliseconds.
    written: Option<u64>,
}

/// The file `<cache>/pins`: the pins of each tree, as the digests of its blobs. Keys a later
/// release adds are ignored here.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PinsFile {
    #[serde(default)]
    pub trees: Vec<TreePins>,
}

/// The pins of one tree of one mount.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TreePins {
    pub mount: String,
    pub tree: String,
    pub blobs: Vec<String>,
}

impl PinSet {
    /// The bytes of the blobs that are pinned.
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether any owner pins `hex`.
    pub(super) fn is_pinned(&self, hex: &str) -> bool {
        self.by_blob.contains_key(hex)
    }

    /// Pins `hex`, an object of `size` bytes (none if the cache does not hold it), for `owner`,
    /// unless that takes the pinned bytes over `limit`. A blob that another tree pins already
    /// costs nothing more.
    pub(super) fn pin(
        &mut self,
        owner: &Owner,
        hex: &str,
        size: Option<u64>,
        limit: u64,
    ) -> Pinned {
        let Some(size) = size else {
            return Pinned::Absent;
        };
        let shared = self
            .by_owner
            .get_key_value(owner)
            .map_or_else(|| Arc::new(owner.clone()), |(known, _)| Arc::clone(known));
        if let Some(pin) = self.by_blob.get_mut(hex) {
            if pin.owners.insert(Arc::clone(&shared)) {
                self.dirty = true;
                self.by_owner
                    .entry(shared)
                    .or_default()
                    .insert(hex.to_owned());
            }
            return Pinned::Yes;
        }
        let Some(total) = self.bytes.checked_add(size).filter(|total| *total <= limit) else {
            return Pinned::Refused;
        };
        self.bytes = total;
        self.dirty = true;
        let owners = BTreeSet::from([Arc::clone(&shared)]);
        self.by_blob.insert(hex.to_owned(), Pin { size, owners });
        self.by_owner
            .entry(shared)
            .or_default()
            .insert(hex.to_owned());
        Pinned::Yes
    }

    /// Takes `owner`'s pins away; gives the number of blobs it pinned.
    pub(super) fn unpin(&mut self, owner: &Owner) -> usize {
        let Some(blobs) = self.by_owner.remove(owner) else {
            return 0;
        };
        self.dirty = true;
        for hex in &blobs {
            let emptied = self.by_blob.get_mut(hex).is_some_and(|pin| {
                pin.owners.remove(owner);
                pin.owners.is_empty()
            });
            if emptied && let Some(pin) = self.by_blob.remove(hex) {
                self.bytes -= pin.size;
            }
        }
        blobs.len()
    }

    /// Takes away the pins of every owner that is not a tree of `mount` in `trees`; gives the
    /// number of owners dropped.
    pub(super) fn retain_live(&mut self, mount: &str, trees: &[String]) -> usize {
        let dead: Vec<Owner> = self
            .by_owner
            .keys()
            .filter(|owner| owner.mount != mount || !trees.contains(&owner.tree))
            .map(|owner| Owner::clone(owner))
            .collect();
        for owner in &dead {
            let _ = self.unpin(owner);
        }
        dead.len()
    }

    /// Takes the pins of the object `hex` away, whoever holds them: it is gone from the cache.
    pub(super) fn forget(&mut self, hex: &str) {
        let Some(pin) = self.by_blob.remove(hex) else {
            return;
        };
        self.dirty = true;
        self.bytes -= pin.size;
        for owner in pin.owners {
            let emptied = self.by_owner.get_mut(&owner).is_some_and(|blobs| {
                blobs.remove(hex);
                blobs.is_empty()
            });
            if emptied {
                let _ = self.by_owner.remove(&owner);
            }
        }
    }

    /// Notes that the object `hex`, if pinned, is now `size` bytes: it was written over.
    pub(super) fn resize(&mut self, hex: &str, size: u64) {
        if let Some(pin) = self.by_blob.get_mut(hex) {
            self.bytes = self.bytes - pin.size + size;
            pin.size = size;
        }
    }

    /// The pins as a file holds them, the trees in order and their blobs in order.
    pub(super) fn to_file(&self) -> PinsFile {
        let trees = self
            .by_owner
            .iter()
            .map(|(owner, blobs)| TreePins {
                mount: owner.mount.clone(),
                tree: owner.tree.clone(),
                blobs: blobs.iter().cloned().collect(),
            })
            .collect();
        PinsFile { trees }
    }

    /// If the file is due: the set changed, and `now` is a second or more after the last write
    /// (or the clock went back), or `force`. Marks the set saved as of `now`; [`PinSet::unsaved`]
    /// undoes that if the write fails.
    pub(super) fn due(&mut self, now: u64, force: bool) -> Option<PinsFile> {
        let rested = self
            .written
            .is_none_or(|then| then > now || now - then >= SAVE_EVERY);
        if !self.dirty || !(force || rested) {
            return None;
        }
        self.dirty = false;
        self.written = Some(now);
        Some(self.to_file())
    }

    /// Notes that the file [`PinSet::due`] gave was not written.
    pub(super) fn unsaved(&mut self) {
        self.dirty = true;
    }

    /// Whether the set changed since the file was last written.
    #[cfg(test)]
    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }
}
