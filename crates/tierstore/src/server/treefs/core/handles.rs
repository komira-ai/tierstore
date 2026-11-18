//! The open files of the core: the handles that [`Core::open`] gives out, split in 64 maps by the
//! low bits of the handle's number, so that the opens, reads and releases of different files do
//! not wait for each other. A file is closed after the lock of its map is released.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::super::locked;
use super::Handle;

/// How many maps the handles are split in.
const SHARDS: usize = 64;

/// The open files, by handle.
#[derive(Debug)]
pub(super) struct Handles([Mutex<HashMap<u64, Arc<Handle>>>; SHARDS]);

impl Handles {
    /// No open file.
    pub(super) fn new() -> Self {
        Self(std::array::from_fn(|_| Mutex::new(HashMap::new())))
    }

    /// The map that holds `fh`.
    fn shard(&self, fh: u64) -> &Mutex<HashMap<u64, Arc<Handle>>> {
        &self.0[(fh % SHARDS as u64) as usize]
    }

    /// Records the open file `handle` as `fh`; a handle of the same number it replaces is closed
    /// after the lock is released.
    pub(super) fn insert(&self, fh: u64, handle: Arc<Handle>) {
        let replaced = locked(self.shard(fh)).insert(fh, handle);
        drop(replaced);
    }

    /// Records `handle` as `fh` unless a file is open as `fh` already.
    pub(super) fn insert_if_absent(&self, fh: u64, handle: Handle) {
        let mut shard = locked(self.shard(fh));
        let _ = shard.entry(fh).or_insert_with(|| Arc::new(handle));
    }

    /// The open file `fh`.
    pub(super) fn get(&self, fh: u64) -> Option<Arc<Handle>> {
        locked(self.shard(fh)).get(&fh).cloned()
    }

    /// Whether a file is open as `fh`.
    pub(super) fn contains(&self, fh: u64) -> bool {
        locked(self.shard(fh)).contains_key(&fh)
    }

    /// Closes `fh`, after the lock of its map is released.
    pub(super) fn remove(&self, fh: u64) {
        let closed = locked(self.shard(fh)).remove(&fh);
        drop(closed);
    }

    /// The number of files open.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.iter().map(|shard| locked(shard).len()).sum()
    }

    /// Whether the map of `fh` can be locked now: no request holds it.
    #[cfg(test)]
    pub(super) fn free(&self, fh: u64) -> bool {
        self.shard(fh).try_lock().is_ok()
    }
}

#[cfg(test)]
#[path = "handles_tests.rs"]
mod tests;
