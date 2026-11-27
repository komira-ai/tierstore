//! What the file system's core asks of the node's cache and tree store, which it calls in the
//! process that holds them ([`InProcess`](super::in_process::InProcess),
//! `docs/design/one-service.md`, section 1.5), and the values those requests are answered with.

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::server::local_cache::LocalCache;
use crate::server::local_cache::TreeFilled;
use crate::server::local_cache::attaches::{Attach, Fetches, Levels};
use crate::server::local_cache::descriptors::Descriptors;
use crate::server::local_cache::opens::Opens;
use crate::server::local_cache::tree_lists::TreeLists;
use crate::server::local_cache::trees::{FlatTree, TreeStore};
use crate::{Clock, TreeDigest};

use super::config::FsSettings;
use super::in_process::InProcess;

/// How long the core waits between two tries of a fill ([`InProcess::rest`]).
pub(super) const PAUSE: Duration = Duration::from_secs(1);

/// Why a request did not get its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AskError {
    /// The request was not made: the core is held while a successor takes over (`core/live.rs`).
    #[error("the cache is not answering this mount while its successor takes over")]
    Unavailable,
    /// The request gave no answer in `fetch_timeout`; the core's `passes` reads these words.
    #[error("the cache gave no answer in time")]
    TimedOut,
    /// The end the request was given passed before it was answered.
    #[error("the cache did not answer within the time the request was given")]
    Spent,
    /// The cache of this process stayed read-only for the whole of `fetch_timeout`: another
    /// process holds the lock of its directory.
    #[error("the cache is read-only: another process holds the lock of its directory")]
    ReadOnly,
}

/// What the words of a failure for lack of descriptors begin with (`core/blob.rs`).
pub(super) const NO_DESCRIPTOR: &str = "no descriptor: ";

/// Whether `error` is a call that failed for lack of descriptors, of the process (`EMFILE`) or of
/// the system (`ENFILE`); its errno if so.
pub(super) fn lacks_descriptors(error: &std::io::Error) -> Option<i32> {
    let errno = rustix::io::Errno::from_io_error(error)?;
    [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE]
        .contains(&errno)
        .then(|| errno.raw_os_error())
}

/// When a request is to be answered by, on a clock: what the fills of one tree's manifests share.
#[derive(Clone, Copy)]
pub(super) struct Until<'a> {
    /// The clock the end is on.
    pub clock: &'a dyn Clock,
    /// The end, in milliseconds of `clock`.
    pub end_ms: u64,
}

impl Until<'_> {
    /// The time left before the end; zero once it has passed.
    pub(super) fn left(&self) -> Duration {
        Duration::from_millis(self.end_ms.saturating_sub(self.clock.now()))
    }
}

/// The answer to a fill that did not put the blob in the cache: the words of the cache
/// (`upstream 404`, `mismatch`, `no space`, ...), or that it gave no answer.
pub(super) type FillFailure = String;

/// Where the mount reads the tree of a placement from.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PlacedTree {
    /// The tree is the image of this digest, in the tree's directory of the cache directory.
    Image {
        /// The digest of the tree.
        tree: TreeDigest,
        /// The fs locations of the placement.
        paths: Vec<String>,
    },
    /// The tree, flattened.
    Flat(FlatTree),
}

/// Where a path of the mount is among the placements of the node.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The fs location of the placement with this id, which the node now holds for the mount.
    Tree(String),
    /// A directory above a location.
    Dir,
    /// Nothing is there.
    Nothing,
}

/// The node's cache and tree store, called in this process.
#[derive(Clone)]
pub(super) struct Calls {
    /// The node's cache.
    pub cache: LocalCache,
    /// The node's tree store, if it has one.
    pub trees: Option<TreeStore>,
    /// The runtime the cache's work runs on.
    pub runtime: Handle,
}

impl Calls {
    /// The requests to these, each waiting at most `wait`, with the pauses of the core's tries
    /// taken by `pause`.
    fn asks(&self, wait: Duration, pause: Arc<dyn Fn(Duration) + Send + Sync>) -> Arc<InProcess> {
        let reached = (self.cache.clone(), self.trees.clone());
        Arc::new(InProcess::new(reached, self.runtime.clone(), wait, pause))
    }

    /// What the core of `settings` asks through: it waits `fs.fetch_timeout` for an answer, and
    /// sleeps between its tries.
    pub(super) fn core(&self, settings: &FsSettings) -> Arc<InProcess> {
        self.asks(settings.fetch_timeout, Arc::new(std::thread::sleep))
    }

    /// What the put directory asks through, for a caller that waits between its tries itself: it
    /// never pauses, and waits `answer_timeout` for an answer.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn puts(&self, answer_timeout: Duration) -> Arc<InProcess> {
        self.asks(answer_timeout, Arc::new(std::mem::drop::<Duration>))
    }
}

/// What the core tells the node for its metrics; the core does not wait for it to be told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Report {
    /// The mount first looked into the tree at the fs location (`local_cache/starts.rs`).
    Started(String),
    /// A lookup attached a tree, or could not (`local_cache/attaches.rs`).
    Attached(Attach),
    /// Where the manifests of a read of a tree came from (`local_cache/attaches.rs`).
    Fetched(Fetches),
    /// The levels of a read of a tree and its rounds of fills (`local_cache/attaches.rs`).
    Levels(Levels),
    /// The cache files the mount holds open (`local_cache/descriptors.rs`).
    Descriptors(Descriptors),
    /// How long the mount's opens took, and how many wait now (`local_cache/opens.rs`); boxed,
    /// as it is far larger than the others.
    Opens(Box<Opens>),
    /// The tree lists the mount holds mapped (`local_cache/tree_lists.rs`).
    TreeLists(TreeLists),
}

/// What the cache answered to a fill of a tree's new manifests (`local_cache/tree_fill.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TreeAnswer {
    /// It filled them: what it did, on its clock.
    Filled(TreeFilled),
    /// It did not fill them all: why, in the cache's words or the request's.
    Failed(String),
}
