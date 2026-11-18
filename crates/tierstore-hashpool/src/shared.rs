//! The pool's shared state: admission of cells under the bound, the two queues the threads take
//! from, and the counts. No lock here is held across a hash or an await.
//!
//! **Admission.** A cell holds one of [`QUEUE_CELLS`] slots from the moment it is admitted until
//! its hash is done. A fill is admitted while a slot is free; a recheck cell also needs fewer than
//! [`RECHECK_CELLS`] recheck cells held. Otherwise the caller waits (the receive pause of 4.7). A slot that
//! frees is handed to the first waiter, fills before recheck cells and first come first within
//! each, so a later arrival never takes a slot from a waiter, and a dropped waiter hands on a
//! slot it was given.
//!
//! **Order of hashing.** A thread takes the oldest queued fill, else the oldest recheck cell.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use futures::channel::oneshot;
use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::verify::{CellJob, Verdict, VerifyPriority};

use crate::plan::{QUEUE_CELLS, RECHECK_CELLS};

/// A count kept separately for fills and recheck cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ByPriority {
    /// Cells of fills.
    pub fill: u64,
    /// `recheck` cells.
    pub recheck: u64,
}

impl ByPriority {
    fn bump(&mut self, priority: VerifyPriority) {
        match priority {
            VerifyPriority::Fill => self.fill += 1,
            VerifyPriority::Recheck => self.recheck += 1,
        }
    }
}

/// What the pool is doing and has done, for the metrics of section 12 (T9 exports them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PoolStats {
    /// Cells being hashed now (`hash_pool_busy`).
    pub busy: usize,
    /// Cells admitted and waiting for a thread (`verify_queue_depth`).
    pub queued: usize,
    /// Cells holding a slot: queued, being hashed, or handed a slot by a release and not yet
    /// queued by their caller. Never above [`QUEUE_CELLS`].
    pub held: usize,
    /// Recheck cells among `held`. Never above [`RECHECK_CELLS`].
    pub held_recheck: usize,
    /// The most cells `held` has been.
    pub high_water: usize,
    /// `verify` calls waiting for a slot now.
    pub waiting: usize,
    /// `verify` calls that had to wait for a slot, by priority.
    pub paused: ByPriority,
    /// The milliseconds of the injected clock that waiting calls spent before their slot
    /// (`part_hash_wait_seconds`), counted when each is handed its slot.
    pub pause_wait_ms: u64,
    /// Cells hashed, by priority.
    pub verified: ByPriority,
    /// Cells whose sha256 was not the table's (`verify_failures_total`), by priority.
    pub mismatched: ByPriority,
}

/// A cell admitted and queued for a thread, with the channel its verdict goes back on.
pub(crate) struct Queued {
    job: CellJob,
    reply: oneshot::Sender<Verdict>,
}

/// A waiter's place: priority first (fills sort first), then arrival.
type Ticket = (VerifyPriority, u64);

#[derive(Default)]
struct State {
    held_fill: usize,
    held_recheck: usize,
    high_water: usize,
    fills: VecDeque<Queued>,
    rechecks: VecDeque<Queued>,
    busy: usize,
    /// Waiters not yet handed a slot, each with the time it began to wait and its last waker.
    waiting: BTreeMap<Ticket, (u64, Waker)>,
    /// Waiters handed a slot that have not yet seen it. The slot is counted in `held_*`.
    granted: BTreeSet<Ticket>,
    next_ticket: u64,
    shutdown: bool,
    paused: ByPriority,
    pause_wait_ms: u64,
    verified: ByPriority,
    mismatched: ByPriority,
}

/// Whether one more cell of `priority` fits beside `held_fill` fills and `held_recheck` recheck
/// cells.
fn fits(held_fill: usize, held_recheck: usize, priority: VerifyPriority) -> bool {
    held_fill + held_recheck < QUEUE_CELLS
        && (priority == VerifyPriority::Fill || held_recheck < RECHECK_CELLS)
}

impl State {
    fn held(&self) -> usize {
        self.held_fill + self.held_recheck
    }

    /// Whether a cell of `priority` fits the bound now.
    ///
    /// A new arrival that fits is admitted at once without passing a waiter it should not: while
    /// anyone waits, the first waiter does not fit, so either the pool is full (nothing fits) or
    /// the first waiter is a recheck cell at [`RECHECK_CELLS`] (no recheck cell fits, and a fill
    /// may pass recheck cells). [`State::grant`] keeps that true: it runs whenever a slot frees.
    fn fits(&self, priority: VerifyPriority) -> bool {
        fits(self.held_fill, self.held_recheck, priority)
    }

    fn take_slot(&mut self, priority: VerifyPriority) {
        match priority {
            VerifyPriority::Fill => self.held_fill += 1,
            VerifyPriority::Recheck => self.held_recheck += 1,
        }
        self.high_water = self.high_water.max(self.held());
    }

    fn free_slot(&mut self, priority: VerifyPriority) {
        match priority {
            VerifyPriority::Fill => self.held_fill -= 1,
            VerifyPriority::Recheck => self.held_recheck -= 1,
        }
    }

    /// Hands free slots to the first waiters while they fit, and returns their wakers (woken by
    /// the caller once the lock is released).
    fn grant(&mut self, now_ms: u64) -> Vec<Waker> {
        let mut wake = Vec::new();
        while let Some(entry) = self.waiting.first_entry() {
            let ticket = *entry.key();
            if !fits(self.held_fill, self.held_recheck, ticket.0) {
                break;
            }
            let (since, waker) = entry.remove();
            self.take_slot(ticket.0);
            self.pause_wait_ms += now_ms.saturating_sub(since);
            self.granted.insert(ticket);
            wake.push(waker);
        }
        wake
    }
}

/// The shared state of a pool and of its threads.
pub(crate) struct Core {
    state: Mutex<State>,
    /// Signalled when a cell is queued or the pool shuts down.
    work: Condvar,
    clock: Arc<dyn Clock>,
}

impl Core {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            work: Condvar::new(),
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn stats(&self) -> PoolStats {
        let s = self.lock();
        PoolStats {
            busy: s.busy,
            queued: s.fills.len() + s.rechecks.len(),
            held: s.held(),
            held_recheck: s.held_recheck,
            high_water: s.high_water,
            waiting: s.waiting.len(),
            paused: s.paused,
            pause_wait_ms: s.pause_wait_ms,
            verified: s.verified,
            mismatched: s.mismatched,
        }
    }

    /// The verdict on `job`: waits for a slot, queues the cell for a thread, and waits for it.
    /// The bytes come back as the caller's handle alone once the thread has dropped its own.
    pub(crate) async fn verify(&self, job: CellJob) -> (Verdict, Bytes) {
        Admit {
            core: self,
            priority: job.priority,
            ticket: None,
        }
        .await;
        let bytes = job.bytes.clone();
        let (reply, verdict) = oneshot::channel();
        let queue = Queued { job, reply };
        {
            let mut s = self.lock();
            match queue.job.priority {
                VerifyPriority::Fill => s.fills.push_back(queue),
                VerifyPriority::Recheck => s.rechecks.push_back(queue),
            }
        }
        self.work.notify_one();
        // A cell's verdict is lost only if the thread hashing it died; a lost verdict is never a
        // match, so the cell takes the refetch path of 4.6.
        let verdict = verdict.await.unwrap_or(Verdict::Mismatch);
        (verdict, bytes)
    }

    /// The next cell for a thread, waiting for one; `None` once the pool shuts down and both
    /// queues are empty (queued cells are always hashed).
    pub(crate) fn next_blocking(&self) -> Option<Queued> {
        let mut s = self.lock();
        loop {
            if let Some(q) = Self::pop(&mut s) {
                return Some(q);
            }
            if s.shutdown {
                return None;
            }
            s = self.work.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The next cell, if one is queued, without waiting.
    #[cfg(test)]
    pub(crate) fn try_next(&self) -> Option<Queued> {
        Self::pop(&mut self.lock())
    }

    fn pop(s: &mut State) -> Option<Queued> {
        let q = s.fills.pop_front().or_else(|| s.rechecks.pop_front())?;
        s.busy += 1;
        Some(q)
    }

    /// Hashes a cell taken by [`Self::next_blocking`], frees its slot, hands slots to waiters,
    /// and answers its caller.
    pub(crate) fn process(&self, queued: Queued) {
        let Queued { job, reply } = queued;
        let CellJob {
            expected,
            bytes,
            priority,
            ..
        } = job;
        let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
        // The thread's handle goes before the answer, so the caller's is the only one left.
        drop(bytes);
        let verdict = if digest.as_ref() == expected.as_slice() {
            Verdict::Match
        } else {
            Verdict::Mismatch
        };
        let wake = {
            let mut s = self.lock();
            s.busy -= 1;
            s.free_slot(priority);
            s.verified.bump(priority);
            if verdict == Verdict::Mismatch {
                s.mismatched.bump(priority);
            }
            s.grant(self.clock.now_ms())
        };
        for w in wake {
            w.wake();
        }
        // The caller may have stopped waiting; then nobody wants the verdict.
        let _ = reply.send(verdict);
    }

    /// Stops the threads once both queues are empty.
    pub(crate) fn shut_down(&self) {
        self.lock().shutdown = true;
        self.work.notify_all();
    }
}

/// Waits for a slot of `priority`; holds it once ready. Dropped while waiting, it leaves the
/// line, and a slot it had been handed goes to the next waiter.
struct Admit<'a> {
    core: &'a Core,
    priority: VerifyPriority,
    /// The waiter's place, once it has had to wait; `None` again once it holds its slot.
    ticket: Option<Ticket>,
}

impl Future for Admit<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut s = this.core.lock();
        let Some(ticket) = this.ticket else {
            if s.fits(this.priority) {
                s.take_slot(this.priority);
                return Poll::Ready(());
            }
            let ticket = (this.priority, s.next_ticket);
            s.next_ticket += 1;
            s.paused.bump(this.priority);
            let now = this.core.clock.now_ms();
            s.waiting.insert(ticket, (now, cx.waker().clone()));
            this.ticket = Some(ticket);
            return Poll::Pending;
        };
        if s.granted.remove(&ticket) {
            this.ticket = None;
            return Poll::Ready(());
        }
        // Not handed a slot yet, so still in line: keep its latest waker.
        s.waiting
            .entry(ticket)
            .and_modify(|(_, waker)| waker.clone_from(cx.waker()));
        Poll::Pending
    }
}

impl Drop for Admit<'_> {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let wake = {
            let mut s = self.core.lock();
            if !s.granted.remove(&ticket) {
                // Still in line and holding no slot: leaving frees nothing for anyone else.
                s.waiting.remove(&ticket);
                return;
            }
            s.free_slot(ticket.0);
            s.grant(self.core.clock.now_ms())
        };
        for w in wake {
            w.wake();
        }
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
