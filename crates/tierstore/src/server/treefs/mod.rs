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
//! - `fuse` and `run` (Linux): the FUSE frontend and the process around it.
//!
//! This release runs the mode of a kernel without passthrough: cached opens, and every read the
//! page cache misses is answered by `pread` of the cache file.

mod attach;
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

pub(super) use config::{FsSettings, load};
#[cfg(test)]
pub(super) use config::{FsTable, from_text};
#[cfg(target_os = "linux")]
pub(super) use run::{StartError, serve};

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
