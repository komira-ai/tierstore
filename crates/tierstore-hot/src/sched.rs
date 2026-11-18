//! `HotSched`: the hot device's I/O queue (10.7). A bounded queue depth (`hot.queue_depth`
//! requests run at once, each on one of as many dedicated I/O threads, never the async
//! runtime's), the class order `demand` > `write` > `prepare` > `background` with demand first
//! whenever a thread frees, and the stall watchdog of the drive scheduler: a blocking call
//! outstanding longer than `stall_after` (on the injected clock) puts the device in `stalled`,
//! every queued request fails at once and new ones are refused, until every such call has
//! returned. It has no slices, streams or preemption: an NVMe seek costs nothing.
//!
//! [`HotSched::inline`] runs each call on the caller instead of a thread (one at a time, in
//! arrival order), which is what a deterministic simulation needs; the watchdog and the report
//! are the same.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::upstream::DriveState;
use tierstore_store_api::sched::{DriveClass, DriveReport};
use tokio::sync::oneshot;

/// Why `HotSched` did not run a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SchedError {
    /// The device is `stalled`: queued calls fail at once and new ones are refused (`503`).
    #[error("the hot device is stalled")]
    Stalled,
}

type Job = Box<dyn FnOnce() + Send>;

/// The answer slot of one queued call: taken once, by the job or by the stall.
type Slot<T> = Arc<Mutex<Option<oneshot::Sender<Result<T, SchedError>>>>>;

struct Queued {
    job: Job,
    /// Fails the waiting caller when the device stalls before the job ran.
    fail: Box<dyn FnOnce() + Send>,
}

#[derive(Default)]
struct State {
    queues: [VecDeque<Queued>; 4],
    /// The calls running: id to the time they started.
    running: BTreeMap<u64, u64>,
    next_id: u64,
    stalled: bool,
    stalls: u64,
    shutdown: bool,
}

struct Inner {
    clock: Arc<dyn Clock>,
    stall_after_ms: u64,
    threads: usize,
    state: Mutex<State>,
    ready: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(&self, st: &mut State) -> u64 {
        let id = st.next_id;
        st.next_id += 1;
        st.running.insert(id, self.clock.now_ms());
        id
    }

    fn worker(&self) {
        let mut st = self.lock();
        loop {
            if let Some(q) = st.queues.iter_mut().find_map(VecDeque::pop_front) {
                let id = self.start(&mut st);
                drop(st);
                (q.job)();
                st = self.lock();
                st.running.remove(&id);
                continue;
            }
            if st.shutdown {
                return;
            }
            st = self.ready.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The hot device's I/O queue; clones share it.
#[derive(Clone)]
pub struct HotSched {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for HotSched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotSched")
            .field("threads", &self.inner.threads)
            .finish_non_exhaustive()
    }
}

fn class_index(class: DriveClass) -> usize {
    match class {
        DriveClass::Demand => 0,
        DriveClass::Write => 1,
        DriveClass::Prepare => 2,
        DriveClass::Background => 3,
    }
}

impl HotSched {
    /// A queue running `depth` calls at once on `depth` dedicated threads.
    pub fn new(depth: usize, stall_after_ms: u64, clock: Arc<dyn Clock>) -> std::io::Result<Self> {
        let sched = Self::make(depth.max(1), stall_after_ms, clock);
        for i in 0..sched.inner.threads {
            let inner = Arc::clone(&sched.inner);
            thread::Builder::new()
                .name(format!("hot-io-{i}"))
                .spawn(move || inner.worker())?;
        }
        Ok(sched)
    }

    /// A queue that runs each call on its caller, one at a time (deterministic tests).
    pub fn inline(stall_after_ms: u64, clock: Arc<dyn Clock>) -> Self {
        Self::make(0, stall_after_ms, clock)
    }

    fn make(threads: usize, stall_after_ms: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::new(Inner {
                clock,
                stall_after_ms,
                threads,
                state: Mutex::new(State::default()),
                ready: Condvar::new(),
            }),
        }
    }

    /// Runs `op` as a call of `class` and returns what it returned.
    pub async fn run<T, F>(&self, class: DriveClass, op: F) -> Result<T, SchedError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        if self.inner.threads == 0 {
            let id = {
                let mut st = self.inner.lock();
                if st.stalled {
                    return Err(SchedError::Stalled);
                }
                self.inner.start(&mut st)
            };
            let out = op();
            self.inner.lock().running.remove(&id);
            return Ok(out);
        }
        let (tx, rx) = oneshot::channel::<Result<T, SchedError>>();
        let tx: Slot<T> = Arc::new(Mutex::new(Some(tx)));
        let fail_tx = Arc::clone(&tx);
        let send = move |tx: &Slot<T>, r| {
            // The job and the stall are exclusive, so the slot is full here; the caller may have
            // gone away, and nobody then waits for the answer.
            let sender = tx.lock().unwrap_or_else(PoisonError::into_inner).take();
            drop(sender.map(|tx| tx.send(r)));
        };
        let queued = Queued {
            job: Box::new(move || send(&tx, Ok(op()))),
            fail: Box::new(move || send(&fail_tx, Err(SchedError::Stalled))),
        };
        {
            let mut st = self.inner.lock();
            if st.stalled {
                return Err(SchedError::Stalled);
            }
            st.queues[class_index(class)].push_back(queued);
        }
        self.inner.ready.notify_one();
        rx.await.unwrap_or(Err(SchedError::Stalled))
    }

    /// The stall watchdog (run every second by the node's tick): a call outstanding longer than
    /// `stall_after` puts the device in `stalled` and fails every queued call; the device leaves
    /// `stalled` once no call is that old. Returns the state after the check.
    pub fn watchdog(&self) -> DriveState {
        let now = self.inner.clock.now_ms();
        let (state, failed): (DriveState, Vec<Queued>) = {
            let mut st = self.inner.lock();
            let stuck = st
                .running
                .values()
                .any(|&t| now.saturating_sub(t) > self.inner.stall_after_ms);
            if stuck && !st.stalled {
                st.stalls += 1;
            }
            st.stalled = stuck;
            if stuck {
                let failed = st.queues.iter_mut().flat_map(|q| q.drain(..)).collect();
                (DriveState::Stalled, failed)
            } else {
                (DriveState::Up, Vec::new())
            }
        };
        for q in failed {
            (q.fail)();
        }
        state
    }

    /// The queue's report (the hot device's part of `GET /store/v1/ping` and `x-ts-load`).
    pub fn report(&self) -> DriveReport {
        let now = self.inner.clock.now_ms();
        let st = self.inner.lock();
        let mut queued = [0u32; 4];
        for (n, q) in queued.iter_mut().zip(&st.queues) {
            *n = q.len() as u32;
        }
        let oldest = st.running.values().min().map(|&t| now.saturating_sub(t));
        let stuck = st
            .running
            .values()
            .filter(|&&t| now.saturating_sub(t) > self.inner.stall_after_ms)
            .count();
        DriveReport {
            state: Some(if st.stalled {
                DriveState::Stalled
            } else {
                DriveState::Up
            }),
            oldest_call_ms: oldest,
            queued,
            streams: st.running.len() as u32,
            writes: 0,
            parked: 0,
            stuck_threads: stuck as u32,
        }
    }

    /// Times the device went `stalled`.
    pub fn stalls(&self) -> u64 {
        self.inner.lock().stalls
    }
}

impl Drop for HotSched {
    fn drop(&mut self) {
        // The last handle stops the threads once their queues are empty.
        if Arc::strong_count(&self.inner) == 1 + self.inner.threads {
            self.inner.lock().shutdown = true;
            self.inner.ready.notify_all();
        }
    }
}
