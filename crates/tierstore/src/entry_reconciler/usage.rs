//! The hot tier's usage as the leader keeps it: the figure it publishes in `_tiering/usage`, the
//! listing that gives it, and the objects that listing offers for eviction.
//!
//! A new leader starts from the figure the leader before it published, and lists the hot tier a
//! few pages at a time between its other work, so the figure keeps being published while the
//! listing runs. The listing ends with the bytes of every object that is not under a reserved
//! prefix, and the oldest of those objects, which are the candidates of the eviction (at most
//! [`EntryReconcilerConfig::candidate_limit`](super::EntryReconcilerConfig), kept in a heap while
//! the listing runs). Another listing starts every
//! [`EntryReconcilerConfig::usage_scan_interval`](super::EntryReconcilerConfig). Between listings
//! the figure moves by what this leader does: the bytes of an object it copies that landed after
//! the listing started are added, and the bytes of an object it evicts are taken off. Each object
//! counts once: the listing remembers the key and size of each object it read that was written at
//! or after the listing's start, and the leader remembers the size it has counted for each key it
//! copied, so that
//!
//! - an object whose key the listing has yet to reach is left to the listing and adds nothing,
//!   whatever its write time;
//! - an object the listing read adds nothing, and one that replaced it (the same key, another size)
//!   adds the difference of the sizes;
//! - an object the listing did not read (written after its page was read, or after it ended) adds
//!   its bytes once, however often its copy is tried;
//! - an object written before the listing started is in the listing and adds nothing;
//! - an object evicted while a listing runs, whose key the listing has passed, is taken off the
//!   figure the listing ends with, so that the end of the listing does not bring it back; an
//!   object evicted ahead of the listing is not read by it.
//!
//! "At or after the listing's start" is read on the clock of the hot tier, never the leader's: the
//! start is the write time of the usage object as the hot tier reports it, which is not later than
//! the start itself (so that no object that the listing may have missed is dropped), or the
//! beginning of time if the hot tier has no usage object yet (then the listing remembers every
//! object it reads). These records are kept until the next listing starts. A delete that a client
//! finishes by itself is not seen, so the figure may be too high until the next listing.
//!
//! The bytes that the cold tier does not hold yet (`copy_lag`) are those of the keys whose copy
//! failed, or that the leader gave an entry because the cold tier had no copy, until the copy is
//! made or the hot object is gone.

use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{EntryReconciler, StepReport};
use crate::Error;
use crate::deadline::millis;
use crate::journal::{RESERVED_PREFIXES, USAGE_KEY, Usage, to_body, write_object};

/// An object offered for eviction: when it was last written, its key and its size. The oldest
/// sorts first.
pub(super) type Candidate = (u64, String, u64);

/// A listing of the hot tier that is not finished.
struct Scan {
    /// The last key of the pages read so far.
    after: Option<String>,
    /// The bytes of the objects read so far.
    bytes: u64,
    /// The oldest objects read so far: a heap whose top is the newest of them, so that the one that
    /// goes when the heap is full is the newest.
    oldest: BinaryHeap<Candidate>,
}

/// What the leader knows of the hot tier's usage. Everything here is the leader's own: a new
/// leader starts it afresh.
#[derive(Default)]
pub(super) struct Books {
    /// The bytes the hot tier holds as far as the leader knows; none until the figure of the
    /// leader before it has been read or a listing has ended.
    pub(super) bytes: Option<u64>,
    /// The listing in progress.
    scan: Option<Scan>,
    /// When the next listing starts, on the monotonic clock; none for at once.
    scan_at: Option<u64>,
    /// When the figure is published next, on the monotonic clock; none for at once.
    publish_at: Option<u64>,
    /// The objects the last listing offered for eviction, oldest first.
    pub(super) candidates: VecDeque<Candidate>,
    /// Whether an eviction cycle is open: it opened when the figure passed the high water and
    /// closes only when the figure is no more than the low water, however many steps that takes.
    pub(super) evicting: bool,
}

/// What the calls that run on the pool tell the step, which only a `&self` can reach.
#[derive(Default)]
pub(super) struct Tally {
    /// The count of the objects that landed or went since the last listing started.
    landing: Mutex<Landing>,
    /// The keys whose hot object the cold tier does not hold yet, with the bytes of each.
    lagging: Mutex<BTreeMap<String, u64>>,
}

/// The objects that landed or went since the last listing started, so that each is counted once
/// whether the listing read it or not.
#[derive(Default)]
struct Landing {
    /// The time on the hot tier's clock the last listing started at or before (zero before the
    /// first): an object written before it is in that listing.
    from: u64,
    /// How far the listing in progress has read, if one is: none when no listing runs, and then
    /// `Some(None)` before its first page.
    reached: Option<Option<String>>,
    /// The keys the last listing read of the objects written at or after its start, with the size
    /// it read for each.
    listed: BTreeMap<String, u64>,
    /// The keys the leader copied, with the size it has counted for each, so that a copy tried
    /// again adds nothing more.
    counted: BTreeMap<String, u64>,
    /// The bytes of the objects added since the figure last took them.
    landed: u64,
    /// The bytes taken off since the figure last took them: a copied object that is smaller than
    /// the one counted before it under its key.
    shrunk: u64,
    /// The bytes of the objects the listing in progress has read and the leader has evicted since.
    deducted: u64,
}

impl Landing {
    /// The size counted for `key` since the last listing started: by the copies, else by the
    /// listing.
    fn known(&self, key: &str) -> Option<u64> {
        self.counted.get(key).or(self.listed.get(key)).copied()
    }

    /// Whether the listing in progress has yet to reach `key`.
    fn ahead(&self, key: &str) -> bool {
        self.reached
            .as_ref()
            .is_some_and(|after| after.as_deref().is_none_or(|after| key > after))
    }
}

impl Tally {
    /// Notes that the object `key` of `size` bytes, last written at `modified`, is being copied.
    /// Its bytes are added once, however often the copy is tried: less what the listing or an
    /// earlier copy counted under the key, and not at all if the listing has yet to reach its key
    /// or it was written before the listing started.
    pub(super) fn copied(&self, key: &str, size: u64, modified: u64) {
        let mut landing = self.landing();
        if modified < landing.from || landing.ahead(key) {
            return;
        }
        let known = landing.known(key).unwrap_or(0);
        landing.counted.insert(key.to_owned(), size);
        landing.landed = landing.landed.saturating_add(size.saturating_sub(known));
        landing.shrunk = landing.shrunk.saturating_add(known.saturating_sub(size));
    }

    /// Notes that the listing has read the object `key` of `size` bytes, last written at
    /// `modified`.
    pub(super) fn listed(&self, key: &str, size: u64, modified: u64) {
        let mut landing = self.landing();
        if modified >= landing.from {
            landing.listed.insert(key.to_owned(), size);
        }
    }

    /// Notes that the leader has evicted the object `key` of `size` bytes, last written at
    /// `modified`. While a listing runs and has passed the key, the bytes the listing and the
    /// copies counted for it are taken off the figure the listing will end with.
    pub(super) fn evicted(&self, key: &str, size: u64, modified: u64) {
        let mut landing = self.landing();
        let behind = landing.reached.is_some() && !landing.ahead(key);
        let known = landing.known(key);
        landing.counted.remove(key);
        landing.listed.remove(key);
        if !behind {
            return;
        }
        let held = known
            .or((modified < landing.from).then_some(size))
            .unwrap_or(0);
        landing.deducted = landing.deducted.saturating_add(held);
    }

    /// Starts the count of a listing that starts at `from`.
    pub(super) fn start(&self, from: u64) {
        let mut landing = self.landing();
        *landing = Landing {
            from,
            reached: Some(None),
            ..Landing::default()
        };
    }

    /// Notes how far the listing in progress has read.
    pub(super) fn reach(&self, to: Option<&str>) {
        self.landing().reached = Some(to.map(str::to_owned));
    }

    /// Ends the listing in progress and gives the bytes to add to what it read: the bytes landed,
    /// less those it read of objects evicted since.
    pub(super) fn finish(&self) -> (u64, u64) {
        let mut landing = self.landing();
        landing.reached = None;
        let shrunk = std::mem::take(&mut landing.shrunk);
        (
            std::mem::take(&mut landing.landed),
            std::mem::take(&mut landing.deducted).saturating_add(shrunk),
        )
    }

    /// Takes the bytes landed since they were last taken, less those that shrank.
    pub(super) fn take_landed(&self) -> (u64, u64) {
        let mut landing = self.landing();
        (
            std::mem::take(&mut landing.landed),
            std::mem::take(&mut landing.shrunk),
        )
    }

    /// Notes that the cold tier does not hold the hot object `key` of `size` bytes yet.
    pub(super) fn lags(&self, key: &str, size: u64) {
        self.lagging().insert(key.to_owned(), size);
    }

    /// Notes that the cold tier holds the object `key`, or that the hot tier has none.
    pub(super) fn caught_up(&self, key: &str) {
        self.lagging().remove(key);
    }

    /// The bytes that the cold tier does not hold yet.
    pub(super) fn lag(&self) -> u64 {
        let lagging = self.lagging();
        lagging
            .values()
            .fold(0, |sum, size| sum.saturating_add(*size))
    }

    /// Forgets everything: a new leader starts over.
    pub(super) fn clear(&self) {
        *self.landing() = Landing::default();
        self.lagging().clear();
    }

    fn landing(&self) -> MutexGuard<'_, Landing> {
        // No method panics while holding the lock, so a poisoned state is still whole.
        self.landing.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lagging(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        // No method panics while holding the lock, so a poisoned state is still whole.
        self.lagging.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl EntryReconciler {
    /// Keeps the usage figure: reads the leader's before it at takeover, lists the hot tier when a
    /// listing is due and works on the one in progress, and publishes the figure when it is due.
    /// A failed call is noted in the report and tried again at the next step.
    pub(super) async fn tend_usage(&mut self, now: u64, report: &mut StepReport) {
        if self.books.bytes.is_none() {
            self.books.bytes = self.published(report).await;
        }
        let due = self.books.scan_at.is_none_or(|at| now >= at);
        let scan = match self.books.scan.take() {
            Some(scan) => Some(scan),
            None if due => Some(self.start_scan().await),
            None => None,
        };
        match scan {
            Some(scan) => report.scanning = self.scan_pages(scan, now, report).await,
            None => self.fold_landed(),
        }
    }

    /// Adds the bytes of the objects copied since the last listing started to the figure.
    fn fold_landed(&mut self) {
        let (landed, shrunk) = self.tally.take_landed();
        self.books.bytes = self
            .books
            .bytes
            .map(|bytes| bytes.saturating_add(landed).saturating_sub(shrunk));
    }

    /// The figure the leader before this one last published, if there is a readable one.
    async fn published(&self, report: &mut StepReport) -> Option<u64> {
        match self.hot.get(USAGE_KEY, None).await {
            Ok(body) => serde_json::from_slice::<Usage>(&body)
                .ok()
                .map(|usage| usage.hot_bytes),
            Err(Error::NotFound(_)) => None,
            Err(error) => {
                report.failed.push(error);
                None
            }
        }
    }

    /// Starts a listing. Objects copied from now on that are newer than it are counted by the
    /// deltas, and those listed are counted by the listing. The start is the write time of the
    /// usage object on the hot tier's clock, or the beginning of time if there is none to read.
    async fn start_scan(&mut self) -> Scan {
        self.fold_landed();
        let from = self
            .hot
            .head(USAGE_KEY)
            .await
            .map_or(0, |meta| meta.modified);
        self.tally.start(from);
        Scan {
            after: None,
            bytes: 0,
            oldest: BinaryHeap::new(),
        }
    }

    /// Reads up to [`EntryReconcilerConfig::scan_pages`](super::EntryReconcilerConfig) pages of
    /// `scan`, and ends it if it reaches the last; otherwise it is kept for the next step. Gives
    /// whether pages are left, which is not the case after a failure: that waits for the next
    /// step.
    async fn scan_pages(&mut self, mut scan: Scan, now: u64, report: &mut StepReport) -> bool {
        let page_size = self.config.base.page_size.max(1);
        let limit = self.config.candidate_limit.max(1);
        for _ in 0..self.config.scan_pages.max(1) {
            let listing = self
                .hot
                .list_page("", scan.after.as_deref(), page_size)
                .await;
            let page = match listing {
                Ok(page) => page,
                Err(error) => {
                    report.failed.push(error);
                    self.books.scan = Some(scan);
                    return false;
                }
            };
            for object in &page {
                let reserved = RESERVED_PREFIXES
                    .iter()
                    .any(|prefix| object.key.starts_with(prefix));
                if reserved {
                    continue;
                }
                self.tally.listed(&object.key, object.size, object.modified);
                scan.bytes = scan.bytes.saturating_add(object.size);
                scan.oldest
                    .push((object.modified, object.key.clone(), object.size));
                if scan.oldest.len() > limit {
                    scan.oldest.pop();
                }
            }
            if page.len() < page_size {
                self.end_scan(scan, now);
                return false;
            }
            scan.after = page.last().map(|object| object.key.clone());
            self.tally.reach(scan.after.as_deref());
        }
        self.books.scan = Some(scan);
        true
    }

    /// Takes the result of the listing that has read its last page.
    fn end_scan(&mut self, scan: Scan, now: u64) {
        let (landed, deducted) = self.tally.finish();
        let bytes = scan.bytes.saturating_add(landed);
        self.books.bytes = Some(bytes.saturating_sub(deducted));
        self.books.candidates = scan.oldest.into_sorted_vec().into();
        let wait = millis(self.config.usage_scan_interval);
        self.books.scan_at = Some(now.saturating_add(wait));
    }

    /// Publishes the figure if it is due and known, and the lease has its margin left or can be renewed now. A figure
    /// that fails to be written is tried again at the next step.
    pub(super) async fn publish_usage(&mut self, now: u64, report: &mut StepReport) {
        let Some(hot_bytes) = self.books.bytes else {
            return;
        };
        if self.books.publish_at.is_some_and(|at| now < at) || !self.lease.ensure_margin().await {
            return;
        }
        let usage = Usage {
            hot_bytes,
            copy_lag: self.tally.lag(),
            time: self.config.base.clock.now(),
        };
        match write_object(self.hot.as_ref(), USAGE_KEY, to_body(&usage)).await {
            Ok(()) => {
                let wait = millis(self.config.usage_interval);
                self.books.publish_at = Some(now.saturating_add(wait));
            }
            Err(error) => report.failed.push(error),
        }
    }
}
