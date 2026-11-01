//! The real `tierstore update-hook` binary against stand-ins of what it acts on: a stub of the
//! instances' HTTP answers (stub.rs), fake `nft` and `systemctl` programs that keep their state in
//! files and log every call (the `tierstore-hook-fake` binary, whose code is
//! `tierstore::server::update_hook::fake`), and a scratch root holding the declaration, the state
//! directory and the host's files (rig.rs). The hook is run as the updater runs it, one step per
//! process, with the flags of the conformance build pointing it at the scratch host, its own
//! public port and a poll of a millisecond.
//!
//! No test sleeps or asserts how long anything takes; each host has ports of its own, so the
//! tests run side by side.

#![cfg(target_os = "linux")]

mod failures;
mod flows;
mod killed;
mod metricz;
mod refused;
mod rig;
mod stub;
