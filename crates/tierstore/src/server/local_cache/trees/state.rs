//! What the tree store holds in memory, and the pins it takes in the cache.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard};

use super::super::layout::name_of;
use super::super::pin_set::{Owner, TREE_MOUNT};
use super::super::{LocalCache, PinAnswer, locked};
use super::config::TreesConfig;
use super::files::{Place, Sources};
use super::location::{Location, PlacementId, Table};
use super::mapped::MappedLists;
use crate::TreeDigest;
use crate::tree_image_list::ListImage;

/// A tree the node holds: what is in its directory, read once.
#[derive(Debug)]
pub(super) struct TreeInfo {
    /// The digest of the tree: its root record's, or for an image the SHA-256 of the image.
    pub root: TreeDigest,
    /// This tree and every tree below it by digest, this one first: where a record or a source
    /// the tree's own entries do not hold may be. An image holds no tree: its own alone.
    pub closure: Vec<TreeDigest>,
    /// What the tree is made of.
    pub content: Content,
    /// Whether every range of the tree was in the cache and pinned when it was last said.
    pub ready: AtomicBool,
}

/// What a tree is made of.
#[derive(Debug)]
pub(super) enum Content {
    /// Directory records named by their digests, read from the tree's records file one at a
    /// time, and the sources of its files by digest.
    Records {
        /// Where each record of the tree's own records file is.
        records: HashMap<TreeDigest, Place>,
        /// Where the bytes of the tree's own files are fetched from.
        sources: Sources,
    },
    /// A version 2 tree list, read in place from its mapped image file (shared with the mount of
    /// this process, `mapped.rs`): entries by index, each file at its own span.
    Image(Arc<ListImage>),
}

impl TreeInfo {
    /// The keys of the packs the tree's own files are read from.
    pub(super) fn packs(&self) -> Vec<&str> {
        match &self.content {
            Content::Records { sources, .. } => sources.packs.iter().map(String::as_str).collect(),
            Content::Image(image) => image.keys().iter().map(AsRef::as_ref).collect(),
        }
    }

    /// The records of the tree and the sources of its files, if it is a tree of records.
    pub(super) fn records(&self) -> Option<(&HashMap<TreeDigest, Place>, &Sources)> {
        match &self.content {
            Content::Records { records, sources } => Some((records, sources)),
            Content::Image(_) => None,
        }
    }

    /// The image the tree is, if it is one.
    pub(super) fn image(&self) -> Option<&Arc<ListImage>> {
        match &self.content {
            Content::Image(image) => Some(image),
            Content::Records { .. } => None,
        }
    }
}

/// A tree at a set of locations, and what keeps it.
#[derive(Debug)]
pub(super) struct Placement {
    pub id: PlacementId,
    pub tree: TreeDigest,
    pub locations: Vec<Location>,
    /// When the placement was first made, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// When the lease ends.
    pub expires_ms: u64,
    /// Whether the placement was released: its locations are not in the table, and it lasts only
    /// for the holders it has.
    pub released: bool,
    /// The runs of the mount that attached the placement.
    pub attachments: BTreeSet<String>,
    /// The reads open on the placement.
    pub reads: usize,
}

impl Placement {
    /// Whether something other than the lease holds the placement.
    pub(super) fn held(&self) -> bool {
        !self.attachments.is_empty() || self.reads > 0
    }

    /// Whether the placement is alive at `now`: a lease that has not ended, or a holder.
    pub(super) fn retained(&self, now: u64) -> bool {
        self.held() || (!self.released && self.expires_ms > now)
    }

    /// Whether a lookup by location finds the placement at `now`: alive, and not released.
    pub(super) fn locatable(&self, now: u64) -> bool {
        !self.released && self.retained(now)
    }
}

/// The trees, the placements and the table of their locations.
#[derive(Debug, Default)]
pub(super) struct State {
    /// Whether the files of the directory are this store's to change: it holds the cache's lock
    /// and has read the directory since it took it.
    pub owned: bool,
    /// The generation of the cache's lock when the directory was read.
    pub generation: u64,
    pub trees: HashMap<TreeDigest, Arc<TreeInfo>>,
    pub placements: HashMap<PlacementId, Placement>,
    pub table: Table,
}

/// What every handle of a store shares.
pub(super) struct Shared {
    pub cache: LocalCache,
    pub config: TreesConfig,
    /// The cache directory.
    pub root: PathBuf,
    /// The number of the next staging directory.
    pub staged: AtomicU64,
    pub state: Mutex<State>,
    /// Held for each read of the directory that replaces `state` ([`TreeStore::refresh`]).
    ///
    /// [`TreeStore::refresh`]: super::TreeStore::refresh
    pub reading: Mutex<()>,
    /// The tree lists in memory, which the mount of this process shares (`mapped.rs`).
    pub lists: Arc<MappedLists>,
}

impl Shared {
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        locked(&self.state)
    }

    /// The time, in milliseconds, on the cache's clock.
    pub(super) fn now(&self) -> u64 {
        self.cache.inner.clock.now()
    }

    /// Whether the files may be changed now: the state was read under the lock the cache holds.
    pub(super) fn writable(&self, state: &State) -> bool {
        state.owned
            && self.cache.inner.read_write()
            && state.generation == self.cache.inner.generation()
    }

    /// Pins the packs of `tree` for it, as far as the cache holds them and the limit allows. A
    /// cache that is read-only pins nothing.
    pub(super) fn pin(&self, tree: &TreeInfo) -> PinAnswer {
        let names: Vec<String> = tree
            .packs()
            .into_iter()
            .map(|pack| name_of(pack).hex)
            .collect();
        let owner = tree.root.to_string();
        self.cache
            .inner
            .pin(TREE_MOUNT, &owner, &names)
            .unwrap_or_default()
    }

    /// Takes away the pins of `tree`; the packs stay in the cache, to be evicted in their turn.
    pub(super) fn unpin(&self, tree: &TreeDigest) {
        let owner = Owner {
            mount: TREE_MOUNT.to_owned(),
            tree: tree.to_string(),
        };
        let _ = locked(&self.cache.inner.index).pins.unpin(&owner);
    }
}

/// `root` and every tree below it by digest, `root` first, each once.
pub(super) fn closure(
    trees: &HashMap<TreeDigest, Arc<TreeInfo>>,
    root: TreeDigest,
    children: &[TreeDigest],
) -> Vec<TreeDigest> {
    let mut out = vec![root];
    for child in children {
        for below in trees.get(child).iter().flat_map(|info| &info.closure) {
            if !out.contains(below) {
                out.push(*below);
            }
        }
    }
    out
}
