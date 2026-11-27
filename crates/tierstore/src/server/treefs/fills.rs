//! What the file system's core asks of the node's cache and tree store, and the two ways it is
//! asked (`docs/design/one-service.md`, section 1.5): over the fill socket of the node's
//! tierstore, by [`FillClient`](super::fill_client::FillClient), for the process `tierstore fs`;
//! and by calls inside the one process that holds the cache, by
//! [`InProcess`](super::in_process::InProcess). The answers have the words of the fill socket's in
//! both, so that the core reads them one way.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use tokio::runtime::Handle;

use crate::TreeDigest;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::TreeFilled;
use crate::server::local_cache::attaches::{Attach, Fetches, Levels};
use crate::server::local_cache::descriptors::Descriptors;
use crate::server::local_cache::opens::Opens;
use crate::server::local_cache::requests::Requests;
use crate::server::local_cache::runs::{RunAttach, RunDetach};
use crate::server::local_cache::tree_lists::TreeLists;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use crate::server::local_cache::trees::{MappedLists, TreeStore};

use super::config::FsSettings;
use super::fill_client::{AskError, FillClient, FillFailure, PAUSE, Place, PlacedTree, Until};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use super::in_process::InProcess;

/// Which of the two ways a start makes its core ask: over the fill socket of its settings, or by
/// calls of the cache and the tree store of this process.
#[derive(Clone)]
pub(super) enum Reach {
    /// Over `fs.fill_socket`: the process `tierstore fs`.
    Socket,
    /// By calls: the `fs` interface of the one process (`docs/design/one-service.md`, section
    /// 1.5), which serves on Linux and macOS.
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    Calls(Calls),
}

/// The node's cache and tree store, called in this process.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone)]
pub(super) struct Calls {
    /// The node's cache.
    pub cache: LocalCache,
    /// The node's tree store, if it has one.
    pub trees: Option<TreeStore>,
    /// The runtime the cache's work runs on.
    pub runtime: Handle,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl Calls {
    /// The requests to these, each waiting at most `wait`, with the pauses of the core's tries
    /// taken by `pause`.
    fn fills(&self, wait: Duration, pause: Arc<dyn Fn(Duration) + Send + Sync>) -> Arc<dyn Fills> {
        let reached = (self.cache.clone(), self.trees.clone());
        Arc::new(InProcess::new(reached, self.runtime.clone(), wait, pause))
    }
}

impl Reach {
    /// What the core of `settings` asks through: it waits `fs.fetch_timeout` for an answer, and
    /// pauses between its tries.
    pub(super) fn core(&self, settings: &FsSettings) -> Arc<dyn Fills> {
        match self {
            Self::Socket => Arc::new(FillClient::new(
                settings.fill_socket.clone(),
                settings.fetch_timeout,
            )),
            #[cfg(any(target_os = "linux", target_os = "macos", test))]
            Self::Calls(calls) => calls.fills(settings.fetch_timeout, Arc::new(std::thread::sleep)),
        }
    }

    /// What the put directory of `settings` asks through, for a caller that waits between its
    /// tries itself: it never pauses, and waits `answer_timeout` for an answer.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn puts(&self, settings: &FsSettings, answer_timeout: Duration) -> Arc<dyn Fills> {
        match self {
            Self::Socket => Arc::new(FillClient::for_puts(
                settings.fill_socket.clone(),
                answer_timeout,
            )),
            Self::Calls(calls) => calls.fills(answer_timeout, Arc::new(std::mem::drop::<Duration>)),
        }
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
    /// How long the requests of the mount's frontend took (`local_cache/requests.rs`); boxed, as
    /// it is far larger than the others.
    Requests(Box<Requests>),
    /// An attach of a tree for a run, and its parts (`local_cache/runs.rs`).
    RunAttached(RunAttach),
    /// A detach of a run (`local_cache/runs.rs`).
    RunDetached(RunDetach),
}

impl Report {
    /// The request of the fill socket that tells it.
    pub(super) fn line(&self) -> String {
        match self {
            Self::Started(path) => format!("started {path}"),
            Self::Attached(attach) => attach.line(),
            Self::Fetched(fetches) => fetches.line(),
            Self::Levels(levels) => levels.line(),
            Self::Descriptors(descriptors) => descriptors.line(),
            Self::Opens(opens) => opens.line(),
            Self::TreeLists(lists) => lists.line(),
            Self::Requests(requests) => requests.line(),
            Self::RunAttached(attach) => attach.line(),
            Self::RunDetached(detach) => detach.line(),
        }
    }
}

/// What the node answered to a fill of a tree's new manifests (`fill-tree`,
/// `local_cache/tree_fill.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TreeAnswer {
    /// It filled them: what it did, on its clock.
    Filled(TreeFilled),
    /// It is of a release before the request.
    Unknown,
    /// It did not fill them all: why, in its words or the client's.
    Failed(String),
}

/// The requests of the core to the node's cache and tree store (the verbs of the fill socket,
/// `server/fill_socket/request.rs`, which says what each does).
pub(super) trait Fills: Send + Sync {
    /// The time an answer is waited for, and the write role of the cache: `fs.fetch_timeout`.
    fn answer_timeout(&self) -> Duration;

    /// Puts the whole object `cas/l/<sha256>` in the cache, answered by `until`.
    ///
    /// # Errors
    /// The words of the fill's failure, or that it was not answered in time.
    fn fill_manifest(&self, sha256: &str, until: Until<'_>) -> Result<(), FillFailure>;

    /// Puts the new manifests of the tree whose root manifest is `root` in the cache, the node
    /// going down the tree itself, answered by `until`. The answer is never an error: a node that
    /// did not fill them all leaves the rest to the reader, which fills them one at a time.
    fn fill_tree(&self, root: &TreeDigest, until: Until<'_>) -> TreeAnswer;

    /// Puts the blob `sha256` of `size` bytes at `offset` of the upstream object `key` in the
    /// cache, answered by `until` where given.
    ///
    /// # Errors
    /// As [`Fills::fill_manifest`].
    fn fill_blob(
        &self,
        blob: (&str, u64),
        source: (&str, u64),
        until: Option<Until<'_>>,
    ) -> Result<(), FillFailure>;

    /// Counts the blobs as used now; nothing is answered.
    fn touch(&self, sha256: &[&str]);

    /// Pins `blobs` for `tree` of `mount`: `ok` and the numbers, or the words of the refusal.
    ///
    /// # Errors
    /// The request was not answered.
    fn pin(&self, mount: &str, tree: &str, blobs: &[&str]) -> Result<String, AskError>;

    /// Takes away the pins of `tree` of `mount`.
    ///
    /// # Errors
    /// As [`Fills::pin`].
    fn unpin(&self, mount: &str, tree: &str) -> Result<String, AskError>;

    /// Keeps the pins of `trees` of `mount` and takes away the others.
    ///
    /// # Errors
    /// As [`Fills::pin`].
    fn pins_live(&self, mount: &str, trees: &[String]) -> Result<String, AskError>;

    /// Puts the `size` bytes of the file at `path`, which hash to `sha256`, as the object `key`,
    /// create-only, waiting at most `wait`: the answer is the line of JSON of the fill socket's
    /// `put` (`server/fill_socket/put.rs`).
    ///
    /// # Errors
    /// As [`Fills::pin`].
    #[cfg(any(target_os = "linux", test))]
    fn put(
        &self,
        object: (&str, &str, u64),
        path: &std::path::Path,
        wait: Duration,
    ) -> Result<String, AskError>;

    /// Where the path `path` of the mount is among the placements of the node; a placement found
    /// there is held for `mount` until [`Fills::unplace`].
    ///
    /// # Errors
    /// The words of an answer that is none of these, or that nobody answered.
    fn place(&self, mount: &str, path: &str) -> Result<Place, String>;

    /// The paths of the fs locations of the placements of the node.
    ///
    /// # Errors
    /// As [`Fills::place`].
    fn mounted(&self) -> Result<Vec<String>, String>;

    /// Where the tree of the placement `id` is read from.
    ///
    /// # Errors
    /// `none` for a placement that is gone, `damaged`, `too large`, or that nobody answered.
    fn image(&self, id: &str) -> Result<PlacedTree, String>;

    /// The file of the cache that holds the `size` bytes at `offset` of the pack `pack`, which
    /// hash to `sha256`, and the offset of their first byte in it, filled first if the cache does
    /// not hold them, answered by `until` where given.
    ///
    /// # Errors
    /// The words of the failure, or that nobody answered.
    fn span(
        &self,
        blob: (&str, u64),
        source: (&str, u64),
        until: Option<Until<'_>>,
    ) -> Result<(u64, PathBuf), String>;

    /// The file of the cache that holds the `size` bytes at `offset` of the pack `pack`, and the
    /// offset of their first byte in it, if the cache holds them now: answered at once, with
    /// nothing filled and nothing waited for, so that the open of a file of a placed tree whose
    /// bytes the node holds is answered by the reader that took it (`Core::open_now`) and does not
    /// wait in the queue of the fetches behind the opens that do wait for fills. Only a core in
    /// the node's process can answer so (`InProcess`); over the fill socket the question would
    /// cost a round trip on the reader, so the answer is none and the open is parked and asks
    /// [`Fills::span`].
    fn span_held(&self, _blob: (&str, u64), _source: (&str, u64)) -> Option<(u64, PathBuf)> {
        None
    }

    /// Ends the hold of `mount` on the placement `id`.
    fn unplace(&self, mount: &str, id: &str);

    /// Ends the holds of `mount` on every placement but `ids`.
    ///
    /// # Errors
    /// As [`Fills::pin`].
    fn places_live(&self, mount: &str, ids: &[String]) -> Result<String, AskError>;

    /// Tells the node `report`, for its metrics; the caller does not wait for it.
    fn report(&self, report: Report);

    /// The tree lists the node's tree store holds in memory, which the core shares where the store
    /// is in this process; none over the fill socket, where the core keeps its own.
    fn lists(&self) -> Option<Arc<MappedLists>> {
        None
    }

    /// Waits `pause`: a sleep, or what a test puts in its place.
    fn pause(&self, pause: Duration);

    /// Waits as long as a request waits between two tries.
    fn rest(&self) {
        self.pause(PAUSE);
    }

    /// [`Fills::rest`], if that leaves time before `until` for another try; whether it did.
    fn rest_before(&self, until: Until<'_>) -> bool {
        // A pause that leaves no time would be followed by a try that is not sent.
        if until.left() <= PAUSE {
            return false;
        }
        self.pause(PAUSE);
        true
    }
}
