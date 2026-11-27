//! The requests of the file system's core, made by calls of the node's cache and tree store inside
//! the process that holds them (`docs/design/one-service.md`, section 1.5): each request is made by
//! the method of the cache or the tree store that does it, and is answered with the words of the
//! cache.
//!
//! # The write role
//!
//! Only the process that holds the lock of the cache directory writes the cache. A request made
//! while the cache is read-only waits for the write role (`LocalCache::watch_mode`), up to
//! `fs.fetch_timeout`, or to the end the request was given where that is sooner. A request still waiting then fails: a
//! fill or a span with the words `read-only`, which the core asks again while its open has time
//! and then fails with `EIO`; any other with [`AskError::ReadOnly`], or [`AskError::Spent`] where
//! the request's own end came first. A touch and the reports do not wait: the cache passes over a
//! touch while it is read-only, and the reports reach the cache's metrics at once, with no queue
//! and no thread of their own.
//!
//! # Threads
//!
//! The core is called on blocking threads (the FUSE readers, the blocking tasks of the NFS
//! frontend), never on a thread of the runtime. A request that awaits the cache (a fill, a span, a
//! put) runs as a task on the runtime of the cache, and its caller waits for the answer on a
//! channel, no longer than the wait above; the fill or the put is a task of its own, which goes on
//! when its caller stops waiting. The other requests wait for the write role the same way and then call the
//! cache or the tree store on the caller's thread.
//!
//! # Checks
//!
//! The words a request is made of come from the trees (the keys of the upstream objects) and from
//! the names a job gives in the put directory, so they are checked first ([`checks`]), and a
//! request that fails a check is answered `refused` and the words of the refusal.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::time::{Instant, timeout_at};

use crate::TreeDigest;
use crate::server::local_cache::layout::is_digest;
use crate::server::local_cache::trees::{
    FileSpan, FlatError, MappedLists, MountPlace, MountTree, ReadError, TreeStore, placement_id,
};
use crate::server::local_cache::{Blob, FillError, LocalCache, Mode, PinError};

use super::fills::{AskError, FillFailure, PAUSE, Place, PlacedTree, Report, TreeAnswer, Until};
use checks::{Refusal, check_blob};

pub(super) mod checks;
#[cfg(any(target_os = "linux", test))]
pub(super) mod put;

/// Why a request got no answer.
#[derive(Debug, PartialEq, Eq)]
enum Missed {
    /// The cache did not take the write role in the wait.
    ReadOnly,
    /// The work did not end in the wait, or the runtime is gone.
    Late,
}

/// The answer to a request refused for `why`.
fn refused(why: &Refusal) -> String {
    format!("refused {why}")
}

/// The words of a fill or a span that got no answer: `read-only`, or `late`'s.
fn unfilled(missed: &Missed, late: AskError) -> String {
    match missed {
        Missed::ReadOnly => FillError::ReadOnly.to_string(),
        Missed::Late => late.to_string(),
    }
}

/// The error of a request that got no answer: [`AskError::ReadOnly`], or `late`.
fn unasked(missed: &Missed, late: AskError) -> AskError {
    match missed {
        Missed::ReadOnly => AskError::ReadOnly,
        Missed::Late => late,
    }
}

/// The node's cache and tree store, called in this process.
pub(super) struct InProcess {
    cache: LocalCache,
    /// The node's tree store, if it has one (`[trees]`).
    trees: Option<TreeStore>,
    /// The runtime the cache's work runs on.
    runtime: Handle,
    /// `fs.fetch_timeout`: how long a request waits for the write role and for its answer.
    fetch_timeout: Duration,
    /// Takes a pause between two tries of the core; a test replaces it.
    pause: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl InProcess {
    /// The requests of a core to `cache` and `trees`, whose work runs on `runtime`, each waiting
    /// at most `fetch_timeout`; the pauses between the core's tries are taken by `pause` (a sleep,
    /// but in a test).
    pub(super) fn new(
        (cache, trees): (LocalCache, Option<TreeStore>),
        runtime: Handle,
        fetch_timeout: Duration,
        pause: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        Self {
            cache,
            trees,
            runtime,
            fetch_timeout,
            pause,
        }
    }

    /// How long a request given `until` waits, and what it fails with when it got no answer in
    /// that time: `fetch_timeout` and [`AskError::TimedOut`], or the time left before `until` and
    /// [`AskError::Spent`] where that is shorter.
    ///
    /// # Errors
    /// [`AskError::Spent`] when `until` has passed.
    fn wait(&self, until: Option<Until<'_>>) -> Result<(Duration, AskError), AskError> {
        match until.map(|until| until.left()) {
            Some(left) if left.is_zero() => Err(AskError::Spent),
            Some(left) if left < self.fetch_timeout => Ok((left, AskError::Spent)),
            _ => Ok((self.fetch_timeout, AskError::TimedOut)),
        }
    }

    /// The answer of `work`, run on the runtime as a task of its own once the cache is
    /// read-write, waiting `wait` in all for the write role and for the answer. A `work` that
    /// started goes on when its answer is not waited for; one that did not start never does.
    ///
    /// # Errors
    /// [`Missed::ReadOnly`] if the cache did not take the write role in `wait`; [`Missed::Late`]
    /// if `work` did not end in it, or if the runtime is gone.
    fn call<T: Send + 'static>(
        &self,
        wait: Duration,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, Missed> {
        let mut modes = self.cache.watch_mode();
        let (answer, answered) = sync_channel(1);
        // The task is dropped unrun on a runtime that is shutting down, and its end of the
        // channel with it.
        drop(self.runtime.spawn(async move {
            let end = Instant::now() + wait;
            let writable = modes.wait_for(|mode| *mode == Mode::ReadWrite);
            // The borrow of the mode a wait ends with is let go here, before the work.
            let writer = matches!(timeout_at(end, writable).await, Ok(Ok(_)));
            let got = if writer {
                let done = timeout_at(end, tokio::spawn(work)).await;
                done.ok().and_then(Result::ok).ok_or(Missed::Late)
            } else {
                Err(Missed::ReadOnly)
            };
            let _ = answer.send(got);
        }));
        answered.recv().unwrap_or(Err(Missed::Late))
    }

    /// Waits for the cache to be read-write, as [`InProcess::call`] does, up to `fetch_timeout`.
    ///
    /// # Errors
    /// As [`InProcess::call`].
    fn writer(&self) -> Result<(), Missed> {
        self.call(self.fetch_timeout, async {})
    }

    /// The answer of the cache to a request whose words are what `answer` makes of it, once the
    /// cache is read-write.
    ///
    /// # Errors
    /// [`AskError::ReadOnly`] if it did not become so in `fetch_timeout`; [`AskError::TimedOut`]
    /// if the runtime is gone.
    fn asked(&self, answer: impl FnOnce(&LocalCache) -> String) -> Result<String, AskError> {
        self.writer()
            .map_err(|missed| unasked(&missed, AskError::TimedOut))?;
        Ok(answer(&self.cache))
    }

    /// Puts a blob in the cache by `fill`, answered by `until` where given.
    fn fill(
        &self,
        fill: impl Future<Output = Result<(), FillError>> + Send + 'static,
        until: Option<Until<'_>>,
    ) -> Result<(), FillFailure> {
        let (wait, late) = self.wait(until).map_err(|spent| spent.to_string())?;
        match self.call(wait, fill) {
            Ok(filled) => filled.map_err(|why| why.to_string()),
            Err(missed) => Err(unfilled(&missed, late)),
        }
    }

    /// The time an answer is waited for, and the write role of the cache: `fs.fetch_timeout`.
    pub(super) fn answer_timeout(&self) -> Duration {
        self.fetch_timeout
    }

    /// Puts the whole object `cas/l/<sha256>` in the cache, answered by `until`.
    ///
    /// # Errors
    /// The words of the fill's failure, or that it was not answered in time.
    pub(super) fn fill_manifest(&self, sha256: &str, until: Until<'_>) -> Result<(), FillFailure> {
        if !is_digest(sha256) {
            return Err(refused(&Refusal::Digest));
        }
        let (cache, sha256) = (self.cache.clone(), sha256.to_owned());
        self.fill(
            async move { cache.fill_manifest(&sha256).await },
            Some(until),
        )
    }

    /// Puts the new manifests of the tree whose root manifest is `root` in the cache, the cache
    /// going down the tree itself, within the time this request waits. The answer is never an
    /// error: a cache that did not fill them all leaves the rest to the reader, which fills them
    /// one at a time.
    pub(super) fn fill_tree(&self, root: &TreeDigest, until: Until<'_>) -> TreeAnswer {
        let (wait, late) = match self.wait(Some(until)) {
            Ok(wait) => wait,
            Err(spent) => return TreeAnswer::Failed(spent.to_string()),
        };
        let (cache, root) = (self.cache.clone(), *root);
        match self.call(wait, async move { cache.fill_tree(&root, wait).await }) {
            Ok(Ok(filled)) => TreeAnswer::Filled(filled),
            Ok(Err(why)) => TreeAnswer::Failed(why.to_string()),
            Err(missed) => TreeAnswer::Failed(unfilled(&missed, late)),
        }
    }

    /// Puts the blob `sha256` of `size` bytes at `offset` of the upstream object `key` in the
    /// cache, answered by `until` where given.
    ///
    /// # Errors
    /// As [`InProcess::fill_manifest`].
    pub(super) fn fill_blob(
        &self,
        (sha256, size): (&str, u64),
        (key, offset): (&str, u64),
        until: Option<Until<'_>>,
    ) -> Result<(), FillFailure> {
        check_blob((sha256, size), (key, offset)).map_err(|why| refused(&why))?;
        let (cache, sha256, from) = (self.cache.clone(), sha256.to_owned(), key.to_owned());
        let fill = async move {
            let blob = Blob {
                sha256: &sha256,
                size,
                from: &from,
                offset,
            };
            cache.fill_blob(&blob).await
        };
        self.fill(fill, until)
    }

    /// Counts the blobs as used now, at once: a read-only cache passes over them.
    pub(super) fn touch(&self, sha256: &[&str]) {
        self.cache.touch_blobs(sha256.iter().copied());
    }

    /// Pins `blobs` for `tree` of `mount`, once the cache is read-write: `ok <pinned> <refused>
    /// <absent>` (the blobs now pinned by the tree, the blobs refused for the cache's
    /// `pin_limit`, the blobs the cache does not hold), or the words of the refusal (`read-only`,
    /// or that the mount id `hold`, which the cache's holds pin under, is reserved).
    ///
    /// # Errors
    /// The cache did not become read-write in `fetch_timeout`.
    pub(super) fn pin(&self, mount: &str, tree: &str, blobs: &[&str]) -> Result<String, AskError> {
        let blobs: Vec<String> = blobs.iter().map(|&blob| blob.to_owned()).collect();
        self.asked(|cache| {
            pinned(
                cache
                    .pin(mount, tree, &blobs)
                    .map(|got| format!("{} {} {}", got.pinned, got.refused, got.absent)),
            )
        })
    }

    /// Takes away the pins of `tree` of `mount`: `ok <blobs>` (the blobs the tree had pinned), or
    /// the words of the refusal, as [`InProcess::pin`].
    ///
    /// # Errors
    /// As [`InProcess::pin`].
    pub(super) fn unpin(&self, mount: &str, tree: &str) -> Result<String, AskError> {
        self.asked(|cache| pinned(cache.unpin(mount, tree).map(|count| count.to_string())))
    }

    /// Keeps the pins of `trees` of `mount` and takes away the others: `ok <trees>` (the trees
    /// whose pins were taken away), or the words of the refusal, as [`InProcess::pin`].
    ///
    /// # Errors
    /// As [`InProcess::pin`].
    pub(super) fn pins_live(&self, mount: &str, trees: &[String]) -> Result<String, AskError> {
        self.asked(|cache| pinned(cache.pins_live(mount, trees).map(|count| count.to_string())))
    }

    /// Puts the `size` bytes of the staged file at `path`, which hash to `sha256`, as the object
    /// `key`, create-only, waiting at most `wait`: opened once, and read once, hashed and stored in
    /// one pass as it streams to the upstream. The answer is the line of JSON of [`put`].
    ///
    /// # Errors
    /// The cache did not become read-write, or did not answer, in `wait`.
    #[cfg(any(target_os = "linux", test))]
    pub(super) fn put(
        &self,
        (key, sha256, size): (&str, &str, u64),
        path: &std::path::Path,
        wait: Duration,
    ) -> Result<String, AskError> {
        use checks::check_put;
        let checked = TreeDigest::from_hex(sha256)
            .ok_or(Refusal::Digest)
            .and_then(|digest| check_put(key, path).map(|()| digest));
        let digest = match checked {
            Ok(digest) => digest,
            Err(why) => return Ok(refused(&why)),
        };
        let (cache, key, path) = (self.cache.clone(), key.to_owned(), path.to_owned());
        let put = async move { put::answer(&cache, &key, digest, size, &path).await };
        self.call(wait, put)
            .map_err(|missed| unasked(&missed, AskError::TimedOut))
    }

    /// Where the path `path` of the mount is among the placements of the node; a placement found
    /// there is held for `mount` until [`InProcess::unplace`].
    ///
    /// # Errors
    /// That the cache did not become read-write in `fetch_timeout`.
    pub(super) fn place(&self, mount: &str, path: &str) -> Result<Place, String> {
        self.writer()
            .map_err(|missed| unasked(&missed, AskError::TimedOut).to_string())?;
        let Some(trees) = &self.trees else {
            return Ok(Place::Nothing);
        };
        Ok(match trees.mount_place(path) {
            MountPlace::Placement(id) => match trees.attach(&id, mount) {
                Ok(()) => Place::Tree(id.to_string()),
                // Ended between the look and the hold.
                Err(_) => Place::Nothing,
            },
            MountPlace::Above => Place::Dir,
            MountPlace::Nothing => Place::Nothing,
        })
    }

    /// The paths of the fs locations of the placements of the node.
    ///
    /// # Errors
    /// As [`InProcess::place`].
    pub(super) fn mounted(&self) -> Result<Vec<String>, String> {
        self.writer()
            .map_err(|missed| unasked(&missed, AskError::TimedOut).to_string())?;
        Ok(self
            .trees
            .as_ref()
            .map_or_else(Vec::new, TreeStore::mount_paths))
    }

    /// Where the tree of the placement `id` is read from.
    ///
    /// # Errors
    /// `none` for a placement that is gone or a node with no tree store, `damaged`, `too large`,
    /// or the words of a refused id or of a cache that did not become read-write.
    pub(super) fn image(&self, id: &str) -> Result<PlacedTree, String> {
        let id = placement_id(id).ok_or_else(|| refused(&Refusal::Placement))?;
        self.writer()
            .map_err(|missed| unasked(&missed, AskError::TimedOut).to_string())?;
        let trees = self.trees.as_ref().ok_or("none")?;
        match trees.mount_tree(&id) {
            Ok(MountTree::Image { tree, paths }) => Ok(PlacedTree::Image { tree, paths }),
            Ok(MountTree::Flat(flat)) => Ok(PlacedTree::Flat(flat)),
            Err(why) => Err(unread(&why).to_owned()),
        }
    }

    /// The file of the cache that holds the `size` bytes at `offset` of the pack `pack`, which
    /// hash to `sha256`, and the offset of their first byte in it, filled first if the cache does
    /// not hold them, answered by `until` where given.
    ///
    /// # Errors
    /// The words of the failure, `none` for a node with no tree store, or that it got no answer.
    pub(super) fn span(
        &self,
        (sha256, size): (&str, u64),
        (pack, offset): (&str, u64),
        until: Option<Until<'_>>,
    ) -> Result<(u64, PathBuf), String> {
        check_blob((sha256, size), (pack, offset)).map_err(|why| refused(&why))?;
        let digest = TreeDigest::from_hex(sha256).ok_or_else(|| refused(&Refusal::Digest))?;
        let trees = self.trees.clone().ok_or("none")?;
        let (wait, late) = self.wait(until).map_err(|spent| spent.to_string())?;
        let span = FileSpan {
            pack: pack.to_owned(),
            offset,
        };
        let fetched = async move { trees.fetch_span(&span, size, &digest).await };
        match self.call(wait, fetched) {
            Ok(Ok(held)) => Ok((held.offset, held.path)),
            Ok(Err(why)) => Err(why.to_string()),
            Err(missed) => Err(unfilled(&missed, late)),
        }
    }

    /// The span from the node's tree store, if the cache is read-write now and holds it; never
    /// waited for. A read-only cache's index is its holder's as of the last time this process
    /// read it, so a span it names is not taken: the open asks [`InProcess::span`], which waits
    /// for the write role. So the open of a file of a placed tree whose bytes the node holds is
    /// answered by the reader that took it (`Core::open_now`).
    pub(super) fn span_held(
        &self,
        (_, size): (&str, u64),
        (pack, offset): (&str, u64),
    ) -> Option<(u64, PathBuf)> {
        let trees = self.trees.as_ref()?;
        if *self.cache.watch_mode().borrow() != Mode::ReadWrite {
            return None;
        }
        let held = trees.held_span(pack, offset, size)?;
        Some((held.offset, held.path))
    }

    /// Ends the hold, once the cache is read-write; nothing is lost if it does not become so.
    pub(super) fn unplace(&self, mount: &str, id: &str) {
        if self.writer().is_err() {
            return;
        }
        if let (Some(trees), Some(id)) = (&self.trees, placement_id(id)) {
            let _ = trees.detach(&id, mount);
        }
    }

    /// Ends the holds of `mount` on every placement but `ids`: `ok <holds ended>`, or the words of
    /// a refused id.
    ///
    /// # Errors
    /// As [`InProcess::pin`].
    pub(super) fn places_live(&self, mount: &str, ids: &[String]) -> Result<String, AskError> {
        let keep: Option<Vec<_>> = ids.iter().map(|id| placement_id(id)).collect();
        let Some(keep) = keep else {
            return Ok(refused(&Refusal::Placement));
        };
        let trees = self.trees.as_ref();
        self.asked(|_| {
            let ended = trees.map_or(0, |trees| trees.holds_live(mount, &keep));
            format!("ok {ended}")
        })
    }

    /// Tells the cache `report`, for its metrics: at once, on the caller's thread.
    pub(super) fn report(&self, report: Report) {
        match report {
            Report::Started(path) => {
                let _ = self.cache.run_started(&path);
            }
            Report::Attached(attach) => self.cache.tree_attached(attach),
            Report::Fetched(fetches) => self.cache.manifests_fetched(&fetches),
            Report::Levels(levels) => self.cache.tree_levels(levels),
            Report::Descriptors(descriptors) => self.cache.descriptors_reported(descriptors),
            Report::Opens(opens) => self.cache.opens_reported(*opens),
            Report::TreeLists(lists) => self.cache.tree_lists_reported(lists),
        }
    }

    /// The tree lists the node's tree store holds in memory, which the core shares; none for a
    /// node with no tree store, where the core keeps its own.
    pub(super) fn lists(&self) -> Option<Arc<MappedLists>> {
        self.trees.as_ref().map(|trees| Arc::clone(trees.lists()))
    }

    /// Waits `pause`: a sleep, or what a test puts in its place.
    pub(super) fn pause(&self, pause: Duration) {
        (self.pause)(pause);
    }

    /// Waits as long as the core waits between two tries.
    pub(super) fn rest(&self) {
        self.pause(PAUSE);
    }

    /// [`InProcess::rest`], if that leaves time before `until` for another try; whether it did.
    pub(super) fn rest_before(&self, until: Until<'_>) -> bool {
        // A pause that leaves no time would be followed by a try that is not made.
        if until.left() <= PAUSE {
            return false;
        }
        self.pause(PAUSE);
        true
    }
}

/// The answer to a pin request: `ok` and the numbers, or the words of the failure: a read-only
/// cache, or the mount id of the holds.
fn pinned(done: Result<String, PinError>) -> String {
    match done {
        Ok(numbers) => format!("ok {numbers}"),
        Err(PinError::ReadOnly) => "read-only".to_owned(),
        Err(other) => other.to_string(),
    }
}

/// The words of an `image` whose tree cannot be read for `why`.
fn unread(why: &FlatError) -> &'static str {
    match why {
        FlatError::Read(ReadError::NoPlacement) => "none",
        FlatError::Read(_) => "damaged",
        FlatError::TooLarge => "too large",
    }
}

#[cfg(test)]
#[path = "in_process_tests.rs"]
mod tests;
