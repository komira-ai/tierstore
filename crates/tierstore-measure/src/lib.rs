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
//! The crate is Linux only (it reads block devices, /proc and /sys, and uses `O_DIRECT`,
//! `posix_fadvise`, `fdatasync`, hole punching and `sendfile`); on another system it is empty.

#![cfg(target_os = "linux")]

pub mod blockdev;
pub mod cache;
pub mod churn;
pub mod cli;
pub mod clock;
pub mod cold;
pub mod cpu;
pub mod diskstats;
pub mod error;
pub mod facts;
pub mod guard;
pub mod hdd;
pub mod io;
pub mod loopfs;
pub mod placement;
pub mod report;
pub mod run;
pub mod silo;
pub mod sizes;
pub mod stats;
#[cfg(test)]
mod testutil;
pub mod workload;
pub mod write;

pub use error::Error;
