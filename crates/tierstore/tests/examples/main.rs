//! The runner of `examples/` (crates/tierstore-examples) and its examples, run against this
//! crate's `tierstore` binary.
//!
//! - `build_cache`: the compiler-cache example as it is, and over a cache that keeps nothing.
//! - `runner`: the runner over small examples of the tests' own, whose client is the `tierstore`
//!   binary itself (`--version` answers 0, an unknown flag 2): every kind of check, a report that
//!   differs, an expected failure that passes, and each way a run ends with an error.
//! - `parts`: the spec, the placeholders, the metrics page and the report, as functions.
//! - `pages`: every example's page shows its files, and what the page check refuses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]
#![allow(
    clippy::disallowed_macros,
    reason = "an integration test says on standard error why it skipped (AGENTS.md T12)"
)]

mod build_cache;
mod pages;
mod parts;
mod runner;

use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

/// This crate's `tierstore` binary.
pub fn tierstore() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tierstore"))
}

/// The repository's `examples/`.
pub fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// A scratch directory of the test.
pub fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}
