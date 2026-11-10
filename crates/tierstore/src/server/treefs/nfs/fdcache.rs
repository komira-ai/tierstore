//! The cache of open cache files: a file read once is read again without opening it, and without
//! the core. It is sharded by inode number so that reads of different files do not meet on one
//! lock, and each shard keeps the files in the order they were opened and closes the oldest first
//! (a cache file is immutable, so a descriptor never goes stale).

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::sync::{Arc, Mutex};

use crate::server::treefs::locked;

/// One shard.
#[derive(Default)]
struct Shard {
    files: HashMap<u64, Arc<File>>,
    /// The inodes of `files`, oldest first.
    order: VecDeque<u64>,
}

/// The open cache files of the files read lately.
pub(super) struct FdCache {
    shards: Vec<Mutex<Shard>>,
    per_shard: usize,
}

impl FdCache {
    /// A cache of `shards` shards that keep `per_shard` files each.
    pub(super) fn new(shards: usize, per_shard: usize) -> Self {
        Self {
            shards: (0..shards.max(1)).map(|_| Mutex::default()).collect(),
            per_shard: per_shard.max(1),
        }
    }

    fn shard(&self, ino: u64) -> &Mutex<Shard> {
        // The remainder is below the length of `shards`.
        &self.shards[(ino % self.shards.len() as u64) as usize]
    }

    /// The open file of `ino`, if the cache has it.
    pub(super) fn get(&self, ino: u64) -> Option<Arc<File>> {
        locked(self.shard(ino)).files.get(&ino).cloned()
    }

    /// Keeps `file` as the open file of `ino`, closing the oldest of its shard if the shard is
    /// full.
    pub(super) fn insert(&self, ino: u64, file: Arc<File>) {
        let mut shard = locked(self.shard(ino));
        if shard.files.insert(ino, file).is_some() {
            return;
        }
        shard.order.push_back(ino);
        if shard.order.len() > self.per_shard
            && let Some(oldest) = shard.order.pop_front()
        {
            let _ = shard.files.remove(&oldest);
        }
    }

    /// The files kept now.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| locked(shard).files.len())
            .sum()
    }
}

#[cfg(test)]
#[path = "fdcache_tests.rs"]
mod tests;
