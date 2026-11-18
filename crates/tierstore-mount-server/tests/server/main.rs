//! The tests of the mount server, in one test binary: each module is one part of the protocol.
//! One binary, so that every function of the server is measured once, by all of them (AGENTS.md
//! T17: arms covered only by different test binaries count as missed).

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;
mod epoch;
mod fd_passing;
mod framing;
mod handshake;
mod lifeline;
mod requests;
