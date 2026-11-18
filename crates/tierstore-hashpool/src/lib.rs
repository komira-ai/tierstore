//! The hash pool of the read path on a build node (`docs/design/fetch-cache-serve.md`, 3.3 and
//! 4.7; task T2 of section 18): the implementation of
//! [`Verifier`](tierstore_fetch_api::verify::Verifier) that every fill and every `recheck` cell
//! is verified by.
//!
//! A cell's bytes arrive in a pooled 1 MiB buffer; [`HashPool`] hashes them with `ring`'s sha256
//! on dedicated threads and compares the digest with the cell's entry in the list's cell table, so
//! each cell is judged alone, in any order, before it touches the cache file (3.3). The pool holds
//! at most [`QUEUE_CELLS`] cells (32 MiB of buffers) at once; a `verify` that finds it full waits,
//! and that wait is the receive pause of 4.7, which the serving drive's read-ahead buffer and slot
//! release absorb (10.7). Fills are hashed and handed freed slots before `recheck` cells,
//! which hold at most [`RECHECK_CELLS`] of the slots, so fills always keep at least the other
//! half.
//!
//! How many threads: [`hash_threads`] of the node's cache write ceiling and of the core's sha256
//! rate, which [`measure_core_sha_rate`] measures at startup ([`HashPool::start_measured`] does
//! both). The same rule serves both deployment profiles of section 21: the combined nodes of
//! today (no SHA-NI, about 0.29 GB/s per core: 3 threads at 500 MB/s) and build nodes of a later
//! CPU (with SHA-NI, about 1.5 GB/s: 1 thread). Only the target rate is configuration.

#![forbid(unsafe_code)]

mod plan;
mod pool;
mod shared;

pub use crate::plan::{
    BytesPerSec, DEFAULT_TARGET_RATE, QUEUE_CELLS, RECHECK_CELLS, hash_threads,
    measure_core_sha_rate,
};
pub use crate::pool::{HashPool, HashPoolError};
pub use crate::shared::{ByPriority, PoolStats};
