//! Helpers for the tests of the workspace's crates: scratch directories, loopback ports, child
//! processes that do not outlive the test, and forks run one at a time.
//!
//! The modules are `dir`, `forks`, `port` and `reaped`.

pub mod dir;
pub mod forks;
pub mod port;
pub mod reaped;
