//! [`Committer`]: the one committer of a node (section 8, "Batching"): every dirty entry is
//! committed once per interval, or sooner once enough bytes are pending, never once per part or
//! per demand sub-range.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tierstore_fetch_api::entry::{Entry, EntryError};
use tierstore_fetch_api::ids::PackId;

/// When the committer commits (section 8: "every 1 s or at 32 MiB pending").
///
/// The defaults are 1000 ms and 32 MiB. Both deployment profiles of section 21 keep the cache on
/// a partition of one SSD (section 11), so one pair of values serves both. They are starting
/// constants, marked [I] as section 15 asks until a measurement replaces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitPolicy {
    /// The interval between rounds, in milliseconds of the injected clock.
    pub interval_ms: u64,
    /// The bytes published across the node's entries that start a round before the interval.
    pub pending_bytes: u64,
}

impl Default for CommitPolicy {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            pending_bytes: 32 << 20,
        }
    }
}

/// What one round did, summed over its entries.
#[derive(Debug, Default)]
pub struct CommitRound {
    /// The entries committed.
    pub entries: u32,
    /// `fdatasync`s of data files: at most one per entry.
    pub data_syncs: u32,
    /// Cells newly durable.
    pub cells_durable: u32,
    /// Journal records appended.
    pub records: u32,
    /// The entries whose commit failed, with the error (the store reopens them).
    pub failed: Vec<(PackId, EntryError)>,
}

#[derive(Default)]
struct State {
    /// The entries with cells published since the last round.
    dirty: BTreeMap<PackId, Arc<dyn Entry>>,
    /// The bytes published since the last round.
    pending: u64,
    /// The clock at the last round (0 before the first).
    last_round_ms: u64,
}

/// The committer of a node. The fill calls [`Committer::published`] after it publishes cells,
/// and the evictor calls it (with 0 bytes) after it aborts the eviction of a cell that was not
/// durable, so the entry is visited again (a round that met the cell `evicting` left it pending
/// in the entry's journal); a loop of the server calls [`Committer::commit_due`] on its clock's ticks (and at once when
/// `published` says the bytes are reached). The loop runs on a blocking thread: a round does the
/// entries' `fdatasync`s.
pub struct Committer {
    policy: CommitPolicy,
    state: Mutex<State>,
}

impl std::fmt::Debug for Committer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("Committer")
            .field("policy", &self.policy)
            .field("dirty", &s.dirty.keys().collect::<Vec<_>>())
            .field("pending", &s.pending)
            .field("last_round_ms", &s.last_round_ms)
            .finish()
    }
}

impl Committer {
    /// A committer with `policy`.
    #[must_use]
    pub fn new(policy: CommitPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Each update of the state runs under the lock and does no I/O, so a poisoned lock still
        // guards a usable state.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `bytes` of `entry`'s cells were published. Whether the pending bytes reached
    /// [`CommitPolicy::pending_bytes`], so a round is due now.
    pub fn published(&self, entry: &Arc<dyn Entry>, bytes: u64) -> bool {
        let mut s = self.lock();
        s.dirty
            .entry(entry.pack().clone())
            .or_insert_with(|| entry.clone());
        s.pending = s.pending.saturating_add(bytes);
        s.pending >= self.policy.pending_bytes
    }

    /// Whether a round is due at `now_ms`: something is dirty, and the interval has passed since
    /// the last round or the pending bytes are reached.
    #[must_use]
    pub fn due(&self, now_ms: u64) -> bool {
        self.is_due(&self.lock(), now_ms)
    }

    fn is_due(&self, s: &State, now_ms: u64) -> bool {
        !s.dirty.is_empty()
            && (now_ms.saturating_sub(s.last_round_ms) >= self.policy.interval_ms
                || s.pending >= self.policy.pending_bytes)
    }

    /// Runs a round if one is due at `now_ms`: `Entry::commit_batch` once on every dirty entry,
    /// in pack order. `None` when none is due. The dirty set is taken before the commits, so a
    /// cell published during the round is in the next one.
    pub fn commit_due(&self, now_ms: u64) -> Option<CommitRound> {
        let dirty = {
            let mut s = self.lock();
            if !self.is_due(&s, now_ms) {
                return None;
            }
            s.pending = 0;
            s.last_round_ms = now_ms;
            std::mem::take(&mut s.dirty)
        };
        let mut round = CommitRound::default();
        for (pack, entry) in dirty {
            round.entries += 1;
            match entry.commit_batch() {
                Ok(r) => {
                    round.data_syncs += r.data_syncs;
                    round.cells_durable += r.cells_durable;
                    round.records += r.records;
                }
                Err(e) => round.failed.push((pack, e)),
            }
        }
        Some(round)
    }

    /// The entries with cells published since the last round.
    #[must_use]
    pub fn dirty(&self) -> usize {
        self.lock().dirty.len()
    }
}
