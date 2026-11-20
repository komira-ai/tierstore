//! What one read of a tree from its manifests fetched (`core/placed.rs`): each manifest read from
//! the cache directory, filled through the fill socket and then read, or not had; and how long each
//! fill took on the core's clock. The fetches of a read run on several threads at once, so the
//! counts are atomic. The read reports them to the node when it ends (`fetched`, see
//! `local_cache/attaches.rs`), and an attach on a lookup says them on stderr.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::server::local_cache::attaches::{BUCKETS, Fetches, bucket_of};

/// The fetches of one read of a tree, counted as they end.
#[derive(Debug, Default)]
pub(in crate::server::treefs) struct Fetched {
    cache: AtomicU64,
    filled: AtomicU64,
    failed: AtomicU64,
    fill_ms: AtomicU64,
    fills: [AtomicU64; BUCKETS],
}

fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_add(1, Ordering::Relaxed);
}

impl Fetched {
    /// A manifest was read from the cache directory.
    pub(super) fn cached(&self) {
        bump(&self.cache);
    }

    /// A manifest was asked of the fill socket, which took `ms` milliseconds to answer; `had`
    /// says whether the manifest was then read from the cache directory.
    pub(super) fn filled(&self, ms: u64, had: bool) {
        bump(if had { &self.filled } else { &self.failed });
        let _ = self.fill_ms.fetch_add(ms, Ordering::Relaxed);
        bump(&self.fills[bucket_of(ms)]);
    }

    /// The counts so far.
    pub(in crate::server::treefs) fn counts(&self) -> Fetches {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Fetches {
            cache: load(&self.cache),
            filled: load(&self.filled),
            failed: load(&self.failed),
            fill_ms: load(&self.fill_ms),
            fills: std::array::from_fn(|bucket| load(&self.fills[bucket])),
        }
    }
}
