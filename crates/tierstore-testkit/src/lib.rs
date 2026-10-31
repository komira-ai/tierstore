//! Helpers for the tests of `tierstore` and `objstore-s3`: scratch directories, loopback
//! ports and child processes that do not outlive the test.
//!
//! The modules are the ones of the `js-testkit` crate of the `komira-ai/jobscheduler`
//! repository (`dir`, `port`, `reaped`) that these tests use, copied as they are.

pub mod dir;
pub mod port;
pub mod reaped;
