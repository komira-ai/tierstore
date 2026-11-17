//! The tree store: the node-local trees of `docs/design/node-local-trees.md`, beside the cache.
//!
//! A **tree** is a directory structure whose files are spans of cached packs. It is made by one
//! call, [`TreeStore::create`], from the records and files [`build_tree`](crate::build_tree)
//! makes of a request, or by [`TreeStore::create_image`] from a version 2 tree list, which is the
//! tree as it is (an **image**: [`TreeImage`](crate::TreeImage)), and it appears at one or more
//! **locations**: a bucket and key prefix of the S3 front, or a path under the mount of
//! `tierstore fs`. A tree and the places it appears at are separate things: the tree is content
//! (its digest), a **placement** is that tree at a set of locations, and a placement is what
//! leases, attachments and reads hold. The store keeps them in
//! the cache directory, `trees/<T>/` and `placements/<id>` (module `files` has the layouts), by
//! rename and with no sync, and in memory, where a table sorted by location answers which
//! placement a key falls under. Nothing here is written to a tier or seen by another node, and
//! there is no HTTP in it: the interfaces are adapters over the operations below.
//!
//! # What it answers
//!
//! - [`TreeStore::locate_s3`] and [`TreeStore::locate_fs`]: the placement a key or a path falls
//!   under, and the path below the root of its tree. A key under no placement is an ordinary key.
//! - [`TreeStore::resolve`], [`TreeStore::stat`] and [`TreeStore::list`]: what a path of a
//!   placement is. A file resolves to a **span**, the key of a pack and an offset in it
//!   ([`FileSpan`]): the interface then asks the cache for the range ([`LocalCache::span_of`]) and
//!   reads the pack's own file at the offset. Nothing is copied. The answers read one directory
//!   record at a time from the tree's records file; no tree of records is read whole. An image is
//!   in memory whole and is read in place: a name is found by a binary search of its directory's
//!   children, and nothing is read from a file.
//!
//! # Locations
//!
//! No two placements answer one key. A create whose location is equal to, inside or around a
//! placement's is refused ([`CreateError::Taken`] for an equal one, [`CreateError::Overlaps`] for
//! the others), except the same tree at the same locations, which refreshes the lease. The
//! caller chooses prefixes that are its own; a refusal costs it nothing.
//!
//! # What keeps a tree
//!
//! A placement is alive while something holds it: its **lease**, from the last create until the
//! `ttl` of the request (at most `trees.ttl_max`), an **attachment** of a run of the mount
//! ([`TreeStore::attach`], until [`TreeStore::detach`]), or an open **read** ([`ReadHold`]). A tree
//! is alive while a placement of it is, or another alive tree holds it by digest (a `tree` entry,
//! its **parent**). [`TreeStore::release`] removes a placement's locations at once; it lasts for
//! its holders only. When a placement ends, a tree nothing reaches is dropped: its directory goes
//! and its pins with it, and its packs stay in the cache to be evicted in their turn, so a tree
//! placed again soon is warm. [`TreeStore::sweep`] does this for leases that ended; the loop
//! [`TreeStore::maintain`] calls it.
//!
//! # Pins
//!
//! A tree pins each pack it reads files from, once however many files it has in it, as the owner
//! `("tree", <T>)` of the cache's pin table ([`LocalCache::pin`]'s, with a mount id of the cache's
//! own): the pins are in memory, are not in the cache's pins file, and the store takes them again
//! when it reads the directory. A pack the cache does not hold is not pinned; [`TreeStore::pin`]
//! pins what the cache holds now, and [`TreeStore::mark_ready`] says that every range is held.
//!
//! # A start, and the lock
//!
//! [`TreeStore::open`] reads the directory (module `start` has what it keeps). A tree that was
//! marked ready is dropped if the cache no longer holds a range of it. A store over a cache that
//! holds no lock reads and removes nothing, refuses to create, and serves what it read; the loop
//! reads the directory again once the cache has taken the lock, keeping the attachments and reads
//! the store had.

mod config;
mod create;
mod fetch;
mod files;
mod flat;
mod keys;
mod life;
mod location;
mod read;
mod start;
mod state;
mod table;

#[cfg(test)]
mod create_tests;
#[cfg(test)]
mod fetch_tests;
#[cfg(test)]
mod files_tests;
#[cfg(test)]
mod image_create_tests;
#[cfg(test)]
mod image_tests;
#[cfg(test)]
mod keys_tests;
#[cfg(test)]
mod life_tests;
#[cfg(test)]
mod location_tests;
#[cfg(test)]
mod read_tests;
#[cfg(test)]
mod start_tests;
#[cfg(test)]
mod table_tests;
#[cfg(test)]
pub(in crate::server) mod testing;

use std::io;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

pub use config::{Prefetch, TreesConfig};
pub use create::{CreateError, CreateRequest, Created, ImageRequest, StagedImage};
pub use fetch::{FetchError, OpenFile};
pub(in crate::server) use files::image_path;
pub(in crate::server) use flat::{
    FlatError, FlatKind, FlatTree, MountPlace, MountTree, placement_id,
};
pub use keys::TreeKey;
pub use life::{LifeError, PlacementInfo, ReadHold, SweepReport};
pub use location::{Located, Location, LocationError, PlacementId};
pub use read::{FileSpan, Kind, Listing, ReadError, Resolved, Stat};
pub(in crate::server) use table::{TreesTable, check_trees};

use super::LocalCache;
use state::{Shared, State};

/// The node-local trees of a cache. A handle: clones share one store.
#[derive(Clone)]
pub struct TreeStore {
    shared: Arc<Shared>,
}

impl TreeStore {
    /// Opens the store over `cache`: reads `trees/` and `placements/` of its directory, drops what
    /// does not hold up (see the module docs), and pins what it keeps. A store whose `enabled` is
    /// false reads nothing and makes nothing.
    ///
    /// # Errors
    /// The directories could not be made, in a cache that holds the lock.
    pub fn open(cache: LocalCache, config: TreesConfig) -> io::Result<Self> {
        let root = cache.config().path.clone();
        let enabled = config.enabled;
        let shared = Arc::new(Shared {
            cache,
            config,
            root,
            staged: AtomicU64::new(0),
            state: Mutex::new(State::default()),
        });
        let store = Self { shared };
        if enabled {
            store.load()?;
        }
        Ok(store)
    }

    /// The settings the store was opened with.
    #[must_use]
    pub fn config(&self) -> &TreesConfig {
        &self.shared.config
    }
}
