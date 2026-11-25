//! The NFSv3 frontend of `tierstore fs` (macOS): the trees the core serves, read-only, over the NFS
//! and MOUNT protocols on a loopback socket, mounted at `fs.mountpoint` by `mount_nfs`
//! (`docs/design/virtual-build-fs.md` section 8; decision C9). FSKit may replace it later.
//!
//! - [`config`]: the `[fs.nfs]` table.
//! - `boot`: the epoch of the boot, which the table of tree numbers and the handles carry, so that
//!   a restart of the server keeps them and a reboot does not.
//! - `handle`: the file handle, the core's inode number and the epoch.
//! - `fs`: the procedures, on the core. The protocol under it is the `nfsserve` crate (our fork),
//!   which decodes the RPCs and calls one trait, `NFSFileSystem`.
//! - `fdcache`: the open cache files of the files read lately.
//! - `mount`: the mount and unmount commands, behind a runner a test replaces.
//! - `run`: the process around it.
//! - `interface`: the front as the `fs` interface of the one process, started and stopped by the
//!   start of a build node (`cache_cli.rs`), with a core that calls the node's cache.
//!
//! The frontend is built and tested on every target; a process serves it on macOS only (`tierstore
//! fs`, and the one process whose file has an `fs` interface), where Linux has FUSE (`run.rs` and
//! `interface.rs` of the parent).

mod boot;
pub(in crate::server) mod config;
mod error;
mod fdcache;
mod fs;
mod handle;
mod interface;
mod mount;
mod run;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod uid_tests;
#[cfg(test)]
mod wire_client;
#[cfg(test)]
mod wire_tests;

#[cfg(not(target_os = "linux"))]
pub(in crate::server) use error::NfsError;
#[cfg(any(target_os = "macos", test))]
pub(in crate::server) use interface::{NfsServed, NfsService};
#[cfg(not(target_os = "linux"))]
pub(in crate::server) use run::serve;
