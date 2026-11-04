//! One round of a storage node's reconciler, over the two protocols a node of this release has.
//!
//! Until the format object (`_tiering/format`) says `{"epoch": 2, "phase": "active"}` the round is
//! a pass of the [`Reconciler`] over the journals, as in the release before: writes may still go to
//! journals (before the pause, or after a `--cancel`), and something has to apply them. The first
//! round that reads the active format gives the journal reconciler's lease up and from then on the
//! round is a step of the [`EntryReconciler`], which holds the same lease. A node does not go back:
//! once the epoch has been seen, the journals are not read again. A format that cannot be read
//! makes the round fail and do nothing, since which protocol the fleet is on is not known.

use std::sync::Arc;
use std::time::Duration;

use crate::format::{Format, read_format};
use crate::{Backend, EntryReconciler, EntryReconcilerConfig, Error, Reconciler, ReconcilerConfig};

/// What a round did that the loop acts on.
#[derive(Debug, Default)]
pub(crate) struct Round {
    /// The calls that failed, in the order they did.
    pub(crate) failed: Vec<Error>,
    /// Whether the round changed something and there may be more to do now: the loop does not wait
    /// after such a round.
    pub(crate) progress: bool,
}

/// The entry protocol's reconciler and what it needs to know when to start.
struct Entering {
    hot: Arc<dyn Backend>,
    reconciler: EntryReconciler,
    /// Whether the active format was seen. Never reset.
    entered: bool,
}

/// The reconcilers of a node and the choice between them.
pub(crate) struct Rounds {
    journal: Reconciler,
    entries: Option<Entering>,
}

impl From<Reconciler> for Rounds {
    /// The journal protocol only: the format object is never read.
    fn from(journal: Reconciler) -> Self {
        Self {
            journal,
            entries: None,
        }
    }
}

impl Rounds {
    /// Both reconcilers over `hot` and `cold`, with the settings `config`; the entry reconciler
    /// counts a proxy as departed after `departure`, which is the proxies' own `--departure`.
    pub(crate) fn new(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: ReconcilerConfig,
        departure: Duration,
    ) -> Self {
        let journal = Reconciler::new(Arc::clone(&hot), Arc::clone(&cold), config.clone());
        let mut entry_config = EntryReconcilerConfig::new(config);
        entry_config.departure = departure;
        let reconciler = EntryReconciler::new(Arc::clone(&hot), cold, entry_config);
        Self {
            journal,
            entries: Some(Entering {
                hot,
                reconciler,
                entered: false,
            }),
        }
    }

    /// Does one round, as the module says.
    pub(crate) async fn round(&mut self) -> Round {
        if let Some(entering) = self.entries.as_mut() {
            if !entering.entered {
                match read_format(entering.hot.as_ref()).await {
                    Ok(format) if format.as_ref().is_some_and(Format::runs_entries) => {
                        // The journals were applied before the format said `active` (the verb
                        // refuses to write it otherwise), so none is left for the journal
                        // reconciler; its lease is the one the entry reconciler takes next.
                        self.journal.stop().await;
                        entering.entered = true;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Round {
                            failed: vec![error],
                            progress: false,
                        };
                    }
                }
            }
            if entering.entered {
                let step = entering.reconciler.step().await;
                return Round {
                    progress: step.progress(),
                    failed: step.failed,
                };
            }
        }
        let pass = self.journal.pass().await;
        Round {
            failed: pass.failed,
            progress: false,
        }
    }

    /// Gives the lease up, if it is held.
    pub(crate) async fn stop(&mut self) {
        self.journal.stop().await;
        if let Some(entering) = self.entries.as_mut() {
            entering.reconciler.stop().await;
        }
    }
}

#[cfg(test)]
#[path = "rounds_tests.rs"]
mod tests;
