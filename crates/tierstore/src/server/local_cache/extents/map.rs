//! The set of byte ranges of an object that an entry of `extents-v1` holds, verified.

use std::collections::BTreeMap;

/// Disjoint, non-adjacent half-open ranges `[start, end)` of one object, by start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::server::local_cache) struct ExtentMap {
    runs: BTreeMap<u64, u64>,
}

impl ExtentMap {
    /// The map of the `[offset, length]` pairs of a sidecar: none if a pair is empty, passes
    /// `size`, or is not after the one before it with a gap between (a sidecar this release
    /// writes is never otherwise, so one that is has been changed by hand or damaged).
    pub(in crate::server::local_cache) fn from_pairs(
        pairs: &[[u64; 2]],
        size: u64,
    ) -> Option<Self> {
        let mut runs = BTreeMap::new();
        let mut floor = 0_u64;
        for [offset, len] in pairs {
            let end = offset.checked_add(*len)?;
            let apart = runs.is_empty() || *offset > floor;
            if *len == 0 || end > size || !apart {
                return None;
            }
            runs.insert(*offset, end);
            floor = end;
        }
        Some(Self { runs })
    }

    /// The ranges as `[offset, length]` pairs, in order.
    pub(in crate::server::local_cache) fn pairs(&self) -> Vec<[u64; 2]> {
        self.runs
            .iter()
            .map(|(start, end)| [*start, end - start])
            .collect()
    }

    /// The bytes the ranges hold.
    pub(in crate::server::local_cache) fn bytes(&self) -> u64 {
        self.runs.iter().map(|(start, end)| end - start).sum()
    }

    /// Whether no range is held.
    pub(in crate::server::local_cache) fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Whether `[start, end)` is held whole (an empty range always is).
    pub(in crate::server::local_cache) fn covers(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        let below = self.runs.range(..=start).next_back();
        below.is_some_and(|(_, held_end)| *held_end >= end)
    }

    /// The parts of `[start, end)` that are not held, in order.
    pub(in crate::server::local_cache) fn missing(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        let mut at = start;
        // A range that starts before `start` and reaches into it is the first one to step over.
        let first = self.runs.range(..=start).next_back();
        let after = self.runs.range(start..);
        for (run_start, run_end) in first.into_iter().chain(after) {
            if at >= end {
                break;
            }
            if *run_start > at {
                gaps.push((at, (*run_start).min(end)));
            }
            at = at.max(*run_end);
        }
        if at < end {
            gaps.push((at, end));
        }
        gaps
    }

    /// Adds `[start, end)`, joining the ranges it touches; gives the bytes that were not held.
    pub(in crate::server::local_cache) fn insert(&mut self, start: u64, end: u64) -> u64 {
        if start >= end {
            return 0;
        }
        let added: u64 = self.missing(start, end).iter().map(|(s, e)| e - s).sum();
        let (mut low, mut high) = (start, end);
        let touching: Vec<(u64, u64)> = self
            .runs
            .range(..=end)
            .rev()
            .take_while(|(_, run_end)| **run_end >= start)
            .map(|(run_start, run_end)| (*run_start, *run_end))
            .collect();
        for (run_start, run_end) in touching {
            let _ = self.runs.remove(&run_start);
            low = low.min(run_start);
            high = high.max(run_end);
        }
        self.runs.insert(low, high);
        added
    }

    /// Whether the whole of `[0, size)` is held.
    pub(in crate::server::local_cache) fn is_whole(&self, size: u64) -> bool {
        self.covers(0, size)
    }
}
