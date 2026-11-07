//! The reconciler that works on the journal entries of the keys, one entry per key, in place of the
//! journals of the proxies.
//!
//! It is the process that holds the lease `_tiering/lease`, and it is the only actor that releases
//! the entries of puts. A [`step`](EntryReconciler::step) does this, in order:
//!
//! 1. Reads `_tiering/format`. Until the object says `active` the reconciler does nothing at all:
//!    it takes no lease and reads no entry. That is the epoch gate: a node of this release beside
//!    nodes that still write journals is inert until the operator activates the new format.
//! 2. Takes the lease, or renews it if less than its margin is left, as the journal reconciler does
//!    (renewal runs on a timer of its own, so a long copy does not lose it).
//! 3. Lists `_tiering/proxies/`, to learn which registrations changed since the last step, and
//!    lists `_entries/`. An entry whose name is in its retry delay is left for later.
//! 4. Reads the entries, and works on them on a pool of [`ReconcilerConfig::parallel`] items, puts
//!    and deletes before copy-backs, each item by the rule of its operation and state:
//!    - `(PUT, PENDING)`: the reconciler replaces it, by a compare-and-swap, with
//!      `(PUT, HELD, me)` before it copies. If that fails, a delete amended the entry, and no copy
//!      starts. It then looks at the hot object and the cold object: no hot object, nothing to
//!      copy; no cold object, a copy of the hot one, create-only, with the margin of the lease
//!      checked right before; a cold object, a comparison of the bytes, and a difference is
//!      counted ([`StepReport::mismatched`]) and the hot object is kept. Then it seals the entry
//!      and removes it. A seal that fails means a delete amended the held entry while the copy
//!      ran, and that delete is waiting for the entry's release.
//!    - `(PUT, HELD, me)`: a copy that was cut off, done again.
//!    - `(DEL, PENDING)` and `(DEL, SEALED)`: seal, delete the key from the cold tier and then from
//!      the hot tier (the margin of the lease checked first), and remove the entry; a delete that
//!      waits for that removal answers then.
//!    - `(CB, PENDING)`: a copy-back that a read asked for. If it was first seen more than
//!      [`EntryReconcilerConfig::copy_back_ttl`] ago on this process's monotonic clock, or more than
//!      [`EntryReconcilerConfig::copy_back_max`] copy-backs are open and it is among the newest, it
//!      is sealed and removed without a copy. Otherwise it is sealed, and then, under the seal (a
//!      delete cannot amend a sealed entry, so none can run beside the copy, and the write stops,
//!      landing nothing, at the first chunk it asks for once the margin of the lease is gone: the
//!      entry then stays sealed for the successor), the cold object the key holds now is streamed into the hot tier, create-only, if the hot tier has none; then
//!      the entry is removed. A cold object that is gone, a hot object that is there, or a hot
//!      tier that answers full is nothing to copy, and the entry is removed all the same.
//!    - `(PUT|CB|EVICT, SEALED)`: removed (a copy-back that was sealed is not copied again: a later
//!      read asks again).
//!    - `(PUT|DEL, HELD, other)`: nothing while `other` is live; once it has departed (its
//!      registration is gone, or has stood unchanged for [`EntryReconcilerConfig::departure`] on
//!      this process's monotonic clock, counted from the first time it was seen), the entry is
//!      reaped to `(op, PENDING)` and, for a put, the open multipart uploads of the key are aborted
//!      on both tiers. So is an entry whose owner's process has restarted: the entry names the
//!      boot of the process that made it, and the registration of the owner (read once for each
//!      owner in the step, after the entries) names neither that boot nor, as a peer, any other
//!      process that may still be alive (the module `restart`). An entry of a live
//!      owner that has stood held and unchanged for
//!      [`EntryReconcilerConfig::held_ttl`] (a proxy that failed to hand over or remove it and goes
//!      on beating) is reaped the same way.
//!    - An entry of an operation or state this release does not know is left alone.
//! 5. Keeps the usage figure and evicts, as the modules `usage` and `evict` say: reads the figure
//!    of the leader before it at takeover, lists the hot tier a few pages a step (at takeover and
//!    every [`EntryReconcilerConfig::usage_scan_interval`]), publishes the figure to
//!    `_tiering/usage` every [`EntryReconcilerConfig::usage_interval`], and above the high water of
//!    the budget evicts the oldest objects that the cold tier holds and that no entry names, down
//!    to the low water. The eviction of a key is done under an `(EVICT, SEALED)` entry made
//!    create-only, after a compare of the bytes (skipped when this leader copied the key and both
//!    content tags are unchanged); a key with any entry is never evicted. A hot object that has no
//!    cold copy is adopted by a pending put entry instead.
//! 6. Sweeps the hot tier for strays, now and every [`EntryReconcilerConfig::stray_interval`]: a
//!    hot object older than [`EntryReconcilerConfig::stray_grace`] with no cold object and no entry
//!    gets a `(PUT, PENDING)` entry, which the next step copies. The same sweep aborts the multipart
//!    uploads of the hot tier that have stood open longer than
//!    [`EntryReconcilerConfig::abandoned_upload_after`].
//!
//! An item that fails (a store call that fails, or less than the lease margin left that a renewal
//! at once does not restore) is tried again after a delay that starts at
//! [`EntryReconcilerConfig::retry_min`] and doubles to [`EntryReconcilerConfig::retry_max`], so
//! that a cold tier that is down is not asked about every
//! entry at every step.
//!
//! [`EntryReconciler::run`] steps without waiting while a step makes progress, waits
//! [`EntryReconcilerConfig::idle_min`] while a live owner holds an entry (or this process is not the
//! leader), and otherwise waits a time that starts at `idle_min`, doubles to `idle_max` and starts
//! again at `idle_min` after progress. There is no wake-up: a put into an idle system waits for the
//! wait that is running.
//!
//! Under the format `active-marks` at epoch 3 or later, a reconciler that was given the marks bucket
//! the format names ([`EntryReconciler::with_marks`]) makes a pass over the markers of the
//! put-without-lock design in place of steps 3 to 6: the copy, the hold and the reap, then the
//! copy-back of the `WANT` markers, the stray sweep, the usage and the eviction (the modules
//! `marks`, `copier`, `reap` and `passes` say what each does). They run under the one lease this
//! reconciler holds, whichever phase the format is in, so the lease passes from the entries to the
//! markers and back without a second holder. Under any other reading of that phase it does
//! nothing, as it does nothing until the format says `active`.
//!
//! No call here probes what a store does: that a hot tier honours `If-Match`, or that a cold tier
//! writes create-only, is a requirement of the stores (see the design).

use std::collections::{BTreeMap, BTreeSet};
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{Either, select};
use futures::stream::{self, StreamExt};
use tokio::sync::Notify;

use crate::backend::Backend;
use crate::deadline::millis;
use crate::entry::{Entries, EntryOp, entry_key};
use crate::format::{FormatPhase, read_format};
use crate::journal::PROXIES_PREFIX;
use crate::lease::Lease;
use crate::registrations::Registrations;
use crate::{Error, MarkersConfig, ReconcilerConfig, Sleeper, TokioSleeper};

mod backoff;
mod copier;
mod copy_back;
mod evict;
mod marks;
mod passes;
mod reap;
mod restart;
mod sweep;
mod usage;
mod work;

pub(crate) use backoff::Idle;
use backoff::retry_delay;
use evict::Tags;
use marks::{MarkBooks, MarkSide};
use usage::{Books, Tally};
use work::Verdict;

/// The settings of an [`EntryReconciler`]: the lease, the pool and the water marks of
/// [`ReconcilerConfig`], and its own.
#[derive(Clone)]
pub struct EntryReconcilerConfig {
    /// The holder of the lease and the owner of the entries it holds (`holder`), the pool of
    /// copies (`parallel`), the lease (`lease_ttl`, `lease_renew`, `lease_margin`, `mono`,
    /// `sleeper`), the size of a listing (`page_size`) and the wall clock (`clock`), which must
    /// agree with the times the hot tier gives its objects to within `stray_grace`.
    pub base: ReconcilerConfig,
    /// How long the registration of an owner must have stood unchanged, by the monotonic clock of
    /// this process, for the owner to count as departed. At least six times the proxies' heartbeat.
    pub departure: Duration,
    /// How long a held put or delete of a live owner may stand unchanged, by the monotonic clock
    /// of this process from the first step that sees it so, before it is reaped as if its owner had
    /// departed. A proxy that cannot hand over or remove an entry (the hot tier failed the write)
    /// leaves it held, and its registration goes on beating, so its owner never departs. Longer than
    /// a put or a delete can take. An entry whose owner's process has restarted is not waited for
    /// this long: it is reaped at the next step after the owner's registration stops naming that
    /// process, which the restarted proxy does after the departure time in beats.
    pub held_ttl: Duration,
    /// The shortest wait of the loop; starts from `base.idle_min`.
    pub idle_min: Duration,
    /// The longest wait of the loop when nothing is to be done; starts from `base.idle_max`.
    pub idle_max: Duration,
    /// The delay after an item first fails.
    pub retry_min: Duration,
    /// The longest delay of a failing item.
    pub retry_max: Duration,
    /// The most bytes of one object that a comparison of the hot and the cold copy reads at once.
    pub compare_range: u64,
    /// How old a hot object with no entry and no cold object must be to be adopted.
    pub stray_grace: Duration,
    /// How often the stray sweep runs, after the one at takeover.
    pub stray_interval: Duration,
    /// How long a multipart upload may stand open in the hot tier, by the wall clock of `base` and
    /// the time the hot tier gives for its start, before the stray sweep aborts it. An upload takes
    /// no entry until its completion, so nothing else finds one that its client has left. Zero
    /// turns the abort off.
    pub abandoned_upload_after: Duration,
    /// How long a copy-back that was asked for may wait, on this process's monotonic clock from
    /// the first step that sees it, before it is dropped without a copy.
    pub copy_back_ttl: Duration,
    /// The most copy-backs that are copied: the oldest this many, by when they were first seen;
    /// the rest are dropped without a copy.
    pub copy_back_max: usize,
    /// The most cold lookups per second of the stray sweep.
    pub stray_rate: u32,
    /// How often the figure of the usage is published to `_tiering/usage`. The proxies judge the
    /// usage stale when the object has not changed for their own limit, so this is well under it.
    pub usage_interval: Duration,
    /// How often the hot tier is listed to measure its usage and to find the objects to evict,
    /// after the listing at takeover.
    pub usage_scan_interval: Duration,
    /// The most pages of that listing read in one step, so that the work on the entries and the
    /// publication of the usage go on while it runs.
    pub scan_pages: usize,
    /// The most objects of one listing kept as candidates of the eviction, the oldest.
    pub candidate_limit: usize,
    /// The most keys whose tags of the copies this leader made are remembered, for the eviction
    /// to skip the compare.
    pub tag_record_limit: usize,
    /// The settings of the markers the leader reads and writes (the part of section 9 that is the
    /// lease: the leader reaps a lock unchanged for `markers.lock_lease`).
    pub markers: MarkersConfig,
    /// How many keys the copier works at once under the markers (`copy_concurrency`, 128).
    pub copy_concurrency: usize,
    /// How many markers a listing page of the marks bucket holds (`marks_page`, 1000).
    pub marks_page: usize,
    /// How long a `DIRTY` marker with no hot object behind it must have stood unchanged before
    /// the leader clears it (`orphan_after`, 120 s).
    pub orphan_after: Duration,
    /// How long a `gone` marker is kept after the cold object was seen deleted or overwritten
    /// (`tombstone_hold`, 120 s): at least `lease_ttl` plus `lease_margin`, which the file's
    /// loader checks.
    pub tombstone_hold: Duration,
    /// How old the cold copy of a hot object must be, by the leader's monotonic clock from the
    /// cold write, for the eviction over markers to evict the hot object (`evict_min_age`, 600 s).
    pub evict_min_age: Duration,
    /// The largest object the copy-back over markers copies into the hot tier
    /// (`copy_back_max_bytes`, 64 MiB): the lock of the key is held for the whole stream.
    pub copy_back_max_bytes: u64,
    /// What the loop waits on between steps, and between the lookups of the sweep. The renewal of
    /// the lease waits on `base.sleeper`.
    pub waiter: Arc<dyn Sleeper>,
}

impl EntryReconcilerConfig {
    /// The settings over `base`, with a departure of 60 seconds, a held entry of a live owner reaped after an hour, an idle wait of 1 to 10 seconds,
    /// a retry delay of 1 to 60 seconds, comparisons in ranges of `base.compare_range` (1 MiB), a stray grace of 10
    /// minutes, a sweep every 24 hours at up to 100 lookups a second (which aborts no upload of the
    /// hot tier unless `abandoned_upload_after` is set), copy-backs kept for 10
    /// minutes and at most 1000 of them, the usage published every 10 seconds and measured by a
    /// listing every 24 hours, 100 pages of a listing a step, 10000 candidates of the eviction,
    /// 100000 remembered tags, and a [`TokioSleeper`].
    #[must_use]
    pub fn new(base: ReconcilerConfig) -> Self {
        let compare_range = base.compare_range.get();
        let abandoned_upload_after = base.abandoned_upload_after;
        let (idle_min, idle_max) = (base.idle_min, base.idle_max);
        Self {
            base,
            departure: Duration::from_secs(60),
            held_ttl: Duration::from_secs(3600),
            idle_min,
            idle_max,
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            compare_range,
            stray_grace: Duration::from_secs(600),
            stray_interval: Duration::from_secs(24 * 3600),
            abandoned_upload_after,
            copy_back_ttl: Duration::from_secs(600),
            copy_back_max: 1000,
            stray_rate: 100,
            usage_interval: Duration::from_secs(10),
            usage_scan_interval: Duration::from_secs(24 * 3600),
            scan_pages: 100,
            candidate_limit: 10_000,
            tag_record_limit: 100_000,
            markers: MarkersConfig::default(),
            copy_concurrency: 128,
            marks_page: 1000,
            orphan_after: Duration::from_secs(120),
            tombstone_hold: Duration::from_secs(120),
            evict_min_age: Duration::from_secs(600),
            copy_back_max_bytes: 64 << 20,
            waiter: Arc::new(TokioSleeper),
        }
    }
}

/// What one [`EntryReconciler::step`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Whether `_tiering/format` said `active`, or `active-marks` with the marks bucket this
    /// reconciler was given. If not, nothing else was done.
    pub active: bool,
    /// Whether this reconciler held the lease for the step. If not, nothing else was done.
    pub held: bool,
    /// The entries that changed, or were removed, or whose key was copied or deleted.
    pub changed: usize,
    /// The entries a live owner holds.
    pub pinned: usize,
    /// The entries left alone because their last item failed, and its delay has not run out.
    pub backed_off: usize,
    /// The keys whose hot object and cold object differ in their bytes. The hot object was kept.
    pub mismatched: Vec<String>,
    /// The stray objects that got an entry.
    pub strays_adopted: usize,
    /// The abandoned multipart uploads of the hot tier that were aborted.
    pub uploads_aborted: usize,
    /// The hot objects evicted.
    pub evicted: usize,
    /// Whether the hot tier is still above the low water of its budget after an eviction that ran
    /// out of candidates or stopped at a failure: the loop does not wait longer than `idle_min`.
    pub evicting: bool,
    /// Whether the listing of the hot tier has pages left: the loop steps again at once.
    pub scanning: bool,
    /// The calls that failed, in the order they did.
    pub failed: Vec<Error>,
}

impl StepReport {
    /// Whether the step changed anything, or read pages of the listing and has more: the loop does
    /// not wait after one that did.
    #[must_use]
    pub fn progress(&self) -> bool {
        self.changed > 0 || self.strays_adopted > 0 || self.uploads_aborted > 0 || self.scanning
    }
}

/// An item that failed: how many steps in a row, and the time on the monotonic clock before which
/// it is not tried again.
struct Retry {
    failures: u32,
    until: u64,
}

/// The runner that works on the entries in the hot tier. See the module for what a step does.
pub struct EntryReconciler {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: EntryReconcilerConfig,
    lease: Lease,
    entries: Entries,
    registrations: Registrations,
    retries: BTreeMap<String, Retry>,
    /// When the stray sweep runs next, on the monotonic clock; `None` for at the next step.
    sweep_at: Option<u64>,
    strays_adopted: u64,
    /// The open copy-backs by key: the nonce of the entry and when it was first seen.
    copy_backs: BTreeMap<String, (String, u64)>,
    /// The keys whose copy-back this step drops without a copy (too old, or over the maximum).
    copy_back_drops: BTreeSet<String>,
    /// The held puts and deletes of other owners by key: the nonce of the entry and when it was
    /// first seen so.
    helds: BTreeMap<String, (String, u64)>,
    /// The keys whose held entry has stood unchanged for [`EntryReconcilerConfig::held_ttl`]: reaped
    /// although the owner is live.
    held_expired: BTreeSet<String>,
    /// The boots that the registration of each owner of a held entry names, as read by this step
    /// (see `restart.rs`).
    boots: BTreeMap<String, BTreeSet<String>>,
    /// The usage figure and the listing that gives it.
    books: Books,
    /// What the pool tells the step about the usage.
    tally: Tally,
    /// The tags of the copies this leader made, for the eviction.
    tags: Mutex<Tags>,
    /// The marks bucket and the calls on its markers; none until [`EntryReconciler::with_marks`].
    marks: Option<Arc<MarkSide>>,
    /// What the pass over the markers noted of them.
    mark_books: MarkBooks,
}

impl EntryReconciler {
    /// A reconciler over `hot` and `cold`. Nothing is read or written until a step.
    #[must_use]
    pub fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: EntryReconcilerConfig,
    ) -> Self {
        let base = &config.base;
        let ttl = millis(base.lease_ttl);
        let third = ttl / 3;
        let renew = base.lease_renew.map_or(third, millis);
        let margin = base.lease_margin.map_or(third, millis);
        let mono = Arc::clone(&base.mono);
        let lease = Lease::new(Arc::clone(&hot), mono, &base.holder, ttl, renew, margin);
        let entries = Entries::new(
            Arc::clone(&hot),
            base.holder.as_str(),
            Arc::clone(&base.clock),
        );
        Self {
            hot,
            cold,
            config,
            lease,
            entries,
            registrations: Registrations::default(),
            retries: BTreeMap::new(),
            sweep_at: None,
            strays_adopted: 0,
            copy_backs: BTreeMap::new(),
            copy_back_drops: BTreeSet::new(),
            helds: BTreeMap::new(),
            held_expired: BTreeSet::new(),
            boots: BTreeMap::new(),
            books: Books::default(),
            tally: Tally::default(),
            tags: Mutex::default(),
            marks: None,
            mark_books: MarkBooks::default(),
        }
    }

    /// How many stray objects this reconciler has adopted, for a gauge.
    #[must_use]
    pub fn strays_adopted(&self) -> u64 {
        self.strays_adopted
    }

    /// The longest a stop of the server waits for [`stop`](Self::stop) (`base.stop_wait`).
    #[must_use]
    pub fn stop_wait(&self) -> Duration {
        self.config.base.stop_wait
    }

    /// Gives the lease up, if it is held, and forgets what was seen.
    pub async fn stop(&mut self) {
        self.lease.release().await;
        self.forget();
    }

    /// Forgets the retries, the registrations seen, the sweep (a new leader sweeps at once), the
    /// usage and the tags: the lease is not held.
    fn forget(&mut self) {
        self.retries.clear();
        self.registrations = Registrations::default();
        self.sweep_at = None;
        self.copy_backs.clear();
        self.copy_back_drops.clear();
        self.helds.clear();
        self.held_expired.clear();
        self.boots.clear();
        self.books = Books::default();
        self.tally.clear();
        self.tags().clear();
        self.mark_books = MarkBooks::default();
        self.unsettle_marks();
    }

    /// Does one step, if the format is active and the lease is held.
    pub async fn step(&mut self) -> StepReport {
        let mut report = StepReport::default();
        let format = match read_format(self.hot.as_ref()).await {
            Ok(Some(format)) => format,
            Ok(None) => return report,
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        };
        let side = self.runs_marks(&format);
        if side.is_none() {
            // The passes over the markers begin afresh when the format comes back to them.
            self.unsettle_marks();
            if format.phase != FormatPhase::Active {
                return report;
            }
        }
        report.active = true;
        if !self.lease.ensure().await {
            self.forget();
            return report;
        }
        let lease = self.lease.clone();
        let sleeper = Arc::clone(&self.config.base.sleeper);
        match side {
            Some(side) => {
                lease
                    .renewing(&*sleeper, self.mark_steps(side, report))
                    .await
            }
            None => lease.renewing(&*sleeper, self.steps(report)).await,
        }
    }

    /// The steps of a step, once the format is active and the lease is held.
    async fn steps(&mut self, mut report: StepReport) -> StepReport {
        report.held = true;
        let now = self.config.base.mono.now();
        let listed = match self.hot.list(PROXIES_PREFIX).await {
            Ok(listed) => listed,
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        };
        self.registrations.observe(now, &listed);
        let paths = match self.entries.list(self.config.base.page_size).await {
            Ok(paths) => paths,
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        };
        let listed: BTreeSet<&String> = paths.iter().collect();
        self.retries.retain(|path, _| listed.contains(path));
        self.copy_backs
            .retain(|key, _| listed.contains(&entry_key(key)));
        self.helds.retain(|key, _| listed.contains(&entry_key(key)));
        let waiting = |path: &String| self.retries.get(path).is_some_and(|r| r.until > now);
        let (backed_off, due): (Vec<String>, Vec<String>) =
            paths.iter().cloned().partition(waiting);
        report.backed_off = backed_off.len();
        self.work(due, now, &mut report).await;
        if self.sweep_due(now) {
            self.sweep(now, &mut report).await;
        }
        self.tend_usage(now, &mut report).await;
        self.evict(&mut report).await;
        self.publish_usage(now, &mut report).await;
        report
    }

    /// Reads the entries at `paths`, and does the step of each, puts and deletes first.
    async fn work(&mut self, paths: Vec<String>, now: u64, report: &mut StepReport) {
        let parallel = self.config.base.parallel.max(1);
        let this = &*self;
        let reads = stream::iter(paths).map(|path| async move {
            let read = this.entries.read_path(&path).await;
            (path, read)
        });
        let read: Vec<_> = reads.buffer_unordered(parallel).collect().await;
        let mut items = Vec::new();
        let mut failed = Vec::new();
        for (path, read) in read {
            match read {
                Ok(Some(seen)) => items.push((path, seen)),
                Ok(None) => {}
                Err(error) => failed.push((path, error)),
            }
        }
        items.sort_by_key(|(_, seen)| seen.entry.op == EntryOp::Cb);
        self.note_copy_backs(&items, now);
        self.note_helds(&items, now);
        let (boots, unread) = self.read_boots(&items).await;
        self.boots = boots;
        report.failed.extend(unread);
        let this = &*self;
        let steps = stream::iter(items).map(|(path, seen)| async move {
            let verdict = this.process(&seen, now).await;
            (path, seen.entry.key, verdict)
        });
        let worked: Vec<_> = steps.buffer_unordered(parallel).collect().await;
        for (path, error) in failed {
            self.fail(path, error, report);
        }
        for (path, key, verdict) in worked {
            match verdict {
                Ok(verdict) => {
                    self.retries.remove(&path);
                    match verdict {
                        Verdict::Skipped => {}
                        Verdict::Pinned => report.pinned += 1,
                        Verdict::Changed => report.changed += 1,
                        Verdict::Mismatched => {
                            report.changed += 1;
                            report.mismatched.push(key);
                        }
                    }
                }
                Err(error) => self.fail(path, error, report),
            }
        }
    }

    /// Notes that the item at `path` failed, now by the monotonic clock (a step can be long), and
    /// when it is tried again.
    fn fail(&mut self, path: String, error: Error, report: &mut StepReport) {
        let now = self.config.base.mono.now();
        let failures = self
            .retries
            .get(&path)
            .map_or(1, |retry| retry.failures + 1);
        let config = &self.config;
        let delay = retry_delay(config.retry_min, config.retry_max, failures);
        let until = now.saturating_add(millis(delay));
        self.retries.insert(path, Retry { failures, until });
        report.failed.push(error);
    }

    /// Steps until `halt` is notified, waiting between steps as the module says, and then gives
    /// the lease up. A halt cuts a step that is running, as it cuts a wait.
    pub async fn run(&mut self, halt: &Notify) {
        let mut idle = Idle::new(self.config.idle_min, self.config.idle_max);
        let waiter = Arc::clone(&self.config.waiter);
        loop {
            let report = {
                let step = pin!(self.step());
                match select(step, pin!(halt.notified())).await {
                    Either::Left((report, _)) => report,
                    Either::Right(_) => break,
                }
            };
            if let Some(pause) = idle.after(&report) {
                let wait = pin!(waiter.sleep(pause));
                if let Either::Right(_) = select(wait, pin!(halt.notified())).await {
                    break;
                }
            }
        }
        self.stop().await;
    }
}

#[cfg(test)]
#[path = "entry_reconciler_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "entry_reconciler_marks_tests.rs"]
mod marks_tests;
