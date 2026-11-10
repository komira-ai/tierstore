//! `tierstore fs`: the virtual build file system, a subcommand of the `tierstore` binary
//! (`docs/design/virtual-build-fs.md`). One FUSE mount per node, `fs.mountpoint`, shows the trees
//! that clients attach by digest for their runs, lazily: a file's bytes are fetched from the
//! node's tierstore, over its fill socket, on the file's first open.
//!
//! - [`config`]: the `[fs]` table of the file.
//! - [`ino`], [`table`], [`tree`]: the inode numbers (invariant I1), the table of tree numbers in
//!   the runtime directory, and a flattened tree.
//! - [`attach`], [`control`]: the attachments and the control socket they are made on.
//! - [`fill_client`]: the client of the fill socket (fills, touches and pins).
//! - [`core`]: the operations of a filesystem on inode numbers, with no protocol in them.
//! - `start`: what a start does before a frontend serves (the runtime directory, the core, the
//!   attachments made again, the control socket), and the signals that end the process. It has no
//!   mount in it, so that the FUSE frontend and one without FUSE call it alike.
//! - `fuse` and `run` (Linux): the FUSE frontend and the process around it.
//! - [`nfs`] (macOS serves it; built and tested everywhere): the NFSv3 frontend, over loopback,
//!   and the mount around it.
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
mod backing;
mod config;
pub(in crate::server) mod control;
pub(in crate::server) mod core;
mod fill_client;
mod ino;
mod table;
mod tree;

#[cfg(target_os = "linux")]
mod fuse;
#[cfg(target_os = "linux")]
mod run;
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
#[cfg(not(target_os = "linux"))]
pub(super) use nfs::{NfsError, serve};
#[cfg(target_os = "linux")]
pub(super) use run::serve;
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
