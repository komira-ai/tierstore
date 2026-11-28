//! The tests of the hot tier (section 14 of `docs/design/fetch-cache-serve.md`, the rows of task
//! T-S4), as one test binary over the deterministic simulation of [`support`]: one binary, so
//! every function of the crate is measured as one instance.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod device;
mod group_commit;
mod io_edges;
mod large;
mod large_edges;
mod node;
mod partition;
mod replication;
mod seal_edges;
mod seal_triggers;
mod startup_tick;
mod support;
mod writer_edges;
