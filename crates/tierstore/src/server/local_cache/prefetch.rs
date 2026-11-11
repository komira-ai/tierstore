//! The queue of blobs to fill before a read asks for them, and the workers that fill them.
//!
//! A blob that is queued has its place in the table of the fills (`flights`) like a fill that
//! runs, so that it is one entry of one table: a read of the same name (the mount's open through
//! the fill socket, or a `GET` of the S3 front) finds it there. What differs is that the fill has
//! not started, and that a read that finds it **promotes** it: the read starts it at once, on a
//! permit of its own pool, and the queue forgets it; the blob is fetched once, and the readers
//! that came later join it as they join any fill that runs.
//!
//! The workers are the permits of `prefetch_concurrency`, a pool apart from the two pools of the
//! reads: a worker is a fill that runs on one, and when it ends the next blob of the queue starts
//! on the permit it frees. A blob is checked as it is about to start, not when it is queued (the
//! cache fills while the queue waits): it is dropped if it waited longer than
//! `prefetch_queue_ttl` (the job has most likely read it), and if it would take the cache past its
//! high water mark, since a prefetch never evicts what may be in use for what may be read. A
//! queued blob is in memory only: a restart forgets the queue.
//!
//! [`LocalCache::prefetch`] queues one blob; it answers as soon as the blob is queued, joined or
//! turned away, and fetches nothing before it answers.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::fill::{Class, Ended, Filled, Held, Launch, Pending, Plan, Stop};
use super::layout::name_of;
use super::metrics::{PrefetchFailed, bump};
use super::source::{Blob, object_key};
use super::{Inner, LocalCache, locked, percent_of};
use crate::Error;

/// The prefetch workers, unless the settings say otherwise.
pub const PREFETCH_CONCURRENCY: usize = 2;

/// The blobs that may be queued, unless the settings say otherwise.
pub const PREFETCH_QUEUE: usize = 16_384;

/// How long a blob may wait in the queue, unless the settings say otherwise.
pub const PREFETCH_QUEUE_TTL: Duration = Duration::from_secs(120);

/// The trees one call may expand, unless the settings say otherwise.
pub const HYDRATE_MAX_TREES: usize = 64;

/// The blobs one call may queue, unless the settings say otherwise.
pub const HYDRATE_MAX_BLOBS: usize = 500_000;

/// The bytes of the blobs one call may queue, unless the settings say otherwise.
pub const HYDRATE_MAX_BYTES: u64 = 32 << 30;

/// The settings of the prefetch queue (`local_cache.prefetch_concurrency`, `prefetch_queue` and
/// `prefetch_queue_ttl`) and of the expansion of trees into it (`hydrate_max_trees`,
/// `hydrate_max_blobs` and `hydrate_max_bytes`).
#[derive(Clone, Copy, Debug)]
pub struct PrefetchConfig {
    /// The most blobs filled at once by the queue: a pool of permits of its own, which no read
    /// draws on. 0 turns the queue off: [`LocalCache::prefetch`] answers [`Prefetched::Off`].
    pub concurrency: usize,
    /// The most blobs queued, all callers together; past it a blob is turned away
    /// ([`Prefetched::Busy`]).
    pub queue: usize,
    /// The longest a blob waits in the queue; older, it is dropped when its turn comes.
    pub ttl: Duration,
    /// The most trees one call may expand: a call with more is refused whole.
    pub max_trees: usize,
    /// The most blobs one call may queue, all its trees together; the walk stops at the first
    /// that would pass it.
    pub max_blobs: usize,
    /// The most bytes of the blobs one call may queue; the walk stops at the first blob that
    /// would pass it.
    pub max_bytes: u64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            concurrency: PREFETCH_CONCURRENCY,
            queue: PREFETCH_QUEUE,
            ttl: PREFETCH_QUEUE_TTL,
            max_trees: HYDRATE_MAX_TREES,
            max_blobs: HYDRATE_MAX_BLOBS,
            max_bytes: HYDRATE_MAX_BYTES,
        }
    }
}

/// What [`LocalCache::prefetch`] did with a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Prefetched {
    /// The cache holds it already: counted as used now, and nothing more.
    Present,
    /// A fill of it is queued or running already, whoever asked for it.
    Joined,
    /// It is queued: a worker will fill it, or a read that comes first will.
    Queued,
    /// It is larger than the whole budget.
    TooLarge,
    /// The queue is full.
    Busy,
    /// `prefetch_concurrency` is 0: there is no queue.
    Off,
    /// This process does not hold the lock of the cache directory: it fills nothing.
    ReadOnly,
}

/// A blob that waits for a worker.
pub(super) struct Queued {
    /// What starts the fill.
    pub(super) launch: Launch,
    /// The bytes it will take.
    size: u64,
    /// The time it was queued, in milliseconds of the cache's clock.
    since: u64,
}

/// The blobs that wait, in the order they were queued.
#[derive(Default)]
pub(super) struct Queue {
    /// The number of the next blob to be queued.
    next: u64,
    waiting: BTreeMap<u64, Queued>,
    /// The place in `waiting` of each name.
    places: HashMap<String, u64>,
}

impl Queue {
    pub(super) fn len(&self) -> usize {
        self.waiting.len()
    }

    fn contains(&self, hex: &str) -> bool {
        self.places.contains_key(hex)
    }

    /// The names of the blobs that wait, in the order they will start.
    #[cfg(test)]
    pub(super) fn order(&self) -> Vec<String> {
        let names = self.waiting.values();
        names.map(|queued| queued.launch.name.hex.clone()).collect()
    }

    fn push(&mut self, queued: Queued) {
        let place = self.next;
        self.next += 1;
        let _ = self.places.insert(queued.launch.name.hex.clone(), place);
        let _ = self.waiting.insert(place, queued);
    }

    /// The blob of `hex`, taken out of the queue.
    fn take(&mut self, hex: &str) -> Option<Queued> {
        let place = self.places.remove(hex)?;
        self.waiting.remove(&place)
    }

    /// The blob that has waited longest, taken out of the queue.
    fn pop(&mut self) -> Option<Queued> {
        let (_, queued) = self.waiting.pop_first()?;
        let _ = self.places.remove(&queued.launch.name.hex);
        Some(queued)
    }
}

/// The queue and the pool of its workers.
pub(super) struct Prefetch {
    pub(super) queue: Mutex<Queue>,
    pub(super) workers: Arc<Semaphore>,
}

impl Prefetch {
    pub(super) fn new(config: &PrefetchConfig) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            workers: Arc::new(Semaphore::new(config.concurrency)),
        }
    }
}

/// Why a queued blob is not filled.
enum Dropped {
    /// The cache holds it by now.
    Present,
    /// It waited longer than the queue allows, or would take the cache past its high water mark.
    Failed(PrefetchFailed),
}

impl Inner {
    /// Queues the blob, or says why not: see [`Prefetched`]. A blob that is queued is filled at
    /// once if a worker is free.
    pub(super) fn queue_blob(self: &Arc<Self>, blob: &Blob<'_>) -> Prefetched {
        let outcome = if !self.read_write() {
            Prefetched::ReadOnly
        } else if self.config.prefetch.concurrency == 0 {
            Prefetched::Off
        } else {
            self.admit_blob(blob)
        };
        self.counters.blob(outcome);
        if outcome == Prefetched::Queued {
            self.pump();
        }
        outcome
    }

    /// Puts the blob in the table and the queue, unless it is turned away.
    fn admit_blob(self: &Arc<Self>, blob: &Blob<'_>) -> Prefetched {
        if blob.size > self.config.budget {
            return Prefetched::TooLarge;
        }
        let key = object_key(blob.sha256);
        let name = name_of(&key);
        let plan = Plan::Blob {
            from: blob.from.to_owned(),
            offset: blob.offset,
            size: blob.size,
        };
        let now = self.clock.now();
        // The table is locked, then the queue: the order a read that promotes takes them in.
        let mut flights = locked(&self.flights);
        if locked(&self.index).used(&name.hex, now).is_some() {
            return Prefetched::Present;
        }
        let mut queue = locked(&self.prefetch.queue);
        if flights.contains_key(&name.hex) {
            return Prefetched::Joined;
        }
        if queue.len() >= self.config.prefetch.queue {
            return Prefetched::Busy;
        }
        let (pending, launch) = self.place(&key, &name, plan);
        let _ = flights.insert(name.hex, pending);
        queue.push(Queued {
            launch,
            size: blob.size,
            since: now,
        });
        Prefetched::Queued
    }

    /// Whether the blob of `hex` waits in the queue.
    fn queued(&self, hex: &str) -> bool {
        locked(&self.prefetch.queue).contains(hex)
    }

    /// The fill of `hex` that a read may join: one that runs. A blob that is only queued is none
    /// (the read starts it), and a blob that was queued is none once it is dropped. The table is
    /// locked for the lookup and the check together, and the queue drops a blob and takes it out of
    /// the table under that same lock, so a place found here that is not queued stays the place of
    /// a fill that will run.
    pub(super) fn joinable(&self, hex: &str) -> Option<Pending> {
        let flights = locked(&self.flights);
        let running = flights.get(hex)?;
        (!self.queued(hex)).then(|| running.clone())
    }

    /// Takes the blob of `hex` out of the queue, if it waits there, for a read to start on a
    /// permit of `class`: none if it does not wait (the read joins the fill that runs); the read
    /// is turned away if the S3 front finds every permit taken, and the blob stays queued.
    pub(super) fn take_queued(
        &self,
        hex: &str,
        class: Class,
    ) -> Result<Option<(Launch, Held)>, Filled> {
        let mut queue = locked(&self.prefetch.queue);
        if !queue.contains(hex) {
            return Ok(None);
        }
        let permit = self.foreground_permit(class)?;
        bump(&self.counters.prefetch_promotions);
        Ok(queue
            .take(hex)
            .map(|queued| (queued.launch, Held::foreground(permit))))
    }

    /// Starts queued blobs on the workers that are free, until the queue or the workers run out.
    pub(super) fn pump(self: &Arc<Self>) {
        while let Ok(permit) = Arc::clone(&self.prefetch.workers).try_acquire_owned() {
            let Some(queued) = self.next_queued() else {
                return;
            };
            self.start(queued.launch, Held::worker(permit));
        }
    }

    /// The blob that has waited longest and is still to be filled; the ones before it that are
    /// not are dropped, and leave the table with the table and the queue locked, so that no read
    /// finds a place that nothing will fill.
    pub(super) fn next_queued(&self) -> Option<Queued> {
        let mut flights = locked(&self.flights);
        let mut queue = locked(&self.prefetch.queue);
        let mut gone = Vec::new();
        let next = loop {
            let Some(queued) = queue.pop() else {
                break None;
            };
            let hex = &queued.launch.name.hex;
            match self.waiting_blob(&queued) {
                Ok(()) => break Some(queued),
                Err(why) => {
                    if let Dropped::Failed(failure) = why {
                        self.counters.prefetch_failed(failure);
                    }
                    gone.push(flights.remove(hex));
                    let _ = locked(&self.invalidated).remove(hex);
                }
            }
        };
        drop(queue);
        drop(flights);
        drop(gone);
        next
    }

    /// Whether `queued` is still to be filled when its turn comes.
    fn waiting_blob(&self, queued: &Queued) -> Result<(), Dropped> {
        if locked(&self.index).get(&queued.launch.name.hex).is_some() {
            return Err(Dropped::Present);
        }
        let waited = self.clock.now().saturating_sub(queued.since);
        if u128::from(waited) > self.config.prefetch.ttl.as_millis() {
            return Err(Dropped::Failed(PrefetchFailed::Expired));
        }
        let held = locked(&self.index).bytes();
        let reserved = self.reserved.load(Ordering::SeqCst);
        let total = held.saturating_add(reserved).saturating_add(queued.size);
        if total > percent_of(self.config.budget, self.config.high_water) {
            return Err(Dropped::Failed(PrefetchFailed::NoSpace));
        }
        Ok(())
    }

    /// A fill of a worker ended: counted, and its permit given back to the next blob of the queue.
    pub(super) fn prefetch_ended(self: &Arc<Self>, ended: &Ended, permit: OwnedSemaphorePermit) {
        match ended {
            Ok(Filled::Kept) => bump(&self.counters.prefetch_fills),
            Ok(_) => self.counters.prefetch_failed(PrefetchFailed::NoSpace),
            Err(stop) => self.counters.prefetch_failed(PrefetchFailed::of(stop)),
        }
        drop(permit);
        self.pump();
    }
}

impl PrefetchFailed {
    /// The reason a fill that stopped for `stop` counts under.
    pub(super) fn of(stop: &Stop) -> Self {
        match stop {
            Stop::Upstream(Error::NotFound(_)) => Self::NotFound,
            Stop::Upstream(_) => Self::Upstream,
            Stop::Mismatch => Self::BadDigest,
            Stop::Changed | Stop::Invalidated => Self::Changed,
            Stop::Short | Stop::Local | Stop::Lost | Stop::TooLarge | Stop::Aborted => Self::Other,
        }
    }
}

impl LocalCache {
    /// Queues the blob `blob` to be filled by a worker, as the entry named by its digest (the
    /// entry [`fill_blob`](Self::fill_blob) makes, and the one the S3 front keeps for the object
    /// `cas/l/<sha256>`), unless the cache holds it, or a fill of it is queued or running. It
    /// answers at once, before anything is read from the upstream. A read of the blob that comes
    /// while it waits starts it (see the module).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the hydrate call that queues the blobs is a later change"
        )
    )]
    pub(in crate::server) fn prefetch(&self, blob: &Blob<'_>) -> Prefetched {
        self.inner.queue_blob(blob)
    }
}
