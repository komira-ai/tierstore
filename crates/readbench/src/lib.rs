//! `readbench`: the farm validation of tierstore's read path (`docs/design/fetch-cache-serve.md`,
//! row T10b of section 18; the results are in `docs/measurements/2026-11-t10b.md`).
//!
//! A real job's tree, as the farm's object store holds it today (one JSON manifest per directory
//! under `cas/l/`, each file naming its bytes in a pack under `cas/b/`), is replayed through the
//! read path the daemon will host: its manifests are walked ([`manifest`]) and encoded as a
//! version 2 list, the list's L2 sidecar is made from the packs' cells ([`sidecar`]), and the
//! Prepare planner (`tierstore-plan`), the fetch engine (`tierstore-fetch`), the budget
//! (`tierstore-budget`), the hash pool (`tierstore-hashpool`) and the Linux file entry store
//! (`tierstore-entry`) fetch the tree into a cache directory ([`replay`]). The engine's parts go
//! straight to the volume server that holds each replica of a pack's chunk ([`upstream`], with the
//! map of [`swfs`]), so each drive of the store is a device of the per-drive budget. The same
//! tree is also fetched the way the shipped daemon fetches it (one ranged read per file, in 8 MiB
//! pieces), for its GET count. [`load`] runs many clients at once, [`sample`] samples the drives
//! and the nodes' metrics pages, and [`report`] makes the tables of the results document.
//!
//! Nothing here is part of tierstore: it is a measurement harness. It reads the machine's clock
//! in [`clock`] only; every other module is handed a [`clock::Clock`], so its tests move time by
//! hand and assert counts, never timing.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

pub mod cli;
pub mod clock;
pub mod error;
pub mod http;
pub mod load;
pub mod manifest;
pub mod record;
pub mod replay;
pub mod report;
pub mod sample;
pub mod sidecar;
pub mod swfs;
pub mod upstream;

pub use error::Error;
