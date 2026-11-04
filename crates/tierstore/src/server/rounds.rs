//! One round of a storage node's reconciler: a step of the [`EntryReconciler`] and how long to
//! wait before the next.
//!
//! The step does nothing until the format object (`_tiering/format`) says
//! `{"epoch": 2, "phase": "active"}`, and a format that cannot be read makes the round fail and do
//! nothing. The wait is the idle wait of the entry reconciler's own loop: none while a round makes
//! progress, and otherwise one that doubles from `idle_min` to `idle_max`.

use std::sync::Arc;
use std::time::Duration;

use crate::entry_reconciler::Idle;
use crate::{Backend, EntryReconciler, EntryReconcilerConfig, Error, ReconcilerConfig};

/// What one round did that the loop acts on.
#[derive(Debug, Default)]
pub(crate) struct Round {
    /// The calls that failed, in the order they did.
    pub(crate) failed: Vec<Error>,
    /// How long the loop waits before the next round: `None` to go on at once, as the round made
    /// progress and there may be more to do.
    pub(crate) pause: Option<Duration>,
}

/// The reconciler of a node and the pace of its rounds.
pub(crate) struct Rounds {
    reconciler: EntryReconciler,
    idle: Idle,
}

impl Rounds {
    /// A reconciler over `hot` and `cold` with the settings `config`; it counts a proxy as departed
    /// after `departure`, which is the proxies' own `--departure`.
    pub(crate) fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: ReconcilerConfig,
        departure: Duration,
    ) -> Self {
        let mut entry_config = EntryReconcilerConfig::new(config);
        entry_config.departure = departure;
        let idle = Idle::new(entry_config.idle_min, entry_config.idle_max);
        Self {
            reconciler: EntryReconciler::new(hot, cold, entry_config),
            idle,
        }
    }

    /// Does one round, as the module says.
    pub(crate) async fn round(&mut self) -> Round {
        let step = self.reconciler.step().await;
        let pause = self.idle.after(&step);
        Round {
            failed: step.failed,
            pause,
        }
    }

    /// Gives the lease up, if it is held.
    pub(crate) async fn stop(&mut self) {
        self.reconciler.stop().await;
    }
}

#[cfg(test)]
#[path = "rounds_tests.rs"]
mod tests;
