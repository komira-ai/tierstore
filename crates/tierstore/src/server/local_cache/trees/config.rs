//! The settings of the tree store: the keys of `[trees]` with their defaults.
//!
//! Every key has a default and none is required. The keys that the tree store itself holds a
//! request to are `enabled`, `ttl`, `ttl_max`, `max_placements` and `max_bytes`; `max_entries`,
//! `max_depth` and `max_dir_entries` are the limits [`TreesConfig::limits`] hands the builder of a
//! tree. `fill_concurrency`, `prepare_concurrency` and `prepare_prefetch` are read by the mount
//! provider's `Prepare` (`server/provider`), which also holds its table of sources to
//! `max_sources`. The others (`max_request_bytes`, `prefetch_default`, `create_timeout`,
//! `max_failures`) are read and checked at start so that a file that sets them still loads;
//! nothing in this module uses them, since the create call that read them was removed.

use std::time::Duration;

use crate::TreeLimits;

/// What a create did about the bytes of the tree's files, a value of `prefetch_default` that is
/// read and checked and used by no call now.
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

/// What the mount provider's `Prepare` does about the bytes of the tree's files once the tree is
/// placed (`trees.prepare_prefetch`). It never makes `Prepare` or `Mount` wait for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparePrefetch {
    /// Every file is read into the cache in the background, and the tree is pinned (and marked
    /// ready when every file was read) once the reads end. The default.
    Background,
    /// No file is read: each is filled when it is first opened, and the tree is neither pinned nor
    /// marked ready by `Prepare`.
    None,
}

impl PreparePrefetch {
    /// The value of the key that names this mode.
    #[must_use]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::None => "none",
        }
    }

    /// The mode the value `name` of the key names, or none for a value the key does not take.
    #[must_use]
    pub(crate) fn of_name(name: &str) -> Option<Self> {
        [Self::Background, Self::None]
            .into_iter()
            .find(|mode| mode.name() == name)
    }
}

/// The settings of a [`TreeStore`](super::TreeStore).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreesConfig {
    /// Whether the node makes trees. Off, nothing is placed.
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
    /// The most bytes of the body of one create; checked, and used by no call.
    pub max_request_bytes: u64,
    /// The most file bytes of one tree.
    pub max_bytes: u64,
    /// The most placements alive at once.
    pub max_placements: usize,
    /// The most ranged reads of creates that run at once, node-wide.
    pub fill_concurrency: usize,
    /// The most preparations of the mount provider that read and make their tree at once,
    /// node-wide: each holds its tree's list (at most 256 MiB) and the walk of it meanwhile.
    pub prepare_concurrency: usize,
    /// The most sources the mount provider holds, from their `Prepare` to their `Release` or a
    /// `Sweep`: a `Prepare` of a new source past it is refused `UNAVAILABLE`.
    pub max_sources: usize,
    /// What a create with no `prefetch` did; checked, and used by no call.
    pub prefetch_default: Prefetch,
    /// What the mount provider's `Prepare` does about the bytes of the tree's files.
    pub prepare_prefetch: PreparePrefetch,
    /// The longest a create that waits for its files waited; checked, and used by no call.
    pub create_timeout: Duration,
    /// The most failed entries an answer named; checked, and used by no call.
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
            prepare_concurrency: 8,
            max_sources: 16_384,
            prefetch_default: Prefetch::Wait,
            prepare_prefetch: PreparePrefetch::Background,
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
