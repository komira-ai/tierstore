//! The heap that the reads queued by the mount provider's preparations hold, and its bound.
//!
//! A preparation of a source queues the reads of every file of its tree (the provider's module
//! `prepare`), and the queue is drained only as fast as the node's reads of files go
//! (`trees.fill_concurrency` at once). A read queued holds the pack's name and the offset, size and
//! digest of each of its spans: about a hundred bytes of heap for every file. While the upstream
//! does not answer, the reads drain at a few a minute, and the queues of every source prepared
//! meanwhile stay on the heap. So the provider takes room here for a source's reads before it
//! queues them, and queues none when they do not fit under its bound: the files of that source are
//! then read when first opened, as with `trees.prepare_prefetch = "none"`. Each read gives its room
//! back once it has been read, refused or dropped (a read holds it while it waits for its turn and
//! while it runs).
//!
//! The cache keeps the count, and shows it on `/metricz`: the bytes the queued reads hold
//! (`tierstore_provider_prefetch_queued_bytes`) and the preparations whose reads were not queued
//! for want of room (`tierstore_provider_prefetch_skipped_total`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::LocalCache;

/// The gauge of the bytes held.
const QUEUED_BYTES: &str = "tierstore_provider_prefetch_queued_bytes";

/// The counter of the preparations whose reads were not queued.
const SKIPPED: &str = "tierstore_provider_prefetch_skipped_total";

/// The room taken by the queued reads of the node, and the preparations refused room.
#[derive(Debug, Default)]
pub(in crate::server) struct QueuedReads {
    bytes: AtomicU64,
    skipped: AtomicU64,
}

impl QueuedReads {
    /// Takes room for reads that hold `bytes`, if the reads queued then hold at most `most`:
    /// whether it was taken. A preparation refused room is counted.
    pub(in crate::server) fn take(&self, bytes: u64, most: u64) -> bool {
        let fits = |held: u64| held.checked_add(bytes).filter(|after| *after <= most);
        let taken = self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, fits)
            .is_ok();
        if !taken {
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// Gives back the room of reads that held `bytes`, taken before.
    pub(in crate::server) fn give(&self, bytes: u64) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
    }

    /// The bytes the queued reads hold now.
    pub(in crate::server) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }

    /// The preparations refused room so far.
    pub(in crate::server) fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// The two families of `/metricz`.
    pub(super) fn render(&self) -> String {
        let (bytes, skipped) = (self.bytes(), self.skipped());
        format!(
            "# TYPE {QUEUED_BYTES} gauge\n{QUEUED_BYTES} {bytes}\n\
             # TYPE {SKIPPED} counter\n{SKIPPED} {skipped}\n"
        )
    }
}

impl LocalCache {
    /// The room of the reads the mount provider's preparations queue (module docs).
    pub(in crate::server) fn queued_reads(&self) -> Arc<QueuedReads> {
        Arc::clone(&self.inner.queued_reads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that room is taken while it fits under the bound, exactly up to it, that a take
    /// past it is refused and counted and takes nothing, that room given back can be taken again,
    /// and that a take that would overflow the count is refused.
    #[test]
    fn room_is_taken_up_to_the_bound_and_given_back() {
        let reads = QueuedReads::default();
        assert!(reads.take(60, 100));
        assert!(reads.take(40, 100));
        assert_eq!(reads.bytes(), 100);
        assert!(!reads.take(1, 100));
        assert_eq!((reads.bytes(), reads.skipped()), (100, 1));
        reads.give(40);
        assert!(reads.take(40, 100));
        reads.give(100);
        assert_eq!(reads.bytes(), 0);
        assert!(reads.take(1, u64::MAX));
        assert!(!reads.take(u64::MAX, u64::MAX));
        assert_eq!((reads.bytes(), reads.skipped()), (1, 2));
    }

    /// Asserts the text of the two families, in full.
    #[test]
    fn the_families_are_a_gauge_of_the_bytes_and_a_counter_of_the_refusals() {
        let reads = QueuedReads::default();
        assert!(reads.take(4096, 8192));
        assert!(!reads.take(8192, 8192));
        assert_eq!(
            reads.render(),
            "# TYPE tierstore_provider_prefetch_queued_bytes gauge\n\
             tierstore_provider_prefetch_queued_bytes 4096\n\
             # TYPE tierstore_provider_prefetch_skipped_total counter\n\
             tierstore_provider_prefetch_skipped_total 1\n"
        );
    }
}
