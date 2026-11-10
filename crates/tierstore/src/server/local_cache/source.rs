//! Fills asked for by the filesystem process over the fill socket (see
//! [`fill_socket`](crate::server::fill_socket)): a blob that lies inside a larger object of the
//! upstream, and a whole object named by its content.
//!
//! - [`LocalCache::fill_blob`] reads `size` bytes from `offset` of the upstream object `from`,
//!   hashes them as they pass, and keeps them under the reserved key `_tiering/fill/<sha256>` if
//!   they hash to `<sha256>`. The key is under a reserved prefix, so no front reads it for a
//!   client and no listing shows it; the filesystem process opens its file at the path the layout
//!   gives that key ([`blob_path`]).
//! - [`LocalCache::fill_manifest`] fills the object `cas/l/<sha256>`, up to
//!   [`WHOLE_FILL_LIMIT`], the file a tree's manifest is: the caller knows its hash and not its
//!   size.
//! - [`LocalCache::touch_blobs`] counts blobs as used now, for the cache's order of eviction.
//!
//! These fills have a pool of permits of their own (`fill_socket_concurrency`) and wait for one
//! up to `fill_socket_wait`: a request that finds none free is answered [`FillError::Busy`]. They
//! never take a permit of `max_concurrent_fills`, which a miss of the S3 front takes without
//! waiting. Concurrent requests for one blob share one fetch, and every fetch is a task of its own,
//! so a caller that gives up does not stop it. A cache that does not hold the lock of its
//! directory fills nothing ([`FillError::ReadOnly`]).
//!
//! The fetch is the one of every fill (`fill::fetch_from`): chunks of `fill_chunk` bytes, a staging
//! file, the checks of the lock before each step, a create-only link into `obj/`.

use std::collections::hash_map::Entry;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::{BoxFuture, OptionFuture, Shared};
use tokio::sync::{OwnedSemaphorePermit, oneshot, watch};
use tokio::task::JoinHandle;

use crate::Error;

use super::fill::{Filled, Live, Read, Stop, tagged};
use super::layout::{Name, data_path, name_of};
use super::metrics::{FailedFill, bump};
use super::{Inner, LocalCache, Mode, WHOLE_FILL_LIMIT, locked};

/// The prefix of the keys the blobs are kept under: reserved, so that no client reads one.
const FILL_PREFIX: &str = "_tiering/fill/";

/// A blob that lies inside an object of the upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct Blob<'a> {
    /// The SHA-256 the bytes must have, as sixty-four lower-case hex digits.
    pub sha256: &'a str,
    /// The bytes of the blob, above 0.
    pub size: u64,
    /// The key of the upstream object the blob lies in.
    pub from: &'a str,
    /// Where in that object the blob starts. `offset + size` does not pass `u64::MAX`.
    pub offset: u64,
}

/// Why a fill asked for over the fill socket left nothing in the cache. The text is the answer
/// the socket gives.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::server) enum FillError {
    /// The upstream failed or has no such object; the number is the HTTP status that stands for
    /// the failure.
    #[error("upstream {0}")]
    Upstream(u16),
    /// The bytes do not hash to the digest they were asked under.
    #[error("mismatch")]
    Mismatch,
    /// The upstream gave fewer bytes than the blob has.
    #[error("short")]
    Short,
    /// The blob does not fit the budget, or would take the cache past its critical mark.
    #[error("no space")]
    NoSpace,
    /// A whole object is larger than a manifest may be.
    #[error("too large")]
    TooLarge,
    /// No permit of the fill socket's pool came free in time.
    #[error("busy")]
    Busy,
    /// This process does not hold the lock of the cache directory, or lost it during the fill.
    #[error("read-only")]
    ReadOnly,
    /// The cache directory could not be written.
    #[error("io")]
    Io,
    /// The object changed while it was read.
    #[error("changed")]
    Changed,
    /// The task of the fill panicked or was cancelled.
    #[error("aborted")]
    Aborted,
}

/// The HTTP status that stands for `error` of the upstream.
pub(super) fn status_of(error: &Error) -> u16 {
    match error {
        Error::NotFound(_) => 404,
        Error::Reserved(_) => 403,
        Error::InvalidKey(_) | Error::InvalidSegment(_) => 400,
        Error::Unsupported(_) => 501,
        Error::Unavailable(_) | Error::SlowDown(_) => 503,
        _ => 500,
    }
}

/// The key a blob of `sha256` is kept under.
fn blob_key(sha256: &str) -> String {
    format!("{FILL_PREFIX}{sha256}")
}

/// The path of the file of the blob `sha256` under the cache directory `root`, where the
/// filesystem process opens it once a fill has been answered `ok`.
#[cfg(test)]
pub(in crate::server) fn blob_path(root: &std::path::Path, sha256: &str) -> std::path::PathBuf {
    data_path(root, &name_of(&blob_key(sha256)).hex)
}

/// What a fill reads, owned, so that the task of the fill holds it.
enum Plan {
    /// The object `cas/l/<sha256>`, whole.
    Manifest,
    /// A blob inside another object.
    Blob {
        from: String,
        offset: u64,
        size: u64,
    },
}

/// A fill that runs for the socket: shared by every request for its blob.
pub(super) type Flight = Shared<BoxFuture<'static, Result<(), FillError>>>;

/// A fill's place in the table of the socket's fills: taken out when it is dropped, whether the
/// fill ended, panicked or was cancelled.
pub(super) struct Flying {
    pub(super) inner: Arc<Inner>,
    pub(super) hex: String,
}

impl Drop for Flying {
    fn drop(&mut self) {
        let (flights, gone) = {
            let flights = locked(&self.inner.flights);
            let gone = locked(&self.inner.source_flights).remove(&self.hex);
            // The mark of a delete goes with the last fill of the name (see `Flight`).
            if !flights.contains_key(&self.hex) {
                locked(&self.inner.invalidated).remove(&self.hex);
            }
            (flights, gone)
        };
        drop(flights);
        drop(gone);
    }
}

/// What a request for a fill is about.
struct Wanted {
    key: String,
    name: Name,
    sha256: String,
    plan: Plan,
}

impl Inner {
    /// The fill of `wanted`: at once if it is in the cache, else shared with the fill that runs
    /// for it, else started when a permit of the socket's pool comes free.
    async fn source_fill(self: &Arc<Self>, wanted: Wanted) -> Result<(), FillError> {
        if !self.read_write() {
            return Err(FillError::ReadOnly);
        }
        if self.present(&wanted.name).await {
            return Ok(());
        }
        let running = locked(&self.source_flights).get(&wanted.name.hex).cloned();
        if let Some(running) = running {
            return running.await;
        }
        let waiting = self.source_permits.clone().acquire_owned();
        let permit = match tokio::time::timeout(self.config.fill_socket_wait, waiting).await {
            Ok(Ok(permit)) => permit,
            _ => return Err(FillError::Busy),
        };
        let (flight, launch) = {
            let mut flights = locked(&self.source_flights);
            // A fill that ended while this request waited for its permit left the object in the
            // index: it is not filled again.
            if locked(&self.index).get(&wanted.name.hex).is_some() {
                return Ok(());
            }
            match flights.entry(wanted.name.hex.clone()) {
                Entry::Occupied(running) => (running.get().clone(), None),
                Entry::Vacant(slot) => {
                    let (sender, receiver) = oneshot::channel();
                    let flight = Self::flight_of(receiver);
                    let _ = slot.insert_entry(flight.clone());
                    (flight, Some((permit, sender)))
                }
            }
        };
        if let Some((permit, sender)) = launch {
            self.launch(wanted, permit, sender);
        }
        flight.await
    }

    /// The fill that waits for the task that `receiver` is to be given, and for its end.
    pub(super) fn flight_of(
        receiver: oneshot::Receiver<JoinHandle<Result<(), FillError>>>,
    ) -> Flight {
        let waiting = async move {
            let task = OptionFuture::from(receiver.await.ok()).await;
            task.and_then(Result::ok).unwrap_or(Err(FillError::Aborted))
        };
        waiting.boxed().shared()
    }

    /// Runs the fill on a task of its own, which holds `permit` until it ends, and gives the
    /// task to the fill that waits for it. The table of the socket's fills must not be locked by
    /// the caller: a task dropped before it ran drops its place in the table.
    fn launch(
        self: &Arc<Self>,
        wanted: Wanted,
        permit: OwnedSemaphorePermit,
        sender: oneshot::Sender<JoinHandle<Result<(), FillError>>>,
    ) {
        let inner = Arc::clone(self);
        let flying = Flying {
            inner: Arc::clone(self),
            hex: wanted.name.hex.clone(),
        };
        let task = tokio::spawn(async move {
            let _held = (permit, flying);
            inner.source_fetch(&wanted).await
        });
        // The receiver is in the fill that is in the table, and in the caller: it is there.
        let _ = sender.send(task);
    }

    /// Whether the object is in the cache, counted as used. An entry whose file is not there (an
    /// eviction removed it while a fill linked it) is dropped, so that the fill makes it again.
    async fn present(&self, name: &Name) -> bool {
        if locked(&self.index).get(&name.hex).is_none() {
            return false;
        }
        let file = data_path(&self.config.path, &name.hex);
        if tokio::fs::metadata(&file).await.is_err() {
            self.drop_entry(&name.hex);
            return false;
        }
        let _ = locked(&self.index).used(&name.hex, self.clock.now());
        true
    }

    /// Reads what `wanted` says into the cache.
    async fn source_fetch(self: &Arc<Self>, wanted: &Wanted) -> Result<(), FillError> {
        // Nothing reads the progress of these fills: the filesystem process opens the file once
        // the fill is over.
        let (live, _unread) = watch::channel(Live::Starting);
        let (key, name) = (&wanted.key, &wanted.name);
        let ended = match &wanted.plan {
            Plan::Blob { from, offset, size } => {
                let read = Read::Range {
                    from,
                    offset: *offset,
                    size: *size,
                    sha256: &wanted.sha256,
                };
                self.fetch_from(key, name, &live, &read).await
            }
            Plan::Manifest => {
                let meta = match self.upstream_head(key).await.and_then(|meta| {
                    tagged(key, &meta)?;
                    Ok(meta)
                }) {
                    Ok(meta) => meta,
                    Err(error) => return Err(self.upstream_failed(&error)),
                };
                if meta.size > WHOLE_FILL_LIMIT {
                    return Err(FillError::TooLarge);
                }
                self.fetch_from(key, name, &live, &Read::Headed(meta)).await
            }
        };
        match ended {
            Ok(Filled::Kept) => Ok(()),
            Ok(Filled::Skipped | Filled::Changed) => Err(FillError::NoSpace),
            Err(stop) => Err(self.stopped(stop)),
        }
    }

    /// Counts a failure of the upstream, as a fill does, and gives its answer.
    fn upstream_failed(&self, error: &Error) -> FillError {
        if !matches!(error, Error::NotFound(_)) {
            self.counters.failed(FailedFill::UpstreamError);
        }
        FillError::Upstream(status_of(error))
    }

    /// Counts a fill that stopped for `stop` as a fill that failed, and gives its answer.
    pub(super) fn stopped(&self, stop: Stop) -> FillError {
        match stop {
            Stop::Upstream(error) => self.upstream_failed(&error),
            Stop::Short => {
                self.counters.failed(FailedFill::ShortBody);
                FillError::Short
            }
            Stop::Local => {
                self.counters.failed(FailedFill::LocalIo);
                FillError::Io
            }
            Stop::Changed | Stop::Invalidated => {
                self.counters.failed(FailedFill::Changed);
                FillError::Changed
            }
            Stop::Lost => {
                bump(&self.counters.fills_skipped);
                FillError::ReadOnly
            }
            Stop::Mismatch => {
                self.counters.failed(FailedFill::HashMismatch);
                FillError::Mismatch
            }
        }
    }
}

impl LocalCache {
    /// Keeps the blob `blob` under `_tiering/fill/<sha256>`: read from the upstream, hashed on
    /// its way to the disk, and linked into the cache only if it hashes to `blob.sha256`.
    ///
    /// # Errors
    /// Why nothing was kept: see [`FillError`].
    pub(in crate::server) async fn fill_blob(&self, blob: &Blob<'_>) -> Result<(), FillError> {
        let inner = &self.inner;
        if blob.size > inner.config.budget {
            return Err(FillError::NoSpace);
        }
        let key = blob_key(blob.sha256);
        let plan = Plan::Blob {
            from: blob.from.to_owned(),
            offset: blob.offset,
            size: blob.size,
        };
        let name = name_of(&key);
        let wanted = Wanted {
            key,
            name,
            sha256: blob.sha256.to_owned(),
            plan,
        };
        inner.source_fill(wanted).await
    }

    /// Keeps the object `cas/l/<sha256>`, whole, if it is at most [`WHOLE_FILL_LIMIT`] bytes.
    ///
    /// # Errors
    /// Why nothing was kept: see [`FillError`].
    pub(in crate::server) async fn fill_manifest(&self, sha256: &str) -> Result<(), FillError> {
        let key = format!("cas/l/{sha256}");
        let name = name_of(&key);
        let wanted = Wanted {
            key,
            name,
            sha256: sha256.to_owned(),
            plan: Plan::Manifest,
        };
        self.inner.source_fill(wanted).await
    }

    /// Counts the blobs and whole objects of these digests as used now, for the order of
    /// eviction. A digest the cache does not hold is passed over; so is every one in a cache
    /// that is read-only.
    pub(in crate::server) fn touch_blobs<'a>(&self, sha256s: impl IntoIterator<Item = &'a str>) {
        let inner = &self.inner;
        if !inner.read_write() {
            return;
        }
        let now = inner.clock.now();
        let mut index = locked(&inner.index);
        for sha256 in sha256s {
            let _ = index.used(sha256, now);
            let _ = index.used(&name_of(&blob_key(sha256)).hex, now);
        }
    }

    /// The mode of the cache now, and each time it changes: the fill socket is bound while the
    /// cache is read-write and only then.
    pub(in crate::server) fn watch_mode(&self) -> watch::Receiver<Mode> {
        self.inner.hold.subscribe()
    }
}
