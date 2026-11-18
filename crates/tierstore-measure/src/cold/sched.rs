//! The per-drive scheduler of the prototype, as the design describes it: at most `streams`
//! streams admitted at once (at most `write_streams` of them writes), waiting requests ordered
//! by class (`demand` > `prepare` > `background`; writes rank with `prepare`) and then by arrival;
//! an admitted stream keeps its slot for `slice_bytes` and yields at a chunk boundary only when a
//! request of its class or higher waits; a `demand` that finds only lower classes admitted takes
//! the slot of the lowest-class stream at that stream's next chunk boundary, at most
//! `preempt_max_per_s` times a second; a class queue longer than `queue_max` refuses (a 503).
//! A stream may also give its slot back while its consumer does not read (`park`) and wait for it
//! again before its next chunk.
//!
//! [`State`] is the pure state machine (its tests drive it step by step); [`Sched`] wraps it with a
//! lock and a condition variable for the server's threads.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;

use crate::clock::Clock;

/// A request's class, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Background reads.
    Background,
    /// Prepare reads, and writes.
    Prepare,
    /// A reader is waiting.
    Demand,
}

impl Class {
    /// Parses `demand`, `prepare`, `background`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "demand" => Some(Self::Demand),
            "prepare" => Some(Self::Prepare),
            "background" => Some(Self::Background),
            _ => None,
        }
    }

    /// The name of [`Class::parse`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Demand => "demand",
            Self::Prepare => "prepare",
            Self::Background => "background",
        }
    }
}

/// The scheduler's settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Params {
    /// Streams admitted at once.
    pub streams: usize,
    /// Of which writes.
    pub write_streams: usize,
    /// Bytes a stream holds its slot for before it yields to an equal or higher class.
    pub slice_bytes: u64,
    /// Whether a demand preempts a lower class at a chunk boundary.
    pub preempt: bool,
    /// Preemptions allowed per second.
    pub preempt_max_per_s: usize,
    /// Requests a class queue holds before it refuses.
    pub queue_max: usize,
}

#[derive(Debug, Clone)]
struct Slot {
    id: u64,
    class: Class,
    write: bool,
    used: u64,
    pause: bool,
}

#[derive(Debug, Clone)]
struct Waiter {
    id: u64,
    class: Class,
    write: bool,
    since: Duration,
    parked: bool,
}

/// What a stream does at a chunk boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// Keeps its slot.
    Continue,
    /// Gave its slot up and waits to be admitted again.
    Yield,
}

/// Counters of what the scheduler did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Streams admitted (re-admissions included).
    pub admitted: u64,
    /// Slice-end yields.
    pub yields: u64,
    /// Demand preemptions.
    pub preemptions: u64,
    /// Requests refused for a full queue.
    pub refused: u64,
    /// Slots given back by a stream whose consumer stalled.
    pub parks: u64,
}

/// The state of one drive's scheduler.
#[derive(Debug)]
pub struct State {
    p: Params,
    active: Vec<Slot>,
    waiting: Vec<Waiter>,
    next: u64,
    preempts: VecDeque<Duration>,
    /// What it did.
    pub counts: Counts,
}

impl State {
    /// An idle scheduler.
    #[must_use]
    pub fn new(p: Params) -> Self {
        Self {
            p,
            active: Vec::new(),
            waiting: Vec::new(),
            next: 1,
            preempts: VecDeque::new(),
            counts: Counts::default(),
        }
    }

    /// Queues a request; `None` when its class queue is full.
    pub fn enqueue(&mut self, class: Class, write: bool, now: Duration) -> Option<u64> {
        let queued = self.waiting.iter().filter(|w| w.class == class).count();
        if queued >= self.p.queue_max {
            self.counts.refused += 1;
            return None;
        }
        let id = self.next;
        self.next += 1;
        self.waiting.push(Waiter {
            id,
            class,
            write,
            since: now,
            parked: false,
        });
        Some(id)
    }

    fn best(&self) -> Option<usize> {
        let writes = self.active.iter().filter(|s| s.write).count();
        self.waiting
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.parked && (!w.write || writes < self.p.write_streams))
            .max_by(|(_, a), (_, b)| {
                a.class
                    .cmp(&b.class)
                    .then(b.since.cmp(&a.since))
                    .then(b.id.cmp(&a.id))
            })
            .map(|(i, _)| i)
    }

    /// Admits waiting requests into free slots (best class first, then oldest), and marks a
    /// lower-class stream to pause when a demand waits and no slot is free.
    pub fn admit_ready(&mut self, now: Duration) {
        while self.active.len() < self.p.streams {
            let Some(i) = self.best() else { break };
            let w = self.waiting.remove(i);
            self.active.push(Slot {
                id: w.id,
                class: w.class,
                write: w.write,
                used: 0,
                pause: false,
            });
            self.counts.admitted += 1;
        }
        let demand_waits = self
            .best()
            .is_some_and(|i| self.waiting[i].class == Class::Demand);
        if !self.p.preempt || !demand_waits || self.active.iter().any(|s| s.pause) {
            return;
        }
        while self
            .preempts
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= Duration::from_secs(1))
        {
            self.preempts.pop_front();
        }
        if self.preempts.len() >= self.p.preempt_max_per_s {
            return;
        }
        let victim = self
            .active
            .iter_mut()
            .filter(|s| s.class < Class::Demand)
            .min_by_key(|s| s.class);
        if let Some(v) = victim {
            v.pause = true;
            self.preempts.push_back(now);
            self.counts.preemptions += 1;
        }
    }

    /// Whether `id` holds a slot.
    #[must_use]
    pub fn is_active(&self, id: u64) -> bool {
        self.active.iter().any(|s| s.id == id)
    }

    /// A stream read `bytes` more; decides whether it yields before its next chunk.
    pub fn boundary(&mut self, id: u64, bytes: u64, now: Duration) -> Boundary {
        let Some(i) = self.active.iter().position(|s| s.id == id) else {
            return Boundary::Continue;
        };
        let s = &mut self.active[i];
        s.used += bytes;
        let (class, used, pause) = (s.class, s.used, s.pause);
        let rival = self.waiting.iter().any(|w| !w.parked && w.class >= class);
        if pause || (used >= self.p.slice_bytes && rival) {
            let s = self.active.remove(i);
            if !pause {
                self.counts.yields += 1;
            }
            // A preempted stream goes back first in its class (its arrival time is kept small);
            // a slice-end yield goes behind the others of its class.
            let since = if pause { Duration::ZERO } else { now };
            self.waiting.push(Waiter {
                id,
                class: s.class,
                write: s.write,
                since,
                parked: false,
            });
            self.admit_ready(now);
            return Boundary::Yield;
        }
        if used >= self.p.slice_bytes {
            self.active[i].used = 0;
        }
        Boundary::Continue
    }

    /// Gives `id`'s slot back while its consumer stalls; it waits again with [`State::unpark`].
    pub fn park(&mut self, id: u64, now: Duration) {
        if let Some(i) = self.active.iter().position(|s| s.id == id) {
            let s = self.active.remove(i);
            self.counts.parks += 1;
            self.waiting.push(Waiter {
                id,
                class: s.class,
                write: s.write,
                since: now,
                parked: true,
            });
            self.admit_ready(now);
        }
    }

    /// Makes a parked request eligible for a slot again.
    pub fn unpark(&mut self, id: u64, now: Duration) {
        for w in &mut self.waiting {
            if w.id == id {
                w.parked = false;
            }
        }
        self.admit_ready(now);
    }

    /// Ends `id`, admitted or waiting.
    pub fn release(&mut self, id: u64, now: Duration) {
        self.active.retain(|s| s.id != id);
        self.waiting.retain(|w| w.id != id);
        self.admit_ready(now);
    }

    /// Streams admitted now, and requests waiting.
    #[must_use]
    pub fn load(&self) -> (usize, usize) {
        (self.active.len(), self.waiting.len())
    }
}

/// A shared scheduler for the server's threads.
pub struct Sched<'c> {
    state: Mutex<State>,
    cv: Condvar,
    clock: &'c dyn Clock,
}

/// An admitted (or waiting) request.
#[derive(Debug)]
pub struct Ticket {
    id: u64,
}

impl<'c> Sched<'c> {
    /// A scheduler with `p`.
    #[must_use]
    pub fn new(p: Params, clock: &'c dyn Clock) -> Self {
        Self {
            state: Mutex::new(State::new(p)),
            cv: Condvar::new(),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_active(&self, id: u64) -> Duration {
        let t0 = self.clock.now();
        let mut g = self.lock();
        loop {
            g.admit_ready(self.clock.now());
            if g.is_active(id) {
                break;
            }
            g = self
                .cv
                .wait_timeout(g, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(g);
        self.cv.notify_all();
        self.clock.now().saturating_sub(t0)
    }

    /// Waits for a slot; `None` when the class queue is full. Returns the ticket and the wait.
    pub fn admit(&self, class: Class, write: bool) -> Option<(Ticket, Duration)> {
        let id = self.lock().enqueue(class, write, self.clock.now())?;
        let waited = self.wait_active(id);
        Some((Ticket { id }, waited))
    }

    /// The stream read `bytes`; returns how long it then waited (0 if it kept its slot).
    pub fn boundary(&self, t: &Ticket, bytes: u64) -> Duration {
        let b = self.lock().boundary(t.id, bytes, self.clock.now());
        self.cv.notify_all();
        match b {
            Boundary::Continue => Duration::ZERO,
            Boundary::Yield => self.wait_active(t.id),
        }
    }

    /// Gives the slot back while the consumer stalls.
    pub fn park(&self, t: &Ticket) {
        self.lock().park(t.id, self.clock.now());
        self.cv.notify_all();
    }

    /// Waits for a slot again after [`Sched::park`].
    pub fn unpark(&self, t: &Ticket) -> Duration {
        self.lock().unpark(t.id, self.clock.now());
        self.wait_active(t.id)
    }

    /// Ends the request.
    pub fn release(&self, t: Ticket) {
        self.lock().release(t.id, self.clock.now());
        self.cv.notify_all();
    }

    /// What it did so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.lock().counts
    }
}

#[cfg(test)]
mod tests;
