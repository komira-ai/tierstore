//! Helpers for the tests of the workspace's crates: scratch directories, loopback ports, child
//! processes that do not outlive the test, forks run one at a time, and an HTTP client.
//!
//! The modules are `dir`, `forks`, `http`, `port` and `reaped`.

pub mod dir;
pub mod forks;
pub mod http;
pub mod port;
pub mod reaped;
