//! [`Gate`]: room of a fixed size shared by the cells in flight, taken by class (4.8). The cell
//! buffer pool counts buffers with one ([`crate::pool`]), and the dirty-byte cap counts the bytes
//! of cells being published with another.
//!
//! **Order.** A taker waits when the room it asks for does not fit, or when a taker of its class
//! or of a better class is already waiting: `demand` before `prepare` before `background`, first
//! come first served within a class. Room that comes back goes to the head of the best class
//! first, and a head that does not fit holds every class behind it, so a demand cell is never
//! passed by a prepare cell that waits on the same gate.
//!
//! **Never stuck on one taker.** A taker is let in whenever the gate is empty, even when it asks
//! for more than the whole room (a dirty cap below one cell), so the gate admits one at a time
//! instead of none.
//!
//! **A taker that stops waiting** (its task was aborted: a hedge's loser, an op that ended) stays
//! queued until room reaches it; the room it is then given has nobody to hold it and comes back
//! at once, to the next in line. So a waiter that left never keeps room.
//!
//! The lock is held only for bookkeeping in memory, never across an await, and room is handed to
//! a waiter (and given back by one that left) after it is released.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tierstore_fetch_api::ids::Class;
use tokio::sync::oneshot;

use crate::engine::class_index;

/// What a gate holds at one moment.
#[derive(Clone, Copy, Default)]
pub(crate) struct Level {
    /// The room held now.
    pub used: u64,
    /// The most room held at once since the gate was made.
    pub peak: u64,
    /// Every unit of room let in since the gate was made.
    pub admitted: u64,
    /// Takers waiting now (one that stopped waiting is counted until room reaches it).
    pub waiting: u64,
    /// Takers that had to wait, since the gate was made.
    pub waits: u64,
}

/// Told every change of a gate's level, under the gate's lock: it may only set gauges and wake
/// waiters of progress.
pub(crate) type Observer = Box<dyn Fn(&Level) + Send + Sync>;

struct Waiter {
    need: u64,
    tx: oneshot::Sender<Room>,
}

struct State {
    level: Level,
    queues: [VecDeque<Waiter>; 3],
}

struct Shared {
    cap: u64,
    state: Mutex<State>,
    observe: Observer,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `need` more fits now.
    fn fits(&self, used: u64, need: u64) -> bool {
        (used == 0) | (used.saturating_add(need) <= self.cap)
    }

    /// Lets `need` in, under the lock.
    fn admit(self: &Arc<Self>, st: &mut State, need: u64) -> Room {
        st.level.used += need;
        st.level.peak = st.level.peak.max(st.level.used);
        st.level.admitted += need;
        Room {
            gate: self.clone(),
            n: need,
        }
    }

    /// Gives `n` back and hands the room to the waiters it now fits, best class first.
    fn release(self: &Arc<Self>, n: u64) {
        let mut granted = Vec::new();
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            st.level.used -= n;
            loop {
                // The head of the best class with a waiter, if its room fits: a head that does
                // not fit holds every class behind it.
                let head = st.queues.iter().position(|q| !q.is_empty());
                let used = st.level.used;
                let fitting = head.and_then(|i| {
                    let need = st.queues[i].front().map_or(0, |w| w.need);
                    self.fits(used, need).then_some((i, need))
                });
                let Some((i, need)) = fitting else {
                    break;
                };
                for w in st.queues[i].pop_front().into_iter() {
                    let room = self.admit(st, need);
                    st.level.waiting -= 1;
                    granted.push((w.tx, room));
                }
            }
            (self.observe)(&st.level);
        }
        // Outside the lock: a waiter that left drops its room here, which comes back through
        // `release` again.
        for (tx, room) in granted {
            let _ = tx.send(room);
        }
    }
}

/// Room held: given back when dropped.
pub(crate) struct Room {
    gate: Arc<Shared>,
    n: u64,
}

impl Drop for Room {
    fn drop(&mut self) {
        let n = std::mem::take(&mut self.n);
        self.gate.release(n);
    }
}

/// Room of `cap` units, taken by class.
pub(crate) struct Gate {
    shared: Arc<Shared>,
}

impl Gate {
    pub(crate) fn new(cap: u64, observe: Observer) -> Self {
        Self {
            shared: Arc::new(Shared {
                cap,
                state: Mutex::new(State {
                    level: Level::default(),
                    queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
                }),
                observe,
            }),
        }
    }

    /// The gate's level now.
    pub(crate) fn level(&self) -> Level {
        self.shared.lock().level
    }

    /// `need` units of room for a taker of `class`, and whether it had to wait for them. The
    /// room is held until it is dropped. It is `None` only if the gate dropped a waiter unsent,
    /// which it never does: it holds each sender until it sends.
    pub(crate) async fn take(&self, class: Class, need: u64) -> (Option<Room>, bool) {
        let i = class_index(class);
        let rx = {
            let mut guard = self.shared.lock();
            let st = &mut *guard;
            let ahead = st.queues[..=i].iter().any(|q| !q.is_empty());
            if !ahead & self.shared.fits(st.level.used, need) {
                let room = self.shared.admit(st, need);
                (self.shared.observe)(&st.level);
                return (Some(room), false);
            }
            let (tx, rx) = oneshot::channel();
            st.queues[i].push_back(Waiter { need, tx });
            st.level.waiting += 1;
            st.level.waits += 1;
            (self.shared.observe)(&st.level);
            rx
        };
        (rx.await.ok(), true)
    }
}
