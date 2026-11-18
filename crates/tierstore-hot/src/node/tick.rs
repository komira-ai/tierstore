//! The node's periodic step, on the injected clock (the daemon runs it every second; a test
//! moves the clock and runs it): the device watchdog, seal-and-roll on a stalled, `down` or
//! `out` replica (10.3 step 5), the seal triggers (step 6), seals that did not commit retried,
//! and large-PUT replicas lagging past `hot.put_copy_stall` abandoned.
//!
//! **Seal triggers.** A segment seals by size when the next record does not fit (the
//! committer's own check), and by age: past `hot.seal_max_age` only if it holds at least
//! `hot.seal_min_bytes`, otherwise once it is older than `destage.max_age`, so a slow expiry
//! class does not make a stream of tiny blocks. An empty segment is never sealed.

use std::sync::Arc;

use futures::FutureExt;
use tierstore_fetch_api::upstream::DriveState;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_index::backend::Backend;
use tierstore_store_api::hot::SealReason;
use tierstore_store_api::view::{Admin, NodeState};

use super::writer::{Close, Seg};
use super::{HotNode, sealer};

/// What a tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// The hot device's state after the watchdog.
    pub device: Option<DriveState>,
    /// Segments sealed early: a replica stalled for `replica_stall`, or `down` or `out`.
    pub early: Vec<BlockId>,
    /// Segments sealed by age.
    pub aged: Vec<BlockId>,
    /// Closed segments whose seal had not committed, sealed again.
    pub retried: Vec<BlockId>,
    /// Large-PUT replicas abandoned for lagging.
    pub abandoned: Vec<NodeId>,
}

impl<B: Backend + 'static> HotNode<B> {
    /// One periodic step at the clock's current time.
    pub fn tick(&self) -> TickReport {
        let n = &self.n;
        let now = n.clock.now_ms();
        let cfg = &n.cfg;
        let mut report = TickReport {
            device: Some(n.sched.watchdog()),
            ..TickReport::default()
        };
        let view = n.view();
        let gone: Vec<NodeId> = view
            .nodes
            .iter()
            .filter(|v| v.state == NodeState::Down || v.hot == Admin::Out)
            .map(|v| v.id)
            .collect();
        let (open, writing, unsealed): (Vec<Arc<Seg>>, Vec<Arc<Seg>>, Vec<BlockId>) = {
            let st = n.lock();
            (
                st.open.values().cloned().collect(),
                st.writing.values().cloned().collect(),
                st.unsealed.iter().copied().collect(),
            )
        };
        for seg in &open {
            let (stalled, written) = {
                let s = seg.lock();
                let stalled = s.links.iter().any(|(node, l)| {
                    gone.contains(node)
                        || l
                            .since
                            .is_some_and(|t| now.saturating_sub(t) >= cfg.replica_stall_ms)
                });
                (stalled, s.written)
            };
            let age = now.saturating_sub(seg.created_ms);
            if stalled {
                n.roll(seg, Close::Early);
                report.early.push(seg.id);
            } else if written > 0
                && ((age >= cfg.seal_max_age_ms && written >= cfg.seal_min_bytes)
                    || age >= cfg.destage_max_age_ms)
            {
                n.roll(seg, Close::Sealing);
                report.aged.push(seg.id);
            }
        }
        for seg in writing {
            let closed_at = seg.lock().closed_at;
            if closed_at.is_some_and(|t| now.saturating_sub(t) >= cfg.replica_stall_ms)
                && !report.early.contains(&seg.id)
            {
                seg.lock().closed_at = Some(now);
                self.reseal(seg.id, SealReason::Early);
                report.retried.push(seg.id);
            }
        }
        for id in unsealed {
            self.reseal(id, SealReason::TakeOver);
            report.retried.push(id);
        }
        let st = n.lock();
        for tee in st.large.values() {
            for (node, since) in &tee.blocked {
                if now.saturating_sub(*since) >= cfg.put_copy_stall_ms
                    && let Some(signal) = tee.abandon.get(node)
                {
                    signal.notify_one();
                    report.abandoned.push(*node);
                }
            }
        }
        report
    }

    fn reseal(&self, id: BlockId, reason: SealReason) {
        let n = Arc::clone(&self.n);
        self.n.spawn.spawn(
            async move {
                // Retried at the next tick if it cannot commit now.
                let _ = sealer::seal_segment(&n, id, reason).await;
            }
            .boxed(),
        );
    }
}
