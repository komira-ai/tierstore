//! What the integration tests of this crate share: a mount provider served on a Unix socket of
//! the test, and the programs of the crate.

#![allow(
    dead_code,
    reason = "each test target uses the part of the support it needs"
)]

pub mod provider;

/// The `fake-s3` program of this crate.
pub const FAKE_S3: &str = env!("CARGO_BIN_EXE_fake-s3");

/// The driver of this crate.
pub const DRIVER: &str = env!("CARGO_BIN_EXE_tierstore-blackbox");

/// The scratch root of an integration test.
pub const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
