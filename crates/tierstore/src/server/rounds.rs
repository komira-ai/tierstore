//! One round of a storage node's reconciler: a step of the [`EntryReconciler`] and how long to
//! wait before the next.
//!
//! A round does nothing until the format object (`_tiering/format`) says
//! `{"epoch": 2, "phase": "active"}` or `{"phase": "active-marks"}` (the reconciler then works the
//! markers if it was given the marks bucket the record names, and does nothing otherwise), and a
//! format that cannot be read makes the round fail and do nothing. The wait is the idle wait of the entry reconciler's own loop: none while a round makes
//! progress, and otherwise one that doubles from `idle_min` to `idle_max`.

use std::sync::Arc;
use std::time::Duration;

use super::config_entry::EntryTuning;
use crate::entry_reconciler::Idle;
use crate::format::{Format, read_format};
use crate::{Backend, EntryReconciler, EntryReconcilerConfig, Error, ReconcilerConfig, StepReport};

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
    hot: Arc<dyn Backend>,
    reconciler: EntryReconciler,
    idle: Idle,
}

impl Rounds {
    /// A reconciler over `hot` and `cold` with the settings `config` and `tuning` (`[hot_store]`'s
    /// entry keys); it counts a proxy as departed after `departure`, which is the proxies' own
    /// `--departure`. `marks` is the name and the handle of the marks bucket of the hot store, if
    /// the node has one: the markers are worked only under a format that names it.
    ///
    /// # Errors
    /// The text of the refusal of the markers' settings, which the loader has checked already.
    pub(crate) fn tuned(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        marks: Option<(String, Arc<dyn Backend>)>,
        config: ReconcilerConfig,
        departure: Duration,
        tuning: &EntryTuning,
    ) -> Result<Self, String> {
        let mut entry_config = EntryReconcilerConfig::new(config);
        entry_config.departure = departure;
        tuning.apply_reconciler(&mut entry_config);
        let idle = Idle::new(entry_config.idle_min, entry_config.idle_max);
        let mut reconciler = EntryReconciler::new(Arc::clone(&hot), cold, entry_config);
        if let Some((bucket, store)) = marks {
            reconciler = reconciler
                .with_marks(store, bucket)
                .map_err(|error| error.to_string())?;
        }
        Ok(Self {
            reconciler,
            hot,
            idle,
        })
    }

    /// [`Rounds::tuned`] with the default settings of the entry reconciler.
    #[cfg(test)]
    pub(crate) fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: ReconcilerConfig,
        departure: Duration,
    ) -> Self {
        let tuning = EntryTuning::default();
        Self::tuned(hot, cold, None, config, departure, &tuning).expect("no marks, no refusal")
    }

    /// Does one round, as the module says.
    pub(crate) async fn round(&mut self) -> Round {
        // The entry reconciler goes on any `active` format; a node works the entries only from
        // epoch 2, so the epoch is looked at here. It goes on `active-marks` too, and decides
        // there whether it has the marks bucket the record names.
        match read_format(self.hot.as_ref()).await {
            Ok(format) if format.as_ref().is_some_and(Format::runs_a_pass) => {}
            Ok(_) => return self.waiting(Vec::new()),
            Err(error) => return self.waiting(vec![error]),
        }
        let step = self.reconciler.step().await;
        let pause = self.idle.after(&step);
        Round {
            failed: step.failed,
            pause,
        }
    }

    /// A round that did nothing, with the calls that failed in it.
    fn waiting(&mut self, failed: Vec<Error>) -> Round {
        let pause = self.idle.after(&StepReport::default());
        Round { failed, pause }
    }

    /// Gives the lease up, if it is held.
    ///
    /// It waits for the release no longer than the reconciler's `stop_wait`
    /// (`hot_store.stop_wait`): a hot tier that does not answer does not keep the process from
    /// leaving.
    pub(crate) async fn stop(&mut self) {
        let wait = self.reconciler.stop_wait();
        let _ = tokio::time::timeout(wait, self.reconciler.stop()).await;
    }
}

#[cfg(test)]
#[path = "rounds_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "rounds_marks_tests.rs"]
mod marks_tests;
