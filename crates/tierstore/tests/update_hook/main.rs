//! The real `tierstore update-hook` binary against stand-ins of what it acts on: a stub of the
//! instances' HTTP answers (stub.rs), fake `nft` and `systemctl` programs that keep their state in
//! files and log every call (fake/main.rs, the `tierstore-hook-fake` binary), and a scratch root
//! holding the declaration, the state directory and the host's files (rig.rs). The hook is run
//! as the updater runs it, one step per process, with `--root`, `--root-uid`, `--nft` and
//! `--systemctl` pointing at the scratch host.
//!
//! No test sleeps or asserts how long anything takes; the hook's own polls wait on its clock as
//! they do on a host. The tests that serve the public port (fixed at 9300 in the hook) take a lock
//! and run one at a time.

#![cfg(target_os = "linux")]

mod fakes;
mod flows;
mod killed;
mod metricz;
mod refused;
mod rig;
mod stub;
mod world;
