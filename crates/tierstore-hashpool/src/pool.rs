//! `HashPool`: the threads, and the `Verifier` they serve.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread::JoinHandle;

use bytes::Bytes;
use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::verify::{CellJob, Verdict, Verifier};

use crate::plan::{BytesPerSec, hash_threads, measure_core_sha_rate};
use crate::shared::{Core, PoolStats};

/// Why a pool did not start.
#[derive(Debug, thiserror::Error)]
pub enum HashPoolError {
    /// The operating system refused a hash thread. The threads started before it are stopped.
    #[error("start hash thread {index} of {threads}: {source}")]
    Spawn {
        /// The thread's index, from 0.
        index: usize,
        /// The threads the pool was to run.
        threads: usize,
        /// The refusal.
        source: std::io::Error,
    },
}

/// The bounded hash pool of 4.7: dedicated threads (not the runtime's blocking pool) hashing cells
/// with `ring`'s sha256, at most [`QUEUE_CELLS`](crate::QUEUE_CELLS) cells held at once. Dropping
/// the pool hashes the cells already queued and then joins its threads.
pub struct HashPool {
    core: Arc<Core>,
    threads: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for HashPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashPool")
            .field("threads", &self.threads.len())
            .field("stats", &self.stats())
            .finish()
    }
}

/// The error of a thread the operating system refused.
fn refused(index: usize, threads: NonZeroUsize, source: std::io::Error) -> HashPoolError {
    HashPoolError::Spawn {
        index,
        threads: threads.get(),
        source,
    }
}

impl HashPool {
    /// A pool of `threads` hash threads. `clock` times the waits of [`PoolStats::pause_wait_ms`].
    ///
    /// # Errors
    /// [`HashPoolError::Spawn`] when the operating system refuses a thread.
    pub fn start(threads: NonZeroUsize, clock: Arc<dyn Clock>) -> Result<Self, HashPoolError> {
        let mut pool = Self {
            core: Arc::new(Core::new(clock)),
            threads: Vec::with_capacity(threads.get()),
        };
        for index in 0..threads.get() {
            let core = Arc::clone(&pool.core);
            let spawned = std::thread::Builder::new()
                .name(format!("ts-hash-{index}"))
                .spawn(move || {
                    while let Some(queued) = core.next_blocking() {
                        core.process(queued);
                    }
                });
            // On a refusal `pool` is dropped here, which stops the threads already started.
            let handle = spawned.map_err(|e| refused(index, threads, e))?;
            pool.threads.push(handle);
        }
        Ok(pool)
    }

    /// The pool a node runs (4.7): measures this core's sha256 rate on `clock`, and starts
    /// [`hash_threads`] threads for the cache write ceiling `target`, at most one per CPU.
    ///
    /// # Errors
    /// As [`HashPool::start`].
    pub fn start_measured(
        target: BytesPerSec,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, HashPoolError> {
        let core = measure_core_sha_rate(clock.as_ref());
        let cpus = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        Self::start(hash_threads(target, core, cpus), clock)
    }

    /// The number of hash threads.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads.len()
    }

    /// What the pool is doing and has done.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        self.core.stats()
    }
}

impl Drop for HashPool {
    fn drop(&mut self) {
        self.core.shut_down();
        for handle in self.threads.drain(..) {
            // A thread that panicked has nothing left to stop.
            let _ = handle.join();
        }
    }
}

#[async_trait::async_trait]
impl Verifier for HashPool {
    /// Waits for a slot (the receive pause of 4.7), then for a thread's verdict on the cell. The
    /// bytes come back as the only handle to them, so the buffer can return to its pool.
    async fn verify(&self, job: CellJob) -> (Verdict, Bytes) {
        self.core.verify(job).await
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
