//! The farm measurements that set the constants of tierstore's fetch, cache and cold-tier
//! design (`docs/measurements/2026-11-t0.md` holds the results and the constants they decide).
//!
//! One binary, `ts-measure`, runs every arm on a build node drained of jobs. Each arm writes one
//! JSON line per cell and a Markdown table to its `--out` directory; the results document is
//! assembled from those tables. Nothing here is part of tierstore itself.
//!
//! The arms (the letters are those of the design's measurement plan):
//! - A, raw HDD reads ([`hdd`]): request size x streams x readahead or `O_DIRECT` x adjacent or
//!   scattered, on one drive and on both.
//! - B, the cache device ([`cache`]): writes, `fdatasync` latency, a mixed load, hole punching
//!   and `POSIX_FADV_DONTNEED` policies, on the loop device, the root file system and a loop with
//!   direct I/O.
//! - C-1, silo ranged GETs ([`silo`]); C-2 to C-4, the cold-tier prototype ([`cold`]): a
//!   per-drive scheduler serving packs over HTTP, with a load generator for one drive, two copies,
//!   peers, demand reads, a mixed read and write load and a slow consumer; C-5, pack writes and
//!   steady-state churn with eviction ([`write`], [`churn`]); C-6, drive facts and the object-size
//!   distribution ([`facts`], [`sizes`]); C-7, placement ([`placement`]).
//! - D, CPU rates of the hashes ([`cpu`]); E, node facts ([`facts`]).
//!
//! The arms are Linux only (they read block devices, /proc and /sys, and use `O_DIRECT`,
//! `posix_fadvise`, `fdatasync`, hole punching and `sendfile`). On another system the crate has
//! only [`cli::main`], which says so and exits 2 (`src/unsupported.rs`).

#[cfg(target_os = "linux")]
pub mod blockdev;
#[cfg(target_os = "linux")]
pub mod cache;
#[cfg(target_os = "linux")]
pub mod churn;
#[cfg(target_os = "linux")]
pub mod cli;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
pub mod cli;
#[cfg(target_os = "linux")]
pub mod clock;
#[cfg(target_os = "linux")]
pub mod cold;
#[cfg(target_os = "linux")]
pub mod cpu;
#[cfg(target_os = "linux")]
pub mod diskstats;
#[cfg(target_os = "linux")]
pub mod error;
#[cfg(target_os = "linux")]
pub mod facts;
#[cfg(target_os = "linux")]
pub mod guard;
#[cfg(target_os = "linux")]
pub mod hdd;
#[cfg(target_os = "linux")]
pub mod io;
#[cfg(target_os = "linux")]
pub mod loopfs;
#[cfg(target_os = "linux")]
pub mod placement;
#[cfg(target_os = "linux")]
pub mod report;
#[cfg(target_os = "linux")]
pub mod run;
#[cfg(target_os = "linux")]
pub mod silo;
#[cfg(target_os = "linux")]
pub mod sizes;
#[cfg(target_os = "linux")]
pub mod stats;
#[cfg(target_os = "linux")]
#[cfg(test)]
mod testutil;
#[cfg(target_os = "linux")]
pub mod workload;
#[cfg(target_os = "linux")]
pub mod write;

#[cfg(target_os = "linux")]
pub use error::Error;
