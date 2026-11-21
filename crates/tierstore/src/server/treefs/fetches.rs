//! The threads that answer the requests of the mount that may wait on the fill socket, so that no
//! reader of the connection waits on it (`node_fs.rs`).
//!
//! A reader of the connection that meets an open whose bytes the fill socket must be asked for (a
//! blob the cache does not hold, a span of a pack the tree has not been given yet, a pin not asked
//! yet), a read of a handle an earlier process gave out, or a lookup in the namespace, does not
//! make it: it parks the request here and reads the next one. A node's tierstore that takes fills
//! and never answers then holds these threads, never the readers, and the mount still serves the
//! reads of open files, the opens of files the cache holds and every request of a tree in memory.
//!
//! There are as many of these threads as readers (`fs.threads`), so as many fills are asked at
//! once as before. Requests that find every thread busy wait in a queue, in no thread; each open
//! is answered by `fs.fetch_timeout` after it was parked (`core/blob.rs`), so one that waited out
//! its time in the queue fails at once, as `EIO`, without asking the fill socket. The stop of the
//! readers (a hand-over, a restart) waits for every request parked to be answered, so none that
//! this process read is left unanswered when it ends.

use std::io;
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use super::locked;

/// A request parked: it answers itself.
pub(super) type Job = Box<dyn FnOnce() + Send>;

/// The queue of the parked requests and the threads that answer them.
pub(super) struct Fetches {
    queue: Sender<Job>,
    /// The requests parked and not answered yet.
    busy: Arc<Busy>,
}

/// The count of the requests parked and not answered, and what wakes a wait for none.
#[derive(Default)]
pub(super) struct Busy {
    count: Mutex<usize>,
    idle: Condvar,
}

impl Fetches {
    /// `threads` threads, each named `fs-fetch-<n>`, waiting for parked requests; they end when
    /// this is dropped and the queue is empty.
    ///
    /// # Errors
    /// What the system said when a thread cannot be started.
    pub(super) fn start(threads: NonZeroUsize) -> io::Result<Self> {
        let (queue, jobs) = mpsc::channel::<Job>();
        let jobs = Arc::new(Mutex::new(jobs));
        for n in 0..threads.get() {
            let jobs = Arc::clone(&jobs);
            let _ = std::thread::Builder::new()
                .name(format!("fs-fetch-{n}"))
                .spawn(move || answer(&jobs))?;
        }
        Ok(Self {
            queue,
            busy: Arc::default(),
        })
    }

    /// Parks `job` for a thread of the pool; runs it on the caller's thread when no thread of the
    /// pool is left to take it.
    pub(super) fn park(&self, job: Job) {
        *locked(&self.busy.count) += 1;
        let done = Done(Arc::clone(&self.busy));
        let counted: Job = Box::new(move || {
            let _done = done;
            job();
        });
        if let Err(mpsc::SendError(job)) = self.queue.send(counted) {
            job();
        }
    }

    /// Waits until every request parked is answered: for the stop of the readers, so that no
    /// request this process read is left unanswered when it ends (`serving.rs`). Each is
    /// answered by its time (module docs).
    pub(super) fn drain(&self) {
        let _idle = self
            .busy
            .idle
            .wait_while(locked(&self.busy.count), |count| *count > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The requests parked and not answered yet.
    #[cfg(test)]
    pub(super) fn busy(&self) -> usize {
        *locked(&self.busy.count)
    }
}

/// A parked request's end: its count is taken away when it is answered, or when it panics.
struct Done(Arc<Busy>);

impl Drop for Done {
    fn drop(&mut self) {
        let mut count = locked(&self.0.count);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.0.idle.notify_all();
        }
    }
}

/// Answers the parked requests of `jobs`, one at a time, until the queue is closed.
fn answer(jobs: &Mutex<Receiver<Job>>) {
    loop {
        // The lock is held only while a request is taken, not while it is answered.
        let taken = locked(jobs).recv();
        let Ok(job) = taken else {
            return;
        };
        job();
    }
}

#[cfg(test)]
#[path = "fetches_tests.rs"]
mod tests;
