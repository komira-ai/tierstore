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
//!
//! The frontend is built and tested on every target; the process serves it on macOS only, where
//! Linux has FUSE (`run.rs` of the parent).

mod boot;
pub(in crate::server) mod config;
mod error;
mod fdcache;
mod fs;
mod handle;
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
#[cfg(not(target_os = "linux"))]
pub(in crate::server) use run::serve;
