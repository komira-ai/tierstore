//! The settings of the tree store: the keys of `[trees]` with their defaults.
//!
//! Every key has a default and none is required. The keys that the tree store itself holds a
//! request to are `enabled`, `ttl`, `ttl_max`, `max_placements` and `max_bytes`; `max_entries`,
//! `max_depth` and `max_dir_entries` are the limits [`TreesConfig::limits`] hands the builder of a
//! tree. The others (`max_request_bytes`, `fill_*`, `coalesce_gap`, `prefetch_default`,
//! `whole_pack_percent`, `create_timeout`, `max_failures`) are read and checked at start and held
//! here for the create call that fetches the files of a tree; nothing in this module uses them.

use std::time::Duration;

use crate::TreeLimits;

/// What a create does about the bytes of the tree's files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prefetch {
    /// Nothing is fetched: a range is filled when it is first read.
    None,
    /// The call returns at once and the ranges are filled in the background.
    Background,
    /// The call returns when every range is in the cache, or fails whole.
    Wait,
}

/// The settings of a [`TreeStore`](super::TreeStore).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreesConfig {
    /// Whether the node makes trees. Off, a create is refused and nothing is placed.
    pub enabled: bool,
    /// How long a placement lasts after its last create when the request gives no `ttl`.
    pub ttl: Duration,
    /// The longest `ttl` taken: a longer one is taken as this.
    pub ttl_max: Duration,
    /// The most entries of one create, below the root.
    pub max_entries: u64,
    /// The most names on a path.
    pub max_depth: usize,
    /// The most children of one directory.
    pub max_dir_entries: usize,
    /// The most bytes of the body of one create.
    pub max_request_bytes: u64,
    /// The most file bytes of one tree.
    pub max_bytes: u64,
    /// The most placements alive at once.
    pub max_placements: usize,
    /// The most ranged reads of creates that run at once, node-wide.
    pub fill_concurrency: usize,
    /// The bytes a second of all creates together, or none for no limit.
    pub fill_rate: Option<u64>,
    /// The most blobs waiting to be fetched.
    pub fill_queue: usize,
    /// Ranges of one object this near one another are one read.
    pub coalesce_gap: u64,
    /// What a create with no `prefetch` does.
    pub prefetch_default: Prefetch,
    /// The share of a pack, in percent, the needed ranges must cover for the whole pack to be
    /// read in one request.
    pub whole_pack_percent: u8,
    /// The longest a create that waits for its files waits.
    pub create_timeout: Duration,
    /// The most failed entries an answer names.
    pub max_failures: usize,
}

impl Default for TreesConfig {
    fn default() -> Self {
        let limits = TreeLimits::default();
        Self {
            enabled: true,
            ttl: Duration::from_secs(4 * 3600),
            ttl_max: Duration::from_secs(24 * 3600),
            max_entries: limits.max_entries,
            max_depth: limits.max_depth,
            max_dir_entries: limits.max_dir_entries,
            max_request_bytes: 256 << 20,
            max_bytes: 32 << 30,
            max_placements: 1024,
            fill_concurrency: 8,
            fill_rate: None,
            fill_queue: 16_384,
            coalesce_gap: 256 << 10,
            prefetch_default: Prefetch::Wait,
            whole_pack_percent: 50,
            create_timeout: Duration::from_secs(120),
            max_failures: 20,
        }
    }
}

impl TreesConfig {
    /// The limits the builder of a tree ([`build_tree`](crate::build_tree)) holds a request to.
    #[must_use]
    pub fn limits(&self) -> TreeLimits {
        TreeLimits {
            max_entries: self.max_entries,
            max_depth: self.max_depth,
            max_dir_entries: self.max_dir_entries,
        }
    }
}
