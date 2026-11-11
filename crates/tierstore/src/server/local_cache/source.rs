//! Fills asked for by the filesystem process over the fill socket (see
//! [`fill_socket`](crate::server::fill_socket)): a blob that lies inside a larger object of the
//! upstream, and a whole object named by its content.
//!
//! - [`LocalCache::fill_blob`] reads `size` bytes from `offset` of the upstream object `from`,
//!   hashes them as they pass, and keeps them as the entry named by `<sha256>` (the name of the
//!   object `cas/l/<sha256>`) if they hash to `<sha256>`. The entry is the same one the S3 front
//!   keeps for that object: one entry and one fill for a digest, whichever path asks. A blob cut
//!   out of a larger object is of origin `span`, which the S3 front does not serve as an object;
//!   the filesystem process opens its file at the path the layout gives the name ([`blob_path`]).
//! - [`LocalCache::fill_manifest`] fills the object `cas/l/<sha256>`, up to
//!   [`WHOLE_FILL_LIMIT`], the file a tree's manifest is: the caller knows its hash and not its
//!   size.
//! - [`LocalCache::touch_blobs`] counts blobs as used now, for the cache's order of eviction.
//!
//! These fills have a pool of permits of their own (`fill_socket_concurrency`) and wait for one
//! up to `fill_socket_wait`: a request that finds none free is answered [`FillError::Busy`]. They
//! never take a permit of `max_concurrent_fills`, which a miss of the S3 front takes without
//! waiting. A request for a digest that has a fill running, started by this socket, by the S3
//! front or by a worker of the prefetch queue, joins it and takes no permit; one for a digest that
//! is only queued starts it, on a permit of this socket's pool. Every fetch is a task of its own, so a caller that gives
//! up does not stop it. A cache that does not hold the lock of its
//! directory fills nothing ([`FillError::ReadOnly`]).
//!
//! The fetch is the one of every fill (`fill::fetch_from`): chunks of `fill_chunk` bytes, a staging
//! file, the checks of the lock before each step, a create-only link into `obj/`.

use std::sync::Arc;

use tokio::sync::watch;

use crate::Error;

use super::fill::{Class, Ended, Filled, Pending, Plan, Stop};
use super::layout::{Name, Origin, data_path, name_of};
use super::{Inner, LocalCache, Mode, locked};

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

/// The key of the object the entry named by `sha256` stands for.
pub(super) fn object_key(sha256: &str) -> String {
    format!("cas/l/{sha256}")
}

/// The path of the file of the blob `sha256` under the cache directory `root`, where the
/// filesystem process opens it once a fill has been answered `ok`: the file of the entry named by
/// the digest, the same one a whole object `cas/l/<sha256>` has.
pub(in crate::server) fn blob_path(root: &std::path::Path, sha256: &str) -> std::path::PathBuf {
    data_path(root, sha256)
}

/// The path of the file of the object `cas/l/<sha256>` under the cache directory `root`, where the
/// filesystem process reads a tree manifest once its fill has been answered `ok`.
pub(in crate::server) fn manifest_path(root: &std::path::Path, sha256: &str) -> std::path::PathBuf {
    data_path(root, sha256)
}

/// What the fill socket's request is about.
struct Wanted {
    key: String,
    name: Name,
    plan: Plan,
}

/// The answer of the socket to a fill that ended as `ended`.
pub(super) fn answer(ended: Ended) -> Result<(), FillError> {
    match ended {
        Ok(Filled::Kept) => Ok(()),
        Ok(Filled::Skipped | Filled::Changed) => Err(FillError::NoSpace),
        Err(Stop::Upstream(error)) => Err(FillError::Upstream(status_of(&error))),
        Err(Stop::Short) => Err(FillError::Short),
        Err(Stop::Local) => Err(FillError::Io),
        Err(Stop::Changed | Stop::Invalidated) => Err(FillError::Changed),
        Err(Stop::Lost) => Err(FillError::ReadOnly),
        Err(Stop::Mismatch) => Err(FillError::Mismatch),
        Err(Stop::TooLarge) => Err(FillError::TooLarge),
        Err(Stop::Aborted) => Err(FillError::Aborted),
    }
}

/// The answer of the socket to the fill `pending`, once it ends; none if it is the place of a keep
/// that has left the table, and the request is to look again.
async fn waited(pending: Pending) -> Option<Result<(), FillError>> {
    if pending.keeping {
        pending.released().await;
        None
    } else {
        Some(answer(pending.ended().await))
    }
}

impl Inner {
    /// The fill of `wanted`: at once if it is in the cache, else shared with the fill that runs
    /// for its name (whichever path started it), else started when a permit of the socket's pool
    /// comes free.
    async fn source_fill(self: &Arc<Self>, wanted: Wanted) -> Result<(), FillError> {
        if !self.read_write() {
            return Err(FillError::ReadOnly);
        }
        loop {
            if self.present(&wanted.name).await {
                return Ok(());
            }
            // A fill that is only queued is not joined: this request starts it (`join_or_start`
            // takes it out of the queue), on a permit of its own pool.
            let pending = match self.joinable(&wanted.name.hex) {
                Some(running) => {
                    self.counters.joined(false);
                    running
                }
                None => {
                    let waiting = self.source_permits.clone().acquire_owned();
                    let waited = tokio::time::timeout(self.config.fill_socket_wait, waiting).await;
                    let Ok(Ok(permit)) = waited else {
                        return Err(FillError::Busy);
                    };
                    let class = Class::Mount(permit);
                    match self.join_or_start(&wanted.key, &wanted.name, wanted.plan.clone(), class)
                    {
                        Ok(pending) => pending,
                        // A fill that ended while this request waited for its permit left the
                        // object in the index: it is not filled again. (A request of this class
                        // is refused only so: the S3 class is the one that is turned away.)
                        Err(_) => return Ok(()),
                    }
                }
            };
            // The place of a client's put (a keep) ends within the keep's deadline, and leaves
            // the object in the cache or not; either way this request looks again.
            if let Some(answered) = waited(pending).await {
                return answered;
            }
        }
    }

    /// The on-disk name of `key` as the S3 front serves it: none if the cache does not keep the
    /// key, or if the entry of that name is a `span` cut out of a pack, which is no object of the
    /// upstream (the client decides by an object's presence whether it has to be written, so a
    /// blob that is only a span must not look written). The read goes to the upstream instead.
    pub(super) fn served_name(&self, key: &str) -> Option<Name> {
        let span = |name: &Name| {
            let index = locked(&self.index);
            index
                .get(&name.hex)
                .is_some_and(|entry| entry.origin == Origin::Span)
        };
        self.cacheable(key).filter(|name| !span(name))
    }

    /// Whether the object is in the cache, counted as used. An entry whose file is not there (an
    /// eviction removed it while a fill linked it) or is not the size recorded for it (a power cut
    /// left it short) is dropped, so that the fill makes it again.
    async fn present(&self, name: &Name) -> bool {
        let Some(size) = locked(&self.index).get(&name.hex).map(|entry| entry.size) else {
            return false;
        };
        // The size of the file is the size recorded when it was kept: the filesystem process
        // opens the file as it is, so a file a power cut left short must not be answered `ok`.
        let file = data_path(&self.config.path, &name.hex);
        let whole = tokio::fs::metadata(&file)
            .await
            .is_ok_and(|meta| meta.len() == size);
        if !whole {
            self.drop_entry(&name.hex);
            return false;
        }
        let _ = locked(&self.index).used(&name.hex, self.clock.now());
        true
    }
}

impl LocalCache {
    /// Keeps the blob `blob` as the entry named by `<sha256>`: read from the upstream, hashed on
    /// its way to the disk, and linked into the cache only if it hashes to `blob.sha256`.
    ///
    /// # Errors
    /// Why nothing was kept: see [`FillError`].
    pub(in crate::server) async fn fill_blob(&self, blob: &Blob<'_>) -> Result<(), FillError> {
        let inner = &self.inner;
        if blob.size > inner.config.budget {
            return Err(FillError::NoSpace);
        }
        let key = object_key(blob.sha256);
        let plan = Plan::Blob {
            from: blob.from.to_owned(),
            offset: blob.offset,
            size: blob.size,
        };
        let name = name_of(&key);
        let wanted = Wanted { key, name, plan };
        inner.source_fill(wanted).await
    }

    /// Keeps the object `cas/l/<sha256>`, whole, if it is at most [`WHOLE_FILL_LIMIT`] bytes.
    ///
    /// # Errors
    /// Why nothing was kept: see [`FillError`].
    pub(in crate::server) async fn fill_manifest(&self, sha256: &str) -> Result<(), FillError> {
        let key = object_key(sha256);
        let name = name_of(&key);
        let wanted = Wanted {
            key,
            name,
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
        }
    }

    /// The mode of the cache now, and each time it changes: the fill socket is bound while the
    /// cache is read-write and only then.
    pub(in crate::server) fn watch_mode(&self) -> watch::Receiver<Mode> {
        self.inner.hold.subscribe()
    }
}
