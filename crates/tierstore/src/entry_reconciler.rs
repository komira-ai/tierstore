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
//!    - `(PUT|CB|EVICT, SEALED)`: removed. `(CB, PENDING)`: sealed and removed without a copy (the
//!      copy-back of an entry is not done by this release).
//!    - `(PUT|DEL, HELD, other)`: nothing while `other` is live; once it has departed (its
//!      registration is gone, or has stood unchanged for [`EntryReconcilerConfig::departure`] on
//!      this process's monotonic clock, counted from the first time it was seen), the entry is
//!      reaped to `(op, PENDING)` and, for a put, the open multipart uploads of the key are aborted
//!      on both tiers.
//!    - An entry of an operation or state this release does not know is left alone.
//! 5. Sweeps the hot tier for strays, now and every [`EntryReconcilerConfig::stray_interval`]: a
//!    hot object older than [`EntryReconcilerConfig::stray_grace`] with no cold object and no entry
//!    gets a `(PUT, PENDING)` entry, which the next step copies.
//!
//! An item that fails (a store call that fails, or less than the lease margin left) is tried again
//! after a delay that starts at [`EntryReconcilerConfig::retry_min`] and doubles to
//! [`EntryReconcilerConfig::retry_max`], so that a cold tier that is down is not asked about every
//! entry at every step.
//!
//! [`EntryReconciler::run`] steps without waiting while a step makes progress, waits
//! [`EntryReconcilerConfig::idle_min`] while a live owner holds an entry (or this process is not the
//! leader), and otherwise waits a time that starts at `idle_min`, doubles to `idle_max` and starts
//! again at `idle_min` after progress. There is no wake-up: a put into an idle system waits for the
//! wait that is running.
//!
//! No call here probes what a store does: that a hot tier honours `If-Match`, or that a cold tier
//! writes create-only, is a requirement of the stores (see the design).

use std::collections::{BTreeMap, BTreeSet};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{Either, select};
use futures::stream::{self, StreamExt};
use tokio::sync::Notify;

use crate::backend::Backend;
use crate::deadline::millis;
use crate::entry::{Entries, EntryOp};
use crate::format::{FormatPhase, read_format};
use crate::journal::PROXIES_PREFIX;
use crate::lease::Lease;
use crate::registrations::Registrations;
use crate::{Error, ReconcilerConfig, Sleeper, TokioSleeper};

mod backoff;
mod sweep;
mod work;

use backoff::{Idle, retry_delay};
use work::Verdict;

/// The settings of an [`EntryReconciler`]: those of the [`Reconciler`](crate::Reconciler) it shares
/// the lease with, and its own.
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
    /// The shortest wait of the loop.
    pub idle_min: Duration,
    /// The longest wait of the loop when nothing is to be done.
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
    /// The most cold lookups per second of the stray sweep.
    pub stray_rate: u32,
    /// What the loop waits on between steps, and between the lookups of the sweep. The renewal of
    /// the lease waits on `base.sleeper`.
    pub waiter: Arc<dyn Sleeper>,
}

impl EntryReconcilerConfig {
    /// The settings over `base`, with a departure of 60 seconds, an idle wait of 1 to 10 seconds,
    /// a retry delay of 1 to 60 seconds, comparisons in ranges of 1 MiB, a stray grace of 10
    /// minutes, a sweep every 24 hours at up to 100 lookups a second, and a [`TokioSleeper`].
    #[must_use]
    pub fn new(base: ReconcilerConfig) -> Self {
        Self {
            base,
            departure: Duration::from_secs(60),
            idle_min: Duration::from_secs(1),
            idle_max: Duration::from_secs(10),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            compare_range: 1 << 20,
            stray_grace: Duration::from_secs(600),
            stray_interval: Duration::from_secs(24 * 3600),
            stray_rate: 100,
            waiter: Arc::new(TokioSleeper),
        }
    }
}

/// What one [`EntryReconciler::step`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Whether `_tiering/format` said `active`. If not, nothing else was done.
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
    /// The calls that failed, in the order they did.
    pub failed: Vec<Error>,
}

impl StepReport {
    /// Whether the step changed anything: the loop does not wait after one that did.
    #[must_use]
    pub fn progress(&self) -> bool {
        self.changed > 0 || self.strays_adopted > 0
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
        }
    }

    /// How many stray objects this reconciler has adopted, for a gauge.
    #[must_use]
    pub fn strays_adopted(&self) -> u64 {
        self.strays_adopted
    }

    /// Gives the lease up, if it is held, and forgets what was seen.
    pub async fn stop(&mut self) {
        self.lease.release().await;
        self.forget();
    }

    /// Forgets the retries, the registrations seen and the sweep (a new leader sweeps at once): the
    /// lease is not held.
    fn forget(&mut self) {
        self.retries.clear();
        self.registrations = Registrations::default();
        self.sweep_at = None;
    }

    /// Does one step, if the format is active and the lease is held.
    pub async fn step(&mut self) -> StepReport {
        let mut report = StepReport::default();
        match read_format(self.hot.as_ref()).await {
            Ok(Some(format)) if format.phase == FormatPhase::Active => report.active = true,
            Ok(_) => return report,
            Err(error) => {
                report.failed.push(error);
                return report;
            }
        }
        if !self.lease.ensure().await {
            self.forget();
            return report;
        }
        let lease = self.lease.clone();
        let sleeper = Arc::clone(&self.config.base.sleeper);
        lease.renewing(&*sleeper, self.steps(report)).await
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
        let waiting = |path: &String| self.retries.get(path).is_some_and(|r| r.until > now);
        let (backed_off, due): (Vec<String>, Vec<String>) =
            paths.iter().cloned().partition(waiting);
        report.backed_off = backed_off.len();
        self.work(due, now, &mut report).await;
        if self.sweep_due(now) {
            self.sweep(now, &mut report).await;
        }
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
        let this = &*self;
        let steps = stream::iter(items).map(|(path, seen)| async move {
            let verdict = this.process(&seen, now).await;
            (path, seen.entry.key, verdict)
        });
        let worked: Vec<_> = steps.buffer_unordered(parallel).collect().await;
        for (path, error) in failed {
            self.fail(path, now, error, report);
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
                Err(error) => self.fail(path, now, error, report),
            }
        }
    }

    /// Notes that the item at `path` failed at `now`, and when it is tried again.
    fn fail(&mut self, path: String, now: u64, error: Error, report: &mut StepReport) {
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
