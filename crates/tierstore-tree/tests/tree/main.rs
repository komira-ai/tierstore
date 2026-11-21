//! The reads, waits and opens of a `Tree` against the real mount server over a real entry store.
//!
//! One test binary on purpose: `Tree::read` is generic over the caller's stage, and the arms of a
//! generic function covered by different test binaries count as missed (the coverage gate counts
//! each instantiation).

// Linux only, as the crate is (its library and its dependencies are `cfg(target_os = "linux")`).
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[path = "../support/mod.rs"]
mod support;

mod changed;
mod open;
mod read;
mod rig;
mod wait;
