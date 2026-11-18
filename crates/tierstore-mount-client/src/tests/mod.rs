//! The tests of the mount client, in the crate's unit-test binary with the modules' own tests:
//! each module is one part of what the client does. One binary, so that every function of the
//! client is measured once, by all of them (AGENTS.md T17: arms covered only by different test
//! binaries count as missed).
//!
//! Every test runs the client on its own runtime and plays the readers from plain threads, as
//! `tsfuse`'s reader threads would. The server is either the real one of T5 over the real entry
//! store of T1 (its sealed state memfds and data files, its mapping of the mount's counter memfd),
//! driven by the fake engine, or a peer the test plays frame by frame. No test reads a clock or
//! sleeps: time is a manual clock the test moves, and every wait is for a reply, a ticket or a
//! count of the client or the server.

mod coalescing;
mod common;
mod lifecycle;
mod protocol;
mod reads;
mod reconnect;
mod runtime;
