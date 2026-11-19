//! The evictor's per-cell protocol of 5.1 over a batch of entries.
//!
//! 1. For each candidate cell, CAS `present -> evicting`; only the cells won go on. A cell in any
//!    other state (`claimed` by a fetch, `absent`, `recheck`, `bad`) is not touched.
//! 2. For each 16-cell group with a won cell, sum the group's reader counters over every holder
//!    (`SeqCst`, [`Entry::group_readers`]) until the sum is 0, pausing between polls.
//! 3. A group whose sum has not reached 0 by `evict_wait` after the wait began: its won cells
//!    go back `evicting -> present`. Nothing of it was journaled (wait first, journal second).
//! 4. For each entry, the won cells of its clear groups as contiguous runs: one
//!    [`Entry::punch`] (one batch of `PUNCH` records, one journal `fsync`, then the punches),
//!    then CAS `evicting -> absent` on each.
//!
//! One wait serves every entry of the batch, so a batch waits `evict_wait` at most.

use std::collections::BTreeSet;
use std::sync::Arc;

use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::entry::{Entry, EntryError, EvictStep};
use tierstore_fetch_api::ids::{CellRun, PackId};
use tierstore_fetch_api::page::GROUP_CELLS;

use crate::pause::Pause;

/// The candidate cells of one entry, ascending.
pub(crate) struct Batch {
    pub entry: Arc<dyn Entry>,
    pub cells: Vec<u32>,
}

/// What a run of the protocol did.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// Cells won by `present -> evicting`.
    pub won: u64,
    /// Cells taken to `absent` after their entry's punch.
    pub evicted: u64,
    /// Bytes the punches released.
    pub bytes_punched: u64,
    /// Journal `fsync`s of the punches.
    pub journal_syncs: u32,
    /// Groups given up at `evict_wait`.
    pub groups_timed_out: u32,
    /// Cells put back `present` because their group stayed busy.
    pub restored: u64,
    /// The entries whose punch failed, with the error. Their cells went to `absent` all the
    /// same: whether or not their bytes were released, a later fetch overwrites them.
    pub punch_failures: Vec<(PackId, EntryError)>,
}

/// Runs steps 1 to 4 over `batches`, waiting `wait_ms` of `clock` at most.
pub(crate) fn run(
    batches: &[Batch],
    clock: &dyn Clock,
    pause: &dyn Pause,
    wait_ms: u64,
) -> Outcome {
    let mut out = Outcome::default();

    // Step 1: the cells won, per batch.
    let won: Vec<Vec<u32>> = batches
        .iter()
        .map(|b| {
            b.cells
                .iter()
                .copied()
                .filter(|&c| b.entry.evict_cell_cas(c, EvictStep::Begin))
                .collect()
        })
        .collect();
    out.won = won.iter().map(|w| w.len() as u64).sum();

    // Step 2: wait for every group with a won cell, as `(batch, group)`.
    let mut pending: BTreeSet<(usize, u32)> = won
        .iter()
        .enumerate()
        .flat_map(|(i, cells)| cells.iter().map(move |c| (i, c / GROUP_CELLS)))
        .collect();
    let deadline = clock.now_ms().saturating_add(wait_ms);
    let mut attempt = 0u32;
    loop {
        pending.retain(|&(i, g)| batches[i].entry.group_readers(g) != 0);
        if pending.is_empty() || clock.now_ms() >= deadline {
            break;
        }
        pause.pause(attempt);
        attempt = attempt.saturating_add(1);
    }

    // Step 3: the groups still busy go back to `present`, unjournaled.
    out.groups_timed_out = u32::try_from(pending.len()).unwrap_or(u32::MAX);
    for (i, (batch, cells)) in batches.iter().zip(&won).enumerate() {
        let (busy, clear): (Vec<u32>, Vec<u32>) = cells
            .iter()
            .partition(|&&c| pending.contains(&(i, c / GROUP_CELLS)));
        for &c in &busy {
            // Only the evictor leaves `evicting`, so the CAS of a won cell wins.
            batch.entry.evict_cell_cas(c, EvictStep::Abort);
        }
        out.restored += busy.len() as u64;

        // Step 4: one journaled punch for the entry's clear cells, then `absent`.
        let runs = runs_of(&clear);
        if runs.is_empty() {
            continue;
        }
        match batch.entry.punch(&runs) {
            Ok(report) => {
                out.bytes_punched += report.bytes_punched;
                out.journal_syncs += report.journal_syncs;
            }
            Err(e) => out.punch_failures.push((batch.entry.pack().clone(), e)),
        }
        for &c in &clear {
            batch.entry.evict_cell_cas(c, EvictStep::Finish);
        }
        out.evicted += clear.len() as u64;
    }
    out
}

/// The contiguous runs of `cells` (ascending, no repeats).
pub(crate) fn runs_of(cells: &[u32]) -> Vec<CellRun> {
    let mut runs: Vec<CellRun> = Vec::new();
    for &c in cells {
        match runs.last_mut() {
            Some(r) if r.first + r.count == c => r.count += 1,
            _ => runs.push(CellRun { first: c, count: 1 }),
        }
    }
    runs
}
