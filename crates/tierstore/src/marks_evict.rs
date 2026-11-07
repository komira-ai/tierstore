//! The eviction of the hot tier's objects over markers (`docs/design/put-without-lock.md`, 5.10):
//! what the leader does to one candidate key, and the pass that does it for a batch of them.
//!
//! The candidates (the oldest objects of the usage listing) and the water marks are the usage
//! books' (5.14); this module is given keys and answers what became of each. For one key:
//!
//! 1. Hot HEAD and cold HEAD. No hot object: nothing to evict. A cold copy younger than
//!    `evict_min_age` on the leader's monotonic clock: left ([`Evicted::Young`]); the age counts
//!    from the copier's cold write, recorded with the pair of tags ([`CopyRecords`]), and a pair
//!    with no record counts as copied at takeover. Unless the record has both current tags, the
//!    bytes are compared in ranges, outside any lock (a compare reads two big objects and must not
//!    stall a writer).
//! 2. Create-only `LOCK(EVICT)` (M3). Any marker at all: the key is left ([`Evicted::Kept`]).
//! 3. Under the lock, a cold HEAD. Cold absent, a tag that is not the one the compare used, or a
//!    compare that found other bytes: release as `DIRTY` and keep hot ([`Evicted::Dirtied`]); hot
//!    is the only place a newer version can be, and the copier will copy it.
//! 4. Equal: the hot object is deleted on the tag the first HEAD gave (`delete_if_match`: on a
//!    store that enforces `If-Match` on a DELETE one request, on the default S3 backend the
//!    backend's HEAD, comparison and DELETE, on the memory and directory backends one atomic
//!    step), after the lock has been written again if it is older than `lock_lease -
//!    queued_call_margin` (5.2) and with the margin of the leader's lease left. That is the design's
//!    hot HEAD under the lock and its DELETE in one call; a tag that differs there is the same
//!    `DIRTY` as in step 3. A call whose outcome is unknown keeps the lock, which the leader's
//!    reap ends by the lease (5.13).
//! 5. Release (M6, M8): `CLEARING`, then the marker is removed.
//!
//! A pass reads the format object once and does nothing unless it says `active-marks` at epoch 3
//! or later and names the marks bucket the evictor was given (the condition of a put and of the
//! copier).

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::stream::{self, StreamExt};

use crate::backend::ByteRange;
use crate::deadline::millis;
use crate::entry::{ambiguous, found};
use crate::format::marks_run_in;
use crate::marks_leader::{LeaderLease, release, renewed};
use crate::{Backend, Clock, Creation, Error, MarkBirth, MarkerWhy, Markers, Meta, Written};

/// The settings of the eviction: `evict_min_age`, `tag_record_limit` and the range of the compare
/// of the design (section 9), with the defaults it gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvictConfig {
    /// A hot object whose cold copy was written less than this long ago is not evicted (600 s).
    pub evict_min_age: Duration,
    /// How many keys' pairs of tags the leader remembers (100 000); the oldest are forgotten.
    pub tag_record_limit: usize,
    /// The most bytes of one object that a compare reads at once (1 MiB).
    pub compare_range: NonZeroU64,
}

impl Default for EvictConfig {
    fn default() -> Self {
        Self {
            evict_min_age: Duration::from_secs(600),
            tag_record_limit: 100_000,
            compare_range: crate::reconciler::COMPARE_RANGE,
        }
    }
}

/// The pair of tags of a key's hot and cold object that the leader copied or compared, and when
/// the cold object was written, on the leader's monotonic clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRecord {
    /// The content tag of the hot object.
    pub hot: String,
    /// The content tag of the cold object.
    pub cold: String,
    /// The reading of the leader's monotonic clock at the cold write.
    pub copied_at: u64,
}

/// The records of the copies this leader made, newest kept, at most a limit of keys: the memory of
/// the fast path of the eviction and of the sweep. The copier writes it (5.9) and the eviction and
/// the sweep read it; a new leader starts it empty.
#[derive(Default)]
pub struct CopyRecords {
    inner: Mutex<Records>,
}

#[derive(Default)]
struct Records {
    next: u64,
    by_key: BTreeMap<String, (u64, CopyRecord)>,
    by_age: BTreeMap<u64, String>,
}

impl CopyRecords {
    fn lock(&self) -> MutexGuard<'_, Records> {
        // No method panics while holding the lock, so a poisoned state is still whole.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records that `key`'s hot object had the tag `hot` and its cold object the tag `cold`, which
    /// was written at `copied_at`, and forgets the oldest records beyond `limit` keys (at least
    /// one).
    pub fn record(&self, key: &str, hot: &str, cold: &str, copied_at: u64, limit: usize) {
        let mut records = self.lock();
        records.forget(key);
        records.next += 1;
        let seq = records.next;
        records.by_age.insert(seq, key.to_owned());
        let record = CopyRecord {
            hot: hot.to_owned(),
            cold: cold.to_owned(),
            copied_at,
        };
        records.by_key.insert(key.to_owned(), (seq, record));
        let excess = records.by_key.len().saturating_sub(limit.max(1));
        let oldest: Vec<String> = records.by_age.values().take(excess).cloned().collect();
        for key in &oldest {
            records.forget(key);
        }
    }

    /// The record of `key`, if there is one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<CopyRecord> {
        self.lock()
            .by_key
            .get(key)
            .map(|(_, record)| record.clone())
    }

    /// When the cold object was written, if the record of `key` has exactly these two tags.
    #[must_use]
    pub fn copied_at(&self, key: &str, hot: &str, cold: &str) -> Option<u64> {
        self.get(key)
            .filter(|record| record.hot == hot && record.cold == cold)
            .map(|record| record.copied_at)
    }

    /// Drops the record of `key`.
    pub fn forget(&self, key: &str) {
        self.lock().forget(key);
    }

    /// Drops every record.
    pub fn clear(&self) {
        *self.lock() = Records::default();
    }
}

impl Records {
    fn forget(&mut self, key: &str) {
        if let Some((seq, _)) = self.by_key.remove(key) {
            self.by_age.remove(&seq);
        }
    }
}

/// What became of one candidate of the eviction.
#[derive(Debug, PartialEq, Eq)]
pub enum Evicted {
    /// The hot object was deleted and the marker removed: the bytes freed. `unreleased` is the
    /// error of the release when it failed after the delete: the lock stays and the leader's reap
    /// ends it, and nothing is owed.
    Removed {
        /// The size of the hot object that was deleted.
        freed: u64,
        /// Why the lock could not be released, if it could not.
        unreleased: Option<Error>,
    },
    /// The cold copy is younger than `evict_min_age`; nothing was done.
    Young,
    /// The key has a marker (an operation or a copy is under way), or the lock was lost to the
    /// leader's reap before the delete; nothing was destroyed.
    Kept,
    /// The hot object is gone already.
    Gone,
    /// Hot is not known to equal cold (no cold copy, another tag, other bytes): the key is marked
    /// `DIRTY` for the copier and the hot object is kept.
    Dirtied,
}

/// What one [`MarkEvictor::evict_pass`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EvictReport {
    /// Whether the format opened writes on markers in the evictor's marks bucket. If not, nothing
    /// else was done.
    pub active: bool,
    /// The bytes of the hot objects deleted.
    pub freed: u64,
    /// The hot objects deleted.
    pub evicted: usize,
    /// The keys marked `DIRTY` and kept.
    pub dirtied: usize,
    /// The keys left because their cold copy is young.
    pub young: usize,
    /// The keys left because of a marker.
    pub kept: usize,
    /// The keys whose hot object was gone.
    pub gone: usize,
    /// The errors of the releases of locks whose delete had succeeded.
    pub unreleased: Vec<Error>,
    /// The keys whose eviction failed, with the error, in the order they failed: the caller puts
    /// them back among its candidates.
    pub failed: Vec<(String, Error)>,
}

/// The leader's eviction of the hot tier's objects over markers.
pub struct MarkEvictor {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    markers: Markers,
    bucket: String,
    lease: LeaderLease,
    records: Arc<CopyRecords>,
    mono: Arc<dyn Clock>,
    config: EvictConfig,
    taken_over_at: AtomicU64,
}

impl MarkEvictor {
    /// The eviction of the leader whose markers are `markers`, in the marks bucket named `bucket`
    /// (the name the format record must give), over `hot` and `cold`; `records` is shared with the
    /// copier. The leader took over now, on `mono`.
    #[must_use]
    pub fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        markers: Markers,
        bucket: &str,
        lease: LeaderLease,
        records: Arc<CopyRecords>,
        mono: Arc<dyn Clock>,
        config: EvictConfig,
    ) -> Self {
        let taken_over_at = AtomicU64::new(mono.now());
        Self {
            hot,
            cold,
            markers,
            bucket: bucket.to_owned(),
            lease,
            records,
            mono,
            config,
            taken_over_at,
        }
    }

    /// The leader took over now: the records are dropped and a pair with no record counts as
    /// copied from this moment.
    pub fn took_over(&self) {
        self.records.clear();
        self.taken_over_at.store(self.mono.now(), Ordering::SeqCst);
    }

    fn taken_over(&self) -> u64 {
        self.taken_over_at.load(Ordering::SeqCst)
    }

    /// Evicts `keys`, at most `parallel` at a time (at least one), if the format opens writes on
    /// markers in this evictor's marks bucket.
    pub async fn evict_pass(&self, keys: Vec<String>, parallel: usize) -> EvictReport {
        let mut report = EvictReport::default();
        match marks_run_in(self.hot.as_ref(), &self.bucket).await {
            Ok(true) => report.active = true,
            Ok(false) => return report,
            Err(error) => {
                report.failed = keys.into_iter().map(|key| (key, error.clone())).collect();
                return report;
            }
        }
        let tries = stream::iter(keys).map(|key| async move {
            let result = self.evict_one(&key).await;
            (key, result)
        });
        let results: Vec<_> = tries.buffer_unordered(parallel.max(1)).collect().await;
        for (key, result) in results {
            match result {
                Ok(Evicted::Removed { freed, unreleased }) => {
                    report.evicted += 1;
                    report.freed = report.freed.saturating_add(freed);
                    report.unreleased.extend(unreleased);
                }
                Ok(Evicted::Dirtied) => report.dirtied += 1,
                Ok(Evicted::Young) => report.young += 1,
                Ok(Evicted::Kept) => report.kept += 1,
                Ok(Evicted::Gone) => report.gone += 1,
                Err(error) => report.failed.push((key, error)),
            }
        }
        report
    }

    /// Evicts the hot object of `key` by the rules of the module. It does not read the format:
    /// [`MarkEvictor::evict_pass`], which does, is the only way in from outside the crate.
    ///
    /// # Errors
    /// The error of a store call that failed. The lock, if one was taken, is released unless a
    /// hot DELETE ended with an outcome that is not known, which keeps it.
    pub(crate) async fn evict_one(&self, key: &str) -> Result<Evicted, Error> {
        let Some(hot) = found(self.hot.head(key).await)? else {
            return Ok(Evicted::Gone);
        };
        let cold = found(self.cold.head(key).await)?;
        let mut differs = false;
        if let Some(cold) = &cold {
            let recorded = self.records.copied_at(key, &hot.tag, &cold.tag);
            let copied_at = recorded.unwrap_or_else(|| self.taken_over());
            let age = self.mono.now().saturating_sub(copied_at);
            if age < millis(self.config.evict_min_age) {
                return Ok(Evicted::Young);
            }
            if recorded.is_none() {
                if self.same_bytes(key, &hot, cold).await? {
                    let limit = self.config.tag_record_limit;
                    self.records
                        .record(key, &hot.tag, &cold.tag, self.taken_over(), limit);
                } else {
                    differs = true;
                }
            }
        }
        let held = match self
            .markers
            .create(key, MarkBirth::Lock(MarkerWhy::Evict))
            .await?
        {
            Creation::Made(held) => held,
            Creation::Exists => return Ok(Evicted::Kept),
        };
        let compared = cold.map(|cold| cold.tag).filter(|_| !differs);
        self.under_lock(key, &hot, compared.as_deref(), held).await
    }

    /// The steps under the lock `held`. `compared` is the tag of the cold object the compare found
    /// equal to the hot one, or none if there was none or it differs.
    async fn under_lock(
        &self,
        key: &str,
        hot: &Meta,
        compared: Option<&str>,
        held: Written,
    ) -> Result<Evicted, Error> {
        let cold = match found(self.cold.head(key).await) {
            Ok(cold) => cold,
            Err(error) => return Err(self.failed(&held, false, error).await),
        };
        match (&cold, compared) {
            (Some(now), Some(compared)) if now.tag == compared => {}
            _ => {
                release(&self.markers, &held, true).await?;
                return Ok(Evicted::Dirtied);
            }
        }
        let Some(held) = renewed(&self.markers, held).await? else {
            return Ok(Evicted::Kept);
        };
        if let Err(error) = self.lease.require().await {
            return Err(self.failed(&held, false, error).await);
        }
        match self.hot.delete_if_match(key, &hot.tag).await {
            Ok(()) => {
                self.records.forget(key);
                let unreleased = release(&self.markers, &held, false).await.err();
                Ok(Evicted::Removed {
                    freed: hot.size,
                    unreleased,
                })
            }
            Err(Error::NotFound(_)) => {
                release(&self.markers, &held, false).await?;
                Ok(Evicted::Gone)
            }
            Err(Error::PreconditionFailed(_)) => {
                release(&self.markers, &held, true).await?;
                Ok(Evicted::Dirtied)
            }
            // The call may still be queued at the store: the lock stays until the reap.
            Err(error) if ambiguous(&error) => Err(error),
            Err(error) => Err(self.failed(&held, false, error).await),
        }
    }

    /// `error`, after the lock `held` was released as the call that failed had changed nothing; a
    /// release that fails too leaves the lock to the reap, and `error` is the one reported.
    async fn failed(&self, held: &Written, changed: bool, error: Error) -> Error {
        let _ = release(&self.markers, held, changed).await;
        error
    }

    /// Whether the cold object has the bytes of the hot one, read in ranges of at most
    /// `compare_range` bytes, so that neither is held whole; the read ends at the first
    /// difference.
    async fn same_bytes(&self, key: &str, hot: &Meta, cold: &Meta) -> Result<bool, Error> {
        if hot.size != cold.size {
            return Ok(false);
        }
        let step = self.config.compare_range.get();
        let mut offset = 0;
        while offset < hot.size {
            let range = ByteRange::new(offset, step.min(hot.size - offset));
            let mine = self.hot.get(key, Some(range)).await?;
            let theirs = self.cold.get(key, Some(range)).await?;
            if mine != theirs {
                return Ok(false);
            }
            offset += range.size();
        }
        Ok(true)
    }
}
