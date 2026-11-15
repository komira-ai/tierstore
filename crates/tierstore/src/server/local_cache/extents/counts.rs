//! The counters and gauges of the entries by extents, in the text of `/metricz` after the
//! cache's own.

use std::fmt::Write as _;
use std::sync::atomic::AtomicU64;

use super::super::metrics::{bump, read};
use super::error::RangeError;

/// The reasons a fill of a range failed that are counted, in the order of [`LABELS`].
const LABELS: [&str; 9] = [
    "upstream",
    "mismatch",
    "short",
    "no_space",
    "busy",
    "read_only",
    "io",
    "changed",
    "aborted",
];

/// The counters of the entries by extents.
#[derive(Debug, Default)]
pub(in crate::server::local_cache) struct ExtentCounters {
    /// Ranges filled from the upstream.
    pub fills: AtomicU64,
    /// The bytes those ranges added to the cache.
    pub fill_bytes: AtomicU64,
    /// Requests that joined a fill of the same range that was running.
    pub joins: AtomicU64,
    /// Requests for a range the cache held already.
    pub held: AtomicU64,
    /// Entries that were filled whole and moved into `obj/`.
    pub promotions: AtomicU64,
    /// Entries removed by the eviction pass.
    pub evictions: AtomicU64,
    /// Fills that left nothing, by reason.
    failures: [AtomicU64; LABELS.len()],
}

impl ExtentCounters {
    /// Counts a fill that failed for `why`, if it is a failure of the fill (a bad request is not).
    pub(super) fn failed(&self, why: &RangeError) {
        let place = match why {
            RangeError::NotKept | RangeError::Empty | RangeError::Past => return,
            RangeError::Upstream(_) => 0,
            RangeError::Mismatch => 1,
            RangeError::Short => 2,
            RangeError::NoSpace => 3,
            RangeError::Busy => 4,
            RangeError::ReadOnly => 5,
            RangeError::Io => 6,
            RangeError::Changed => 7,
            RangeError::Aborted => 8,
        };
        bump(&self.failures[place]);
    }

    /// The fills that failed for the reason at `place` of [`LABELS`].
    #[cfg(test)]
    pub(super) fn failures(&self, label: &str) -> u64 {
        let place = LABELS.iter().position(|known| *known == label);
        place.map_or(0, |place| read(&self.failures[place]))
    }

    /// The text for `/metricz`, with the gauges measured now.
    pub(in crate::server::local_cache) fn render(&self, entries: u64, bytes: u64) -> String {
        let mut text = String::new();
        let plain = [
            ("tierstore_cache_extent_entries", "gauge", entries),
            ("tierstore_cache_extent_bytes", "gauge", bytes),
            (
                "tierstore_cache_extent_fills_total",
                "counter",
                read(&self.fills),
            ),
            (
                "tierstore_cache_extent_fill_bytes_total",
                "counter",
                read(&self.fill_bytes),
            ),
            (
                "tierstore_cache_extent_joins_total",
                "counter",
                read(&self.joins),
            ),
            (
                "tierstore_cache_extent_held_total",
                "counter",
                read(&self.held),
            ),
            (
                "tierstore_cache_extent_promotions_total",
                "counter",
                read(&self.promotions),
            ),
            (
                "tierstore_cache_extent_evictions_total",
                "counter",
                read(&self.evictions),
            ),
        ];
        for (name, kind, value) in plain {
            let _ = writeln!(text, "# TYPE {name} {kind}");
            let _ = writeln!(text, "{name} {value}");
        }
        let name = "tierstore_cache_extent_fill_failures_total";
        let _ = writeln!(text, "# TYPE {name} counter");
        for (label, value) in LABELS.iter().zip(&self.failures) {
            let _ = writeln!(text, "{name}{{reason=\"{label}\"}} {}", read(value));
        }
        text
    }
}
