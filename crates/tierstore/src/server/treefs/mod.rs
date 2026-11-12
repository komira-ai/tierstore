//! `tierstore fs`: the virtual build file system, a subcommand of the `tierstore` binary
//! (`docs/design/virtual-build-fs.md`). One FUSE mount per node, `fs.mountpoint`, shows the trees
//! that clients attach by digest for their runs, and any tree whose digest a lookup names
//! (`core/implicit.rs`, until it is idle for `fs.tree_idle_secs`), lazily: a file's bytes are
//! fetched from the node's tierstore, over its fill socket, on the file's first open.
//!
//! - [`config`]: the `[fs]` table of the file.
//! - [`ino`], [`table`], [`tree`]: the inode numbers (invariant I1), the table of tree numbers in
//!   the runtime directory, and a flattened tree.
//! - [`attach`], [`control`]: the attachments (of runs, and of lookups) and the control socket
//!   the attachments of runs are made on.
//! - [`fill_client`]: the client of the fill socket (fills, touches and pins).
//! - [`core`]: the operations of a filesystem on inode numbers, with no protocol in them.
//! - `start`: what a start does before a frontend serves (the runtime directory, the core, the
//!   attachments made again, the control socket), and the signals that end the process. It has no
//!   mount in it, so that the FUSE frontend and one without FUSE call it alike.
//! - `fuse` and `run` (Linux): the FUSE frontend and the process around it.
//! - [`nfs`] (macOS serves it; built and tested everywhere): the NFSv3 frontend, over loopback,
//!   and the mount around it.
//! - `launch`, `serving`, `sd`, `init_record`, `handle`, `recover` and `handover` (Linux): crash
//!   recovery and the hand-over of the mount. The mount's `/dev/fuse` descriptor is kept by the
//!   service manager (`sd`), so the mount outlives the process; a process that takes the
//!   connection over serves it from the record of the kernel's handshake (`init_record`) without a
//!   handshake (`serving`); the file handles of the process before are told by their numbers
//!   (`handle`); a kernel that cannot send the unanswered requests again is waited on for a bounded
//!   time (`recover`); and a running instance hands the connection to a new one with no moment in
//!   which no one holds it (`handover`). `launch` decides at a start which of these applies.
//!
//! The core serves a frontend in either of two ways: with opens and handles, and a count of the
//! lookups the kernel holds until it forgets them (FUSE); or by inode alone, with the reads of
//! [`core::Core::blob_file`] and no count, so that a tree leaves memory when it is detached (a
//! protocol with no open and no forget, as NFS).
//!
//! Two modes, chosen at start from what the kernel's `INIT` offers (`fuse`): with passthrough
//! (kernel 6.9 and later, `CAP_SYS_ADMIN`, `fs.passthrough` not false) every non-empty file is
//! opened by the kernel from the cache file, under one backing id per blob (`backing`); without,
//! the mode of kernel 6.8: cached opens, and every read the page cache misses is answered by
//! `pread` of the cache file.

mod attach;
#[cfg(any(target_os = "linux", test))]
mod backing;
mod config;
pub(in crate::server) mod control;
pub(in crate::server) mod core;
mod fill_client;
#[cfg(any(target_os = "linux", test))]
mod handle;
mod ino;
// The put directory: served by the FUSE frontend on Linux; its NFS frontend is a later change.
#[cfg_attr(
    all(not(test), not(target_os = "linux")),
    expect(
        dead_code,
        reason = "the NFS frontend of the put directory, which calls the store, is a later change"
    )
)]
mod put;
mod table;
mod tree;

#[cfg(target_os = "linux")]
mod fuse;
#[cfg(target_os = "linux")]
mod fuse_put;
#[cfg(target_os = "linux")]
mod handover;
#[cfg(target_os = "linux")]
mod init_record;
#[cfg(target_os = "linux")]
mod launch;
#[cfg(target_os = "linux")]
mod recover;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod sd;
#[cfg(target_os = "linux")]
mod serving;
// The start is the FUSE frontend's on Linux and the NFS frontend's on macOS; on a target that serves
// neither it is built for the tests.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod start;

// Built on every target, so that it is tested, and measured, where the FUSE frontend is the one
// that serves; macOS serves it.
#[cfg_attr(
    all(target_os = "linux", not(test)),
    expect(
        dead_code,
        reason = "the NFS frontend serves on macOS; Linux serves FUSE"
    )
)]
mod nfs;

pub(super) use config::{FsSettings, load};
#[cfg(test)]
pub(super) use config::{FsTable, from_text};
#[cfg(target_os = "linux")]
pub(super) use launch::serve;
#[cfg(not(target_os = "linux"))]
pub(super) use nfs::{NfsError, serve};
#[cfg(target_os = "linux")]
pub(super) use start::StartError;

use std::sync::{Mutex, MutexGuard};

/// The data of `mutex`, whether or not a thread panicked holding it: the state it guards is
/// whole at every point a panic can come from.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod fakedev;
#[cfg(test)]
#[cfg(target_os = "linux")]
mod recovery_tests;
#[cfg(test)]
pub(in crate::server) mod testing;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::locked;

    /// Asserts that the data of a mutex is had after a thread panicked holding it.
    #[test]
    fn the_data_of_a_mutex_a_thread_panicked_holding_is_still_had() {
        let mutex = Arc::new(Mutex::new(5));
        let held = Arc::clone(&mutex);
        let thread = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a thread dies holding the lock");
        });
        assert!(thread.join().is_err());
        assert_eq!(*locked(&mutex), 5);
    }
}
