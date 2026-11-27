//! What one read of a tree from its manifests fetched (`core/placed.rs`): each manifest read from
//! the cache directory, filled through the cache and then read, or not had; and how long each
//! fill took on the core's clock. The fetches of a read run on several threads at once, so the
//! counts are atomic. The read reports them to the node when it ends (`fetched`, see
//! `local_cache/attaches.rs`), and an attach on a lookup says them on stderr.
//!
//! The fetches are also counted by the level of the tree they are of (0 for the root): how many
//! levels the read went down, the fills of each level and the longest of them, since a level's
//! fills are waited for before the level below is asked for. A read that asked the node to fill the
//! tree's new manifests itself (`fill-tree`) keeps its answer and how long it took.
//! The levels of the read and its rounds of fills go to the node's cache too (`levels`).
//! The end of its last fetch is kept for the attach of a run, which splits its time there.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::super::fills::TreeAnswer;
use super::locked;
use crate::server::local_cache::attaches::{BUCKETS, Fetches, Levels, bucket_of};

/// The fetches of one read of a tree, counted as they end.
#[derive(Debug, Default)]
pub(in crate::server::treefs) struct Fetched {
    cache: AtomicU64,
    filled: AtomicU64,
    failed: AtomicU64,
    fill_ms: AtomicU64,
    fills: [AtomicU64; BUCKETS],
    /// By level, from the root's: the fills asked for, and the longest of them in milliseconds.
    levels: Mutex<Vec<Level>>,
    /// The `fill-tree` of the read, if it asked one.
    tree: Mutex<Option<TreeAsked>>,
    /// When the last fetch of the read ended, on the core's clock: none before the first.
    last: Mutex<Option<u64>>,
}

/// The fills of one level of a read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Level {
    fills: u64,
    slowest_ms: u64,
}

/// A `fill-tree` of a read: its answer, and the milliseconds from the request to the answer on the
/// core's clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server::treefs) struct TreeAsked {
    pub answer: TreeAnswer,
    pub ms: u64,
}

fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_add(1, Ordering::Relaxed);
}

/// The numbers `of`, each after a space.
fn spaced(of: impl Iterator<Item = u64>) -> String {
    of.map(|n| format!(" {n}")).collect()
}

impl Fetched {
    /// A fetch of the read ended at `now`, whatever it had.
    pub(super) fn fetched_at(&self, now: u64) {
        let mut last = locked(&self.last);
        *last = Some(last.map_or(now, |last| last.max(now)));
    }

    /// When the last fetch of the read ended, if it made one: the end of the fetch of an attach of
    /// a run (`core/runs.rs`).
    pub(super) fn last_fetch(&self) -> Option<u64> {
        *locked(&self.last)
    }

    /// The level `level` of the read, made with those above it if it is the first fetch of it.
    fn at(&self, level: usize, change: impl FnOnce(&mut Level)) {
        let mut levels = locked(&self.levels);
        if levels.len() <= level {
            levels.resize(level + 1, Level::default());
        }
        change(&mut levels[level]);
    }

    /// A manifest of the level `level` was read from the cache directory.
    pub(super) fn cached(&self, level: usize) {
        bump(&self.cache);
        self.at(level, |_| {});
    }

    /// The root was read from the cache directory and is a tree list, not a manifest: its level
    /// is counted, and no manifest.
    pub(super) fn listed(&self) {
        self.at(0, |_| {});
    }

    /// A manifest of the level `level` was asked of the node's cache, which took `ms` milliseconds
    /// to answer; `had` says whether the manifest was then read from the cache directory.
    pub(super) fn filled(&self, level: usize, ms: u64, had: bool) {
        bump(if had { &self.filled } else { &self.failed });
        let _ = self.fill_ms.fetch_add(ms, Ordering::Relaxed);
        bump(&self.fills[bucket_of(ms)]);
        self.at(level, |level| {
            level.fills += 1;
            level.slowest_ms = level.slowest_ms.max(ms);
        });
    }

    /// The read asked the node to fill the tree's new manifests (`fill-tree`).
    pub(super) fn asked_tree(&self, asked: TreeAsked) {
        *locked(&self.tree) = Some(asked);
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

    /// The levels the read went down and its rounds of fills so far: one for each
    /// level with a fill, since each waits for the one above it, and one for a `fill-tree`.
    pub(in crate::server::treefs) fn levels(&self) -> Levels {
        let levels = locked(&self.levels);
        let filling = levels.iter().filter(|level| level.fills > 0).count();
        let tree = usize::from(locked(&self.tree).is_some());
        Levels {
            levels: levels.len() as u64,
            rounds: (filling + tree) as u64,
        }
    }

    /// What the read did by level, for its line on stderr: nothing for a read that fetched no
    /// manifest; else `; <n> levels; fills by level: ..., slowest ms by level: ...`, and the
    /// `fill-tree` of the read, if it asked one.
    pub(in crate::server::treefs) fn by_level(&self) -> String {
        let levels = locked(&self.levels).clone();
        if levels.is_empty() {
            return String::new();
        }
        let mut said = format!(
            "; {} levels; fills by level:{}, slowest ms by level:{}",
            levels.len(),
            spaced(levels.iter().map(|level| level.fills)),
            spaced(levels.iter().map(|level| level.slowest_ms)),
        );
        if let Some(asked) = &*locked(&self.tree) {
            let ms = asked.ms;
            said.push_str(&match &asked.answer {
                TreeAnswer::Filled(filled) => format!(
                    "; fill-tree ok in {ms} ms, {} ms at the node, fills by level:{}",
                    filled.ms,
                    spaced(filled.levels.iter().copied()),
                ),
                TreeAnswer::Failed(why) => format!("; fill-tree failed in {ms} ms: {why}"),
            });
        }
        said
    }
}

#[cfg(test)]
#[path = "fetched_tests.rs"]
mod tests;
