//! The counts of the mount provider's sources and preparations, which the cache shows on
//! `/metricz` (`metrics.rs`) beside the reads they queue (`queued_reads.rs`).
//!
//! The provider holds a source in a table from its `Prepare` until its `Release` or a `Sweep`,
//! and holds at most `trees.max_sources` of them: a `Prepare` of a new source past that is
//! refused `UNAVAILABLE` and counted. A preparation reads its tree's list whole (at most 256 MiB)
//! and walks it, so at most `trees.prepare_concurrency` of them read and make their tree at once;
//! the others wait for a turn, holding nothing of their list, and a `Mount` of their source waits
//! as for any preparation.
//!
//! Families: `tierstore_provider_sources` (a gauge, the sources the table holds),
//! `tierstore_provider_preparing` (a gauge, the preparations that hold a turn) and
//! `tierstore_provider_prepare_refused_total` (a counter, the `Prepare` calls refused at the bound
//! of the table).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::LocalCache;

/// The gauge of the sources held.
const SOURCES: &str = "tierstore_provider_sources";

/// The gauge of the preparations that hold a turn.
const PREPARING: &str = "tierstore_provider_preparing";

/// The counter of the `Prepare` calls refused at the bound of the table.
const REFUSED: &str = "tierstore_provider_prepare_refused_total";

/// The counts of the provider of the node.
#[derive(Debug, Default)]
pub(in crate::server) struct Preparations {
    sources: AtomicU64,
    preparing: AtomicU64,
    refused: AtomicU64,
}

/// A preparation that holds a turn, counted until it is dropped.
pub(in crate::server) struct Preparing(Arc<Preparations>);

impl Drop for Preparing {
    fn drop(&mut self) {
        self.0.preparing.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Preparations {
    /// Says that the table holds `sources` sources now.
    pub(in crate::server) fn set_sources(&self, sources: usize) {
        self.sources.store(sources as u64, Ordering::Release);
    }

    /// Counts a `Prepare` refused at the bound of the table.
    pub(in crate::server) fn refuse(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a preparation that took its turn, until the guard is dropped.
    pub(in crate::server) fn enter(self: &Arc<Self>) -> Preparing {
        self.preparing.fetch_add(1, Ordering::AcqRel);
        Preparing(Arc::clone(self))
    }

    /// The sources held, the preparations that hold a turn, and the refusals so far.
    pub(in crate::server) fn counts(&self) -> (u64, u64, u64) {
        (
            self.sources.load(Ordering::Acquire),
            self.preparing.load(Ordering::Acquire),
            self.refused.load(Ordering::Relaxed),
        )
    }

    /// The three families on `/metricz` (module docs).
    pub(in crate::server) fn render(&self) -> String {
        let (sources, preparing, refused) = self.counts();
        families(sources, preparing, refused)
    }
}

/// The text of the families: two gauges and a counter, each with its type line.
fn families(sources: u64, preparing: u64, refused: u64) -> String {
    format!(
        "# TYPE {SOURCES} gauge\n{SOURCES} {sources}\n\
         # TYPE {PREPARING} gauge\n{PREPARING} {preparing}\n\
         # TYPE {REFUSED} counter\n{REFUSED} {refused}\n"
    )
}

impl LocalCache {
    /// The counts of the mount provider's sources and preparations (module docs).
    pub(in crate::server) fn preparations(&self) -> Arc<Preparations> {
        Arc::clone(&self.inner.preparations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the gauges follow what they are told and the guards they gave, and that the
    /// families are their full text, by exact equality.
    #[test]
    fn the_counts_follow_the_table_and_the_turns_and_render_in_full() {
        let counts = Arc::new(Preparations::default());
        counts.set_sources(3);
        let first = counts.enter();
        let second = counts.enter();
        counts.refuse();
        assert_eq!(counts.counts(), (3, 2, 1));
        drop(first);
        assert_eq!(counts.counts(), (3, 1, 1));
        assert_eq!(
            counts.render(),
            "# TYPE tierstore_provider_sources gauge\n\
             tierstore_provider_sources 3\n\
             # TYPE tierstore_provider_preparing gauge\n\
             tierstore_provider_preparing 1\n\
             # TYPE tierstore_provider_prepare_refused_total counter\n\
             tierstore_provider_prepare_refused_total 1\n"
        );
        drop(second);
        counts.set_sources(0);
        assert_eq!(counts.counts(), (0, 0, 1));
    }
}
