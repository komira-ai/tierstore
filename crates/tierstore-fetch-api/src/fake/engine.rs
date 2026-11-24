//! A fake engine with scripted waits.

use std::collections::BTreeMap;
use std::sync::Mutex;

use futures::channel::oneshot;

use super::lock;
use crate::engine::{Engine, PackSpec, WaitOutcome};
use crate::ids::{ByteRange, CellRun, Class, JobId, PackId, Rank};

/// One `ensure` the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ensured {
    /// The pack.
    pub pack: PackId,
    /// The runs.
    pub runs: Vec<CellRun>,
    /// The class.
    pub class: Class,
    /// The job.
    pub job: JobId,
    /// The rank.
    pub rank: Rank,
}

#[derive(Debug, Default)]
struct State {
    outcomes: BTreeMap<PackId, WaitOutcome>,
    parked: BTreeMap<PackId, Vec<oneshot::Sender<WaitOutcome>>>,
    parking: BTreeMap<PackId, bool>,
    ensures: Vec<Ensured>,
    waits: Vec<(PackId, ByteRange, JobId)>,
}

/// An engine whose waits answer a scripted outcome per pack (`Ready` when none is set), or park
/// until [`FakeEngine::complete`] when the pack is set to park. `ensure` and `wait` are logged.
#[derive(Debug, Default)]
pub struct FakeEngine {
    state: Mutex<State>,
}

impl FakeEngine {
    /// An engine whose every wait is ready.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The outcome of every later wait on `pack`; waits already parked on it are answered too.
    pub fn set_outcome(&self, pack: PackId, outcome: WaitOutcome) {
        self.complete(&pack, outcome);
    }

    /// Later waits on `pack` park until [`FakeEngine::complete`].
    pub fn park(&self, pack: PackId) {
        lock(&self.state).parking.insert(pack, true);
    }

    /// Completes every parked wait on `pack` with `outcome`, which also answers later waits;
    /// the number of waits completed.
    pub fn complete(&self, pack: &PackId, outcome: WaitOutcome) -> usize {
        let waiters = {
            let mut s = lock(&self.state);
            s.parking.remove(pack);
            s.outcomes.insert(pack.clone(), outcome);
            s.parked.remove(pack).unwrap_or_default()
        };
        waiters
            .into_iter()
            .map(|w| w.send(outcome))
            .filter(Result::is_ok)
            .count()
    }

    /// Answers every wait parked on `pack` now with `outcome` and keeps the pack parking, in one
    /// step under the lock: a wait that comes after it parks and is never answered at once, as it
    /// could be between a [`complete`](Self::complete) and the [`park`](Self::park) after it. The
    /// number of waits answered.
    pub fn answer_parked(&self, pack: &PackId, outcome: WaitOutcome) -> usize {
        let waiters = {
            let mut s = lock(&self.state);
            s.outcomes.insert(pack.clone(), outcome);
            s.parked.remove(pack).unwrap_or_default()
        };
        waiters
            .into_iter()
            .map(|w| w.send(outcome))
            .filter(Result::is_ok)
            .count()
    }

    /// The waits parked on `pack` now: the progress event a test waits for.
    #[must_use]
    pub fn parked(&self, pack: &PackId) -> usize {
        lock(&self.state).parked.get(pack).map_or(0, Vec::len)
    }

    /// Every `ensure` so far, in order.
    #[must_use]
    pub fn ensures(&self) -> Vec<Ensured> {
        lock(&self.state).ensures.clone()
    }

    /// Every `wait` so far, in order.
    #[must_use]
    pub fn waits(&self) -> Vec<(PackId, ByteRange, JobId)> {
        lock(&self.state).waits.clone()
    }
}

#[async_trait::async_trait]
impl Engine for FakeEngine {
    fn ensure(&self, pack: &PackSpec, runs: &[CellRun], class: Class, job: JobId, rank: Rank) {
        lock(&self.state).ensures.push(Ensured {
            pack: pack.id.clone(),
            runs: runs.to_vec(),
            class,
            job,
            rank,
        });
    }

    async fn wait(&self, pack: &PackSpec, range: ByteRange, job: JobId) -> WaitOutcome {
        let parked = {
            let mut s = lock(&self.state);
            s.waits.push((pack.id.clone(), range, job));
            if s.parking.contains_key(&pack.id) {
                let (tx, rx) = oneshot::channel();
                s.parked.entry(pack.id.clone()).or_default().push(tx);
                Err(rx)
            } else {
                Ok(s.outcomes
                    .get(&pack.id)
                    .copied()
                    .unwrap_or(WaitOutcome::Ready))
            }
        };
        match parked {
            Ok(outcome) => outcome,
            // The sender is dropped only with the engine; a dropped engine loses the fill.
            Err(rx) => rx.await.unwrap_or(WaitOutcome::Lost),
        }
    }
}
