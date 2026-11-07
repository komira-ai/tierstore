//! The eviction, the copy-back and the stray sweep of the leader over markers
//! (`docs/design/put-without-lock.md`, 5.10 to 5.12), as the step of an [`EntryReconciler`] that
//! works the markers runs them. They are the passes of `marks_evict`, `marks_copy_back` and
//! `marks_sweep`, built by [`EntryReconciler::with_marks`] over the lease the reconciler holds
//! (`LeaderLease::of`) and the pairs of tags the copier records, so that one lease and one memory
//! of copies serve every phase of a step and no second leader can be acting beside the first. Each
//! pass reads the format object itself and does nothing unless it still names the marks bucket at
//! epoch 3 or later; a pass that finds it changed ends the step's work of its kind.
//!
//! A step over the markers does, in order: the pass over the markers (the module `marks`: copy,
//! hold, reap), whose listing also finds the `WANT` markers; then
//!
//! - **the copy-back**, if the listing found a `WANT` and was whole: the asks are given to
//!   [`CopyBacker::pass`] all at once, because it ranks them by when it first saw each, drops the
//!   oldest beyond `copy_back_max` and those past `copy_back_ttl`, and forgets the asks no longer
//!   open. A copy-back that fails releases its lock, which removes the ask (a later read asks
//!   again), and is reported;
//! - **the stray sweep**, at takeover and every `stray_interval` ([`StraySweep::due`]);
//! - the usage (the module `usage`), and then **the eviction**, if the figure is above the high
//!   water or a cycle is open: batches of the candidates of the last listing of the hot tier, oldest
//!   first, go to [`MarkEvictor::evict_pass`] until the figure is at the low water or no candidate
//!   is left. A key whose cold copy is younger than `evict_min_age` is not evicted and not offered
//!   again until the next listing, which the eviction then asks for in `evict_min_age` instead of
//!   at the end of `usage_scan_interval`. A key whose eviction failed goes back to the front of the
//!   candidates.
//!
//! A leader that loses the lease, or that finds the format outside `active-marks` for a step,
//! starts the passes afresh (they forget their asks, their records and their sweep) the next time
//! it works the markers; the lease is the reconciler's own, so a change of phase never leaves two
//! leases of one holder to wait for each other.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Arc;

use super::marks::MarkSide;
use super::usage::Candidate;
use super::{EntryReconciler, StepReport};
use crate::deadline::millis;
use crate::marks_leader::LeaderLease;
use crate::{
    Backend, CopyBackConfig, CopyBacker, CopyRecords, EvictConfig, MarkEvictor, MarkSeen, Markers,
    StraySweep, SweepConfig, SweepTime,
};

/// The leader's eviction, copy-back and stray sweep over markers.
pub(super) struct Passes {
    evictor: MarkEvictor,
    copy_back: CopyBacker,
    sweep: StraySweep,
}

impl Passes {
    /// The leader took over, or came back to the markers: the passes forget what they knew.
    pub(super) fn took_over(&mut self) {
        self.evictor.took_over();
        self.copy_back.took_over();
        self.sweep.took_over();
    }
}

impl EntryReconciler {
    /// The passes over the marks bucket `bucket` of `store`, whose markers this reconciler makes
    /// (`markers`): over its lease, its settings and `records`.
    pub(super) fn passes(
        &self,
        markers: &Markers,
        store: &Arc<dyn Backend>,
        bucket: &str,
        records: &Arc<CopyRecords>,
    ) -> Passes {
        let config = &self.config;
        let base = &config.base;
        let lease = LeaderLease::of(self.lease.clone());
        let evict = EvictConfig {
            evict_min_age: config.evict_min_age,
            tag_record_limit: config.tag_record_limit,
            compare_range: NonZeroU64::new(config.compare_range).unwrap_or(NonZeroU64::MIN),
        };
        let evictor = MarkEvictor::new(
            Arc::clone(&self.hot),
            Arc::clone(&self.cold),
            markers.alongside(),
            bucket,
            lease.clone(),
            Arc::clone(records),
            Arc::clone(&base.mono),
            evict,
        );
        let copy_back = CopyBackConfig {
            copy_back_ttl: config.copy_back_ttl,
            copy_back_max: config.copy_back_max,
            copy_back_max_bytes: config.copy_back_max_bytes,
            ..CopyBackConfig::default()
        };
        let copy_back = CopyBacker::new(
            Arc::clone(&self.hot),
            Arc::clone(&self.cold),
            markers.alongside(),
            bucket,
            lease,
            Arc::clone(&base.mono),
            copy_back,
        );
        let sweep = SweepConfig {
            stray_grace: config.stray_grace,
            stray_interval: config.stray_interval,
            stray_rate: config.stray_rate,
            abandoned_upload_after: config.abandoned_upload_after,
            page_size: base.page_size,
        };
        let time = SweepTime {
            clock: Arc::clone(&base.clock),
            mono: Arc::clone(&base.mono),
            waiter: Arc::clone(&config.waiter),
        };
        let sweep = StraySweep::new(
            Arc::clone(&self.hot),
            Arc::clone(&self.cold),
            Arc::clone(store),
            bucket,
            markers.alongside(),
            Arc::clone(records),
            time,
            sweep,
        );
        Passes {
            evictor,
            copy_back,
            sweep,
        }
    }

    /// Copies back the objects of the `WANT` markers `wants` that the listing of the step found,
    /// if there are any. A marker that was copied, dropped, found nothing to copy or lost to a
    /// raced put is a change; one that moved under the pass is counted as pinned.
    pub(super) async fn mark_copy_back(
        &self,
        side: &MarkSide,
        wants: Vec<MarkSeen>,
        report: &mut StepReport,
    ) {
        if wants.is_empty() {
            return;
        }
        let parallel = self.config.base.parallel.max(1);
        let mut passes = side.passes.lock().await;
        let done = passes.copy_back.pass(wants, parallel).await;
        report.changed += done.landed + done.dropped + done.nothing + done.raced;
        report.pinned += done.lost;
        report
            .failed
            .extend(done.failed.into_iter().map(|(_, error)| error));
    }

    /// Sweeps the hot tier for strays and abandoned uploads if the sweep is due: at takeover and
    /// every `stray_interval`.
    pub(super) async fn mark_sweep(&mut self, side: &MarkSide, report: &mut StepReport) {
        let mut passes = side.passes.lock().await;
        if !passes.sweep.due() {
            return;
        }
        let done = passes.sweep.run().await;
        self.strays_adopted += done.adopted as u64;
        report.strays_adopted += done.adopted;
        report.uploads_aborted += done.uploads_aborted;
        report.failed.extend(done.failed);
    }

    /// Evicts, if the figure says the hot tier holds more than the high water of its budget: a
    /// batch of candidates after another on the pool, until it holds no more than the low water,
    /// no candidate is left, a call fails (those keys are kept for the next step) or the format no
    /// longer opens the markers to this leader. `now` is the start of the step on the monotonic
    /// clock.
    pub(super) async fn mark_evict(&mut self, side: &MarkSide, now: u64, report: &mut StepReport) {
        let Some(mut bytes) = self.books.bytes else {
            return;
        };
        let budget = self.config.base.hot_budget as f64;
        let high = (budget * self.config.base.high_water) as u64;
        let low = (budget * self.config.base.low_water) as u64;
        // A cycle opens above the high water and stays open, across steps, until the low water.
        if bytes <= high && !self.books.evicting {
            return;
        }
        let parallel = self.config.base.parallel.max(1);
        let passes = side.passes.lock().await;
        let mut waiting = false;
        while bytes > low {
            let count = parallel.min(self.books.candidates.len());
            let keys: Vec<String> = self
                .books
                .candidates
                .iter()
                .take(count)
                .map(|candidate| candidate.1.clone())
                .collect();
            if keys.is_empty() {
                break;
            }
            let done = passes.evictor.evict_pass(keys, parallel).await;
            if !done.active && done.failed.is_empty() {
                break;
            }
            let mut batch: BTreeMap<String, Candidate> = self
                .books
                .candidates
                .drain(..count)
                .map(|candidate| (candidate.1.clone(), candidate))
                .collect();
            bytes = bytes.saturating_sub(done.freed);
            report.evicted += done.evicted;
            report.changed += done.evicted + done.dirtied;
            report.failed.extend(done.unreleased);
            waiting |= !done.waiting.is_empty();
            for key in &done.removed {
                let tally = &self.tally;
                batch
                    .remove(key)
                    .into_iter()
                    .for_each(|(modified, _, size)| tally.evicted(key, size, modified));
            }
            let failed = !done.failed.is_empty();
            for (key, error) in done.failed {
                report.failed.push(error);
                let candidates = &mut self.books.candidates;
                batch
                    .remove(&key)
                    .into_iter()
                    .for_each(|candidate| candidates.push_front(candidate));
            }
            if failed {
                break;
            }
        }
        if waiting {
            self.books
                .rescan_by(now.saturating_add(millis(self.config.evict_min_age)));
        }
        self.books.bytes = Some(bytes);
        self.books.evicting = bytes > low;
        report.evicting = self.books.evicting;
    }
}
