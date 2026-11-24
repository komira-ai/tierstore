//! The requests of the file system's core made by calls inside the process that holds the node's
//! cache and tree store (`docs/design/one-service.md`, section 1.5): each request of the fill
//! socket (`server/fill_socket/`) is made by the method of the cache or the tree store that the
//! socket's answer to it calls, with no line and no socket between, and is answered with the
//! words the socket would give.
//!
//! # The write role
//!
//! The fill socket is bound only while the cache holds the lock of its directory, and a client
//! that finds no socket asks again for `fetch_timeout`. So here: a request made while the cache
//! is read-only waits for the write role (`LocalCache::watch_mode`), up to `fs.fetch_timeout`, or
//! to the end the request was given where that is sooner. A request still waiting then fails: a
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
//! when its caller stops waiting, as the fill socket's fill goes on when its client stops waiting
//! for the answer. The other requests wait for the write role the same way and then call the
//! cache or the tree store on the caller's thread.
//!
//! # Checks
//!
//! The words a request is made of come from the trees (the keys of the upstream objects) and from
//! the names a job gives in the put directory, so they are checked as the socket checks a line
//! (`fill_socket/request.rs`), and a request that fails a check is answered as the socket answers
//! it: `refused` and the words of the refusal.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::time::{Instant, timeout_at};

use crate::TreeDigest;
use crate::server::fill_socket;
use crate::server::fill_socket::request::{Refusal, check_blob};
use crate::server::local_cache::layout::is_digest;
use crate::server::local_cache::trees::{FileSpan, MountPlace, MountTree, TreeStore, placement_id};
use crate::server::local_cache::{Blob, FillError, LocalCache, Mode};

use super::fill_client::{AskError, FillFailure, Place, PlacedTree, Until};
use super::fills::{Fills, Report};

/// Why a request got no answer.
#[derive(Debug, PartialEq, Eq)]
enum Missed {
    /// The cache did not take the write role in the wait.
    ReadOnly,
    /// The work did not end in the wait, or the runtime is gone.
    Late,
}

/// The answer of the socket to a request it refuses for `why`.
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
}

impl Fills for InProcess {
    fn answer_timeout(&self) -> Duration {
        self.fetch_timeout
    }

    fn fill_manifest(&self, sha256: &str, until: Until<'_>) -> Result<(), FillFailure> {
        if !is_digest(sha256) {
            return Err(refused(&Refusal::Digest));
        }
        let (cache, sha256) = (self.cache.clone(), sha256.to_owned());
        self.fill(
            async move { cache.fill_manifest(&sha256).await },
            Some(until),
        )
    }

    fn fill_blob(
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
    fn touch(&self, sha256: &[&str]) {
        self.cache.touch_blobs(sha256.iter().copied());
    }

    fn pin(&self, mount: &str, tree: &str, blobs: &[&str]) -> Result<String, AskError> {
        let blobs: Vec<String> = blobs.iter().map(|&blob| blob.to_owned()).collect();
        self.asked(|cache| fill_socket::pin(cache, (mount, tree), &blobs))
    }

    fn unpin(&self, mount: &str, tree: &str) -> Result<String, AskError> {
        self.asked(|cache| fill_socket::unpin(cache, mount, tree))
    }

    fn pins_live(&self, mount: &str, trees: &[String]) -> Result<String, AskError> {
        self.asked(|cache| fill_socket::pins_live(cache, mount, trees))
    }

    /// Puts the staged file as `fill_socket::put::answer` does: opened once, and read once,
    /// hashed and stored in one pass as it streams to the upstream.
    #[cfg(any(target_os = "linux", test))]
    fn put(
        &self,
        (key, sha256, size): (&str, &str, u64),
        path: &std::path::Path,
        wait: Duration,
    ) -> Result<String, AskError> {
        use crate::server::fill_socket::request::check_put;
        let checked = TreeDigest::from_hex(sha256)
            .ok_or(Refusal::Digest)
            .and_then(|digest| check_put(key, path).map(|()| digest));
        let digest = match checked {
            Ok(digest) => digest,
            Err(why) => return Ok(refused(&why)),
        };
        let (cache, key, path) = (self.cache.clone(), key.to_owned(), path.to_owned());
        let put = async move { fill_socket::put::answer(&cache, &key, digest, size, &path).await };
        self.call(wait, put)
            .map_err(|missed| unasked(&missed, AskError::TimedOut))
    }

    fn place(&self, mount: &str, path: &str) -> Result<Place, String> {
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

    fn mounted(&self) -> Result<Vec<String>, String> {
        self.writer()
            .map_err(|missed| unasked(&missed, AskError::TimedOut).to_string())?;
        Ok(self
            .trees
            .as_ref()
            .map_or_else(Vec::new, TreeStore::mount_paths))
    }

    fn image(&self, id: &str) -> Result<PlacedTree, String> {
        let id = placement_id(id).ok_or_else(|| refused(&Refusal::Placement))?;
        self.writer()
            .map_err(|missed| unasked(&missed, AskError::TimedOut).to_string())?;
        let trees = self.trees.as_ref().ok_or("none")?;
        match trees.mount_tree(&id) {
            Ok(MountTree::Image { tree, paths }) => Ok(PlacedTree::Image { tree, paths }),
            Ok(MountTree::Flat(flat)) => Ok(PlacedTree::Flat(flat)),
            Err(why) => Err(fill_socket::mount::refusal(&why).to_owned()),
        }
    }

    fn span(
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

    /// Ends the hold, once the cache is read-write; nothing is lost if it does not become so.
    fn unplace(&self, mount: &str, id: &str) {
        if self.writer().is_err() {
            return;
        }
        if let (Some(trees), Some(id)) = (&self.trees, placement_id(id)) {
            let _ = trees.detach(&id, mount);
        }
    }

    fn places_live(&self, mount: &str, ids: &[String]) -> Result<String, AskError> {
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

    /// Tells the cache at once, on the caller's thread.
    fn report(&self, report: Report) {
        match report {
            Report::Started(path) => {
                let _ = self.cache.run_started(&path);
            }
            Report::Attached(attach) => self.cache.tree_attached(attach),
            Report::Fetched(fetches) => self.cache.manifests_fetched(&fetches),
            Report::Descriptors(descriptors) => self.cache.descriptors_reported(descriptors),
        }
    }

    fn pause(&self, pause: Duration) {
        (self.pause)(pause);
    }
}

#[cfg(test)]
#[path = "in_process_tests.rs"]
mod tests;
