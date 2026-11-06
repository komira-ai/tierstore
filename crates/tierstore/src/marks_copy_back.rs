//! The copy of a cold object into the hot tier over markers (`docs/design/put-without-lock.md`,
//! 5.11): the ask a cold read makes, and the leader's work on the asks.
//!
//! - [`WantRequests`] is the read side. A cold hit with no marker may create a `WANT` marker (M10)
//!   in the background, best effort, and it is bounded: only when the format is `active-marks`,
//!   only below the level `Critical`, only for a whole-object read of at most
//!   `copy_back_max_bytes`, and only if this proxy has not asked for the key within `want_retry`
//!   (a bounded in-memory set of `want_recent_limit` keys, no store request). A cold-only key read
//!   a thousand times a second makes one create a minute, not a thousand a second.
//! - [`CopyBacker`] is the leader's side. For each `WANT` marker it was given: CAS to
//!   `LOCK(COPYBACK, dirty = false)` (M11); a hot HEAD (present: done); the cold `open_read`; a
//!   create-only streamed write into hot, fed through the lease, the lock written again every
//!   `lock_refresh` while the stream runs (M14); then `CLEARING` and removed (M6, M8). A hot
//!   write answered 412 (a newer put or another copy-back created it) ends in `DIRTY` (M5), and
//!   the copier reconciles cold with what hot holds. A copy-back that is too old (`copy_back_ttl`
//!   on the leader's monotonic clock from the pass that first saw the marker), beyond the oldest
//!   `copy_back_max`, or for an object above `copy_back_max_bytes` is dropped: the lock is taken
//!   and released at once, so the marker goes through the rows of the table and no row is added.
//!   A hot write whose outcome is unknown keeps the lock, because the write may still land and a
//!   release would let a delete of the key be undone by it; the leader's reap ends the lock
//!   (5.13). A failure before the write releases it.
//!
//! A pass reads the format object once and does nothing unless it says `active-marks`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{self, StreamExt};

use crate::deadline::millis;
use crate::format::marks_active;
use crate::marks_leader::{LeaderLease, release, renewed};
use crate::{
    Backend, Clock, Creation, Error, MarkBirth, MarkMove, MarkSeen, MarkerState, MarkerWhy,
    Markers, Opened, Outcome, Written,
};

/// The settings of the copy-back: `want_retry`, `want_recent_limit`, `copy_back_max_bytes`,
/// `copy_back_ttl` and `copy_back_max` of the design (section 9), with the defaults it gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyBackConfig {
    /// A proxy asks for the copy-back of a key at most once in this long (60 s).
    pub want_retry: Duration,
    /// The most keys a proxy remembers asking for (100 000); the oldest are forgotten.
    pub want_recent_limit: usize,
    /// An object larger than this is not copied back (64 MiB): the lock is held for the whole
    /// stream, and a writer of the key waits for it.
    pub copy_back_max_bytes: u64,
    /// How long an ask may wait, on the leader's monotonic clock from the pass that first saw it,
    /// before it is dropped without a copy (600 s).
    pub copy_back_ttl: Duration,
    /// The most asks that are copied: the oldest this many, the rest dropped (1000).
    pub copy_back_max: usize,
}

impl Default for CopyBackConfig {
    fn default() -> Self {
        Self {
            want_retry: Duration::from_secs(60),
            want_recent_limit: 100_000,
            copy_back_max_bytes: 64 << 20,
            copy_back_ttl: Duration::from_secs(600),
            copy_back_max: 1000,
        }
    }
}

/// A cold read that found its object, as the proxy tells [`WantRequests::ask`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColdRead {
    /// The size of the object read, in bytes. Only a read of the whole object asks.
    pub size: u64,
    /// Whether the hot tier is at its critical level.
    pub critical: bool,
    /// Whether the proxy's format says `active-marks`.
    pub active: bool,
}

/// Why a cold read asked for nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The format is not `active-marks`: no proxy writes a marker.
    Inactive,
    /// The hot tier is at its critical level.
    Critical,
    /// The object is larger than `copy_back_max_bytes`.
    TooBig,
    /// This proxy asked for the key within `want_retry`.
    Recent,
}

/// What an ask came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Wanted {
    /// The `WANT` marker was created.
    Made,
    /// A marker of the key exists already: nothing was written.
    Exists,
    /// The store failed the create. The read ignores it.
    Failed(Error),
    /// Nothing was asked, and no request was made.
    Skipped(Skip),
}

/// The asks of one proxy for the copy-back of a key.
pub struct WantRequests {
    config: CopyBackConfig,
    mono: Arc<dyn Clock>,
    asked: Mutex<Asked>,
}

#[derive(Default)]
struct Asked {
    next: u64,
    by_key: BTreeMap<String, (u64, u64)>,
    by_age: BTreeMap<u64, String>,
}

impl Asked {
    fn forget(&mut self, key: &str) {
        if let Some((seq, _)) = self.by_key.remove(key) {
            self.by_age.remove(&seq);
        }
    }
}

impl WantRequests {
    /// The asks of a proxy that times them on `mono`, a monotonic clock.
    #[must_use]
    pub fn new(config: CopyBackConfig, mono: Arc<dyn Clock>) -> Self {
        Self {
            config,
            mono,
            asked: Mutex::new(Asked::default()),
        }
    }

    fn asked(&self) -> MutexGuard<'_, Asked> {
        // No method panics while holding the lock, so a poisoned state is still whole.
        self.asked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `key` may be asked for now, noting that it is when it may.
    fn allow(&self, key: &str) -> bool {
        let now = self.mono.now();
        let retry = millis(self.config.want_retry);
        let mut asked = self.asked();
        let recent = asked
            .by_key
            .get(key)
            .is_some_and(|(_, at)| now.saturating_sub(*at) < retry);
        if recent {
            return false;
        }
        asked.forget(key);
        asked.next += 1;
        let seq = asked.next;
        asked.by_age.insert(seq, key.to_owned());
        asked.by_key.insert(key.to_owned(), (seq, now));
        let excess = asked
            .by_key
            .len()
            .saturating_sub(self.config.want_recent_limit.max(1));
        let oldest: Vec<String> = asked.by_age.values().take(excess).cloned().collect();
        for key in &oldest {
            asked.forget(key);
        }
        true
    }

    /// Asks for the copy-back of `key`, which `read` found in the cold tier and, the proxy having
    /// read its marker, not under a marker: a create-only `WANT` (M10). The caller runs it in a
    /// task of its own and ignores the answer; the read never waits for it.
    pub async fn ask(&self, markers: &Markers, key: &str, read: ColdRead) -> Wanted {
        if !read.active {
            return Wanted::Skipped(Skip::Inactive);
        }
        if read.critical {
            return Wanted::Skipped(Skip::Critical);
        }
        if read.size > self.config.copy_back_max_bytes {
            return Wanted::Skipped(Skip::TooBig);
        }
        if !self.allow(key) {
            return Wanted::Skipped(Skip::Recent);
        }
        match markers.create(key, MarkBirth::Want).await {
            Ok(Creation::Made(_)) => Wanted::Made,
            Ok(Creation::Exists) => Wanted::Exists,
            Err(error) => Wanted::Failed(error),
        }
    }
}

/// What became of one `WANT` marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopiedBack {
    /// The object was written into the hot tier: its size.
    Landed(u64),
    /// Dropped without a copy: too old, beyond the maximum, or too large.
    Dropped,
    /// There was nothing to copy: hot has the key, cold has not, or hot is full.
    Nothing,
    /// A newer put or another copy-back created the hot object first: the key is `DIRTY`.
    Raced,
    /// The marker was not a `WANT` any more (a put, a delete or a reap wrote it), or the lock was
    /// taken from the copy-back before the write.
    Lost,
}

/// What one [`CopyBacker::pass`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CopyBackReport {
    /// Whether the format said `active-marks`. If not, nothing else was done.
    pub active: bool,
    /// The objects copied into the hot tier.
    pub landed: usize,
    /// The asks dropped without a copy.
    pub dropped: usize,
    /// The asks with nothing to copy.
    pub nothing: usize,
    /// The asks that met a hot object made first.
    pub raced: usize,
    /// The asks whose marker had moved.
    pub lost: usize,
    /// The keys that failed, with the error.
    pub failed: Vec<(String, Error)>,
}

/// What the leader finds to copy for a key.
enum Source {
    /// The cold object, opened.
    Open(Opened),
    /// Nothing to write: the answer for the key.
    Skip(CopiedBack),
}

/// The leader's copy-back of cold objects into the hot tier over markers.
pub struct CopyBacker {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    markers: Markers,
    lease: LeaderLease,
    mono: Arc<dyn Clock>,
    config: CopyBackConfig,
    /// The nonce of each open `WANT` and when a pass first saw it: a new marker of the key, by its
    /// nonce, starts again.
    first_seen: BTreeMap<String, (String, u64)>,
    /// The keys this pass drops without a copy.
    drops: BTreeSet<String>,
}

impl CopyBacker {
    /// The copy-back of the leader whose markers are `markers`, over `hot` and `cold`.
    #[must_use]
    pub fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        markers: Markers,
        lease: LeaderLease,
        mono: Arc<dyn Clock>,
        config: CopyBackConfig,
    ) -> Self {
        Self {
            hot,
            cold,
            markers,
            lease,
            mono,
            config,
            first_seen: BTreeMap::new(),
            drops: BTreeSet::new(),
        }
    }

    /// Forgets when each ask was first seen: the leader took over, or lost the lease.
    pub fn took_over(&mut self) {
        self.first_seen.clear();
        self.drops.clear();
    }

    /// Works on `markers`, all the markers the discovery found in this round (those that are not
    /// `WANT` are ignored), at most `parallel` at a time (at least one), if the format says
    /// `active-marks`. The oldest `copy_back_max` asks that are younger than `copy_back_ttl` are
    /// copied; the others are dropped.
    pub async fn pass(&mut self, markers: Vec<MarkSeen>, parallel: usize) -> CopyBackReport {
        let mut report = CopyBackReport::default();
        match marks_active(self.hot.as_ref()).await {
            Ok(true) => report.active = true,
            Ok(false) => return report,
            Err(error) => {
                report.failed = markers
                    .into_iter()
                    .map(|seen| (seen.marker.key, error.clone()))
                    .collect();
                return report;
            }
        }
        let wants: Vec<MarkSeen> = markers
            .into_iter()
            .filter(|seen| seen.marker.st == MarkerState::Want)
            .collect();
        self.note(&wants);
        let this = &*self;
        let tries = stream::iter(wants).map(|seen| async move {
            let result = this.copy_back_one(&seen).await;
            (seen.marker.key, result)
        });
        let results: Vec<_> = tries.buffer_unordered(parallel.max(1)).collect().await;
        for (key, result) in results {
            match result {
                Ok(CopiedBack::Landed(_)) => report.landed += 1,
                Ok(CopiedBack::Dropped) => report.dropped += 1,
                Ok(CopiedBack::Nothing) => report.nothing += 1,
                Ok(CopiedBack::Raced) => report.raced += 1,
                Ok(CopiedBack::Lost) => report.lost += 1,
                Err(error) => report.failed.push((key, error)),
            }
        }
        report
    }

    /// Notes when each ask in `wants` was first seen, and which this pass drops (too old, or not
    /// among the oldest `copy_back_max`).
    fn note(&mut self, wants: &[MarkSeen]) {
        let now = self.mono.now();
        let open: BTreeSet<&String> = wants.iter().map(|seen| &seen.marker.key).collect();
        self.first_seen.retain(|key, _| open.contains(key));
        for seen in wants {
            let marker = &seen.marker;
            let known = self
                .first_seen
                .get(&marker.key)
                .is_some_and(|(nonce, _)| *nonce == marker.nonce);
            if !known {
                self.first_seen
                    .insert(marker.key.clone(), (marker.nonce.clone(), now));
            }
        }
        let ttl = millis(self.config.copy_back_ttl);
        let mut by_age: Vec<(u64, &String)> = self
            .first_seen
            .iter()
            .map(|(key, (_, first))| (*first, key))
            .collect();
        by_age.sort();
        self.drops = by_age
            .iter()
            .enumerate()
            .filter(|(rank, (first, _))| {
                *rank >= self.config.copy_back_max || now.saturating_sub(*first) >= ttl
            })
            .map(|(_, (_, key))| (*key).clone())
            .collect();
    }

    /// The copy-back of the key of the `WANT` marker `seen`, by the rules of the module.
    async fn copy_back_one(&self, seen: &MarkSeen) -> Result<CopiedBack, Error> {
        let key = seen.marker.key.as_str();
        let held = match self
            .markers
            .advance(seen, MarkMove::Lock(MarkerWhy::CopyBack))
            .await?
        {
            Outcome::Done(held) => held,
            Outcome::Lost | Outcome::Refused => return Ok(CopiedBack::Lost),
        };
        if self.drops.contains(key) {
            release(&self.markers, &held, false).await?;
            return Ok(CopiedBack::Dropped);
        }
        let opened = match self.source(key).await {
            Ok(Source::Open(opened)) => opened,
            Ok(Source::Skip(skipped)) => {
                release(&self.markers, &held, false).await?;
                return Ok(skipped);
            }
            Err(error) => return Err(self.failed(&held, error).await),
        };
        let Some(held) = renewed(&self.markers, held).await? else {
            return Ok(CopiedBack::Lost);
        };
        if let Err(error) = self.lease.require().await {
            return Err(self.failed(&held, error).await);
        }
        let size = opened.meta.size;
        let held = Mutex::new(held);
        let body = self
            .lease
            .while_held(opened.body)
            .then(|chunk| self.keep(chunk, &held))
            .boxed();
        let landed = self.hot.put_stream(key, Some(size), body).await;
        let held = held.into_inner().unwrap_or_else(PoisonError::into_inner);
        match landed {
            Ok(()) => {
                release(&self.markers, &held, false).await?;
                Ok(CopiedBack::Landed(size))
            }
            Err(Error::AlreadyExists(_)) => {
                release(&self.markers, &held, true).await?;
                Ok(CopiedBack::Raced)
            }
            Err(Error::Full(_)) => {
                release(&self.markers, &held, false).await?;
                Ok(CopiedBack::Nothing)
            }
            // The write may still land: the lock stays until the reap.
            Err(error) => Err(error),
        }
    }

    /// The cold object to copy, opened, or why there is nothing to copy: the hot tier has the key,
    /// the cold tier has not, or the object is larger than `copy_back_max_bytes`.
    async fn source(&self, key: &str) -> Result<Source, Error> {
        match self.hot.head(key).await {
            Ok(_) => return Ok(Source::Skip(CopiedBack::Nothing)),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match self.cold.open_read(key, None).await {
            Ok(opened) if opened.meta.size > self.config.copy_back_max_bytes => {
                Ok(Source::Skip(CopiedBack::Dropped))
            }
            Ok(opened) => Ok(Source::Open(opened)),
            Err(Error::NotFound(_)) => Ok(Source::Skip(CopiedBack::Nothing)),
            Err(error) => Err(error),
        }
    }

    /// Passes a chunk of the stream on, after writing the lock again if `lock_refresh` has passed
    /// since it was last written (M14). A lock that was taken away ends the stream with an error,
    /// and nothing lands.
    async fn keep(
        &self,
        chunk: Result<Bytes, Error>,
        held: &Mutex<Written>,
    ) -> Result<Bytes, Error> {
        let chunk = chunk?;
        let current = held.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if self.markers.refresh_due(&current) {
            match self
                .markers
                .advance(&current.seen, MarkMove::Refresh)
                .await?
            {
                Outcome::Done(written) => {
                    *held.lock().unwrap_or_else(PoisonError::into_inner) = written;
                }
                Outcome::Lost | Outcome::Refused => {
                    return Err(Error::Unavailable(
                        "the lock of the copy-back was taken away".to_owned(),
                    ));
                }
            }
        }
        Ok(chunk)
    }

    /// `error`, after the lock `held` was released as nothing was written; a release that fails
    /// too leaves the lock to the reap, and `error` is the one reported.
    async fn failed(&self, held: &Written, error: Error) -> Error {
        let _ = release(&self.markers, held, false).await;
        error
    }
}
