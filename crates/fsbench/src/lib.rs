//! `fsbench`: the benchmark of the FUSE layer of `tierstore fs`, and a null file system to hold it
//! against.
//!
//! A run is `readers` processes that each open, read and close the files of a fixed list over and
//! over (one *cycle*: `stat`, `open`, a read of the first bytes, `close`), beside `burners`
//! processes that spin on every CPU, for a fixed time; then a cold and a warm whole-file read of
//! the largest files of the list. While it runs, the harness samples once a second the
//! processes in D state and the requests the kernel holds for the mount
//! (`/sys/fs/fuse/connections/<n>/waiting`), and it measures the CPU time the server process,
//! its control group and the whole node spent. One JSON object per run goes to the results file
//! ([`record`]); `fsbench report` turns a results file into the tables of a measurement
//! document ([`report`]). `fsbench null-fs` ([`nullfs`]) serves the same list with no storage
//! behind it, so the transport is measured apart from what `tierstore fs` does per request;
//! `fsbench tsfuse-null-fs` ([`tsnull`]) does the same on `tsfuse`, the FUSE library of the
//! redesign, with its cloned readers and its READ reply path, on `/dev/fuse` or over io_uring.
//! Both serve the tree of [`tree`]. `fsbench storm` ([`storm`]) is the storm test of the
//! io_uring transport: parked reads from one CPU and from every CPU, and the latency of metadata
//! during them, on a file system of its own ([`stormfs`]); `fsbench storm-report` makes its
//! tables ([`stormrec`]). `fsbench summary` ([`summary`]) puts the series of several results files
//! in one table. `fsbench dstate-report` reads what a sampler of read-only tools printed on the
//! build nodes during a real build ([`dstate`]) and makes its tables ([`dstaterep`]): D state and
//! FUSE waiting under the farm's own jobs.
//!
//! The processes speak one line of JSON at a time over their standard streams: a reader says it
//! is ready, is told to go, and answers with its counts and latency histograms ([`reader`]); a
//! burner spins until its standard input closes ([`burn`]); the null file system says it is
//! mounted and unmounts when its standard input closes. Every process thus ends by itself, and
//! nothing needs a signal.
//!
//! The crate is Linux only (it reads /proc and /sys and mounts FUSE).

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

pub mod burn;
pub mod cli;
mod clock;
pub mod dstate;
pub mod dstaterep;
pub mod error;
pub mod hist;
pub mod host;
pub mod list;
pub mod nullfs;
pub mod reader;
pub mod record;
pub mod report;
pub mod run;
pub mod storm;
pub mod stormfs;
pub mod stormrec;
pub mod summary;
pub mod tree;
pub mod tsnull;

pub use error::Error;
