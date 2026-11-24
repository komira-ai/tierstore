//! What the file system's core asks of the node's cache and tree store, and the two ways it is
//! asked (`docs/design/one-service.md`, section 1.5): over the fill socket of the node's
//! tierstore, by [`FillClient`](super::fill_client::FillClient), for the process `tierstore fs`;
//! and by calls inside the one process that holds the cache, by
//! [`InProcess`](super::in_process::InProcess). The answers have the words of the fill socket's in
//! both, so that the core reads them one way.

use std::path::PathBuf;
use std::time::Duration;

use crate::server::local_cache::attaches::{Attach, Fetches};
use crate::server::local_cache::descriptors::Descriptors;

use super::fill_client::{AskError, FillFailure, PAUSE, Place, PlacedTree, Until};

/// What the core tells the node for its metrics; the core does not wait for it to be told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Report {
    /// The mount first looked into the tree at the fs location (`local_cache/starts.rs`).
    Started(String),
    /// A lookup attached a tree, or could not (`local_cache/attaches.rs`).
    Attached(Attach),
    /// Where the manifests of a read of a tree came from (`local_cache/attaches.rs`).
    Fetched(Fetches),
    /// The cache files the mount holds open (`local_cache/descriptors.rs`).
    Descriptors(Descriptors),
}

impl Report {
    /// The request of the fill socket that tells it.
    pub(super) fn line(&self) -> String {
        match self {
            Self::Started(path) => format!("started {path}"),
            Self::Attached(attach) => attach.line(),
            Self::Fetched(fetches) => fetches.line(),
            Self::Descriptors(descriptors) => descriptors.line(),
        }
    }
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

    /// Ends the hold of `mount` on the placement `id`.
    fn unplace(&self, mount: &str, id: &str);

    /// Ends the holds of `mount` on every placement but `ids`.
    ///
    /// # Errors
    /// As [`Fills::pin`].
    fn places_live(&self, mount: &str, ids: &[String]) -> Result<String, AskError>;

    /// Tells the node `report`, for its metrics; the caller does not wait for it.
    fn report(&self, report: Report);

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
