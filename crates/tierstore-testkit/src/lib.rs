//! Helpers for the tests of `tierstore` and `objstore-s3`: scratch directories, loopback
//! ports and child processes that do not outlive the test.
//!
//! The modules are `dir`, `port` and `reaped`.

pub mod dir;
pub mod port;
pub mod reaped;
