//! A scripted workload crashed at every step: the files as each way of tearing leaves them, the store
//! as each prefix of its batches after the last flush leaves it, recovered, gives the index of the
//! acknowledged operations.
//!
//! The memory store is the only one with a crash model (`MemBackend::crashed`), so these tests run
//! on it alone; the generic tests of the replay and rebuild files run on every store kind.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod recover_support;

use std::sync::Arc;

use recover_support::*;
use tierstore_index::backend::MemBackend;
use tierstore_index::fs::fake::Torn;
use tierstore_index::recover::Mode;

/// Puts, a delete and a put of the same key again with other bytes, seals, and flushes of the
/// mark in the middle of segments and just after a seal.
const WORKLOAD: [Op; 17] = [
    Op::Put("a", "A1", 10),
    Op::Put("b", "B1", 11),
    Op::Mark,
    Op::Put("c", "C1", 12),
    Op::Delete("a", 20),
    Op::Seal,
    Op::Mark,
    Op::Put("d", "D1", 21),
    Op::Put("a", "A2", 30),
    Op::Mark,
    Op::Delete("b", 31),
    Op::Seal,
    Op::Put("e", "E1", 40),
    Op::Mark,
    Op::Delete("d", 41),
    Op::Put("zz", "Z1", 42),
    Op::Seal,
];

/// Recovers the crash state `(files, store)` and returns the view of the result and the mode.
fn recovered(
    files: &tierstore_index::fs::fake::FakeFs,
    store: &MemBackend,
    empty: bool,
) -> (View, Mode) {
    let hot = Arc::new(files.crash(Torn::Kept));
    let disks = Disks::new(hot, Vec::new());
    let index = if empty {
        index_on(MemBackend::new())
    } else {
        index_on(store.crashed(usize::MAX))
    };
    let report = disks.recover(&index).unwrap();
    (view(&index, &KEYS, &blocks()), report.mode)
}

#[test]
fn a_workload_crashed_at_every_step_recovers_to_its_acknowledged_operations() {
    let mut node = Node::new(true);
    for op in WORKLOAD {
        node.run(op);
    }
    let total = node.acked();
    let references: Vec<View> = (0..=total)
        .map(|n| view(&node.reference(n), &KEYS, &blocks()))
        .collect();
    let mut replays = 0;
    let mut rebuilds = 0;
    for (at, snap) in node.snaps.iter().enumerate() {
        for (t, files) in snap.files.iter().enumerate() {
            // Every prefix of the batches after the last flush, and no usable store at all.
            let stores = snap.stores.iter().map(|s| (s, false));
            for (k, (store, empty)) in stores.chain([(&snap.stores[0], true)]).enumerate() {
                let (got, mode) = recovered(files, store, empty);
                match mode {
                    Mode::Replay => replays += 1,
                    Mode::Rebuild => rebuilds += 1,
                }
                // The operation in flight at the crash is either whole or absent.
                let ok = got == references[snap.acked]
                    || (snap.acked < total && got == references[snap.acked + 1]);
                assert!(
                    ok,
                    "snapshot {at} (acked {}), torn variant {t}, store prefix {k}, empty {empty}:\n{got:#?}",
                    snap.acked
                );
            }
        }
    }
    assert!(
        replays > 100 && rebuilds > 100,
        "{replays} replays, {rebuilds} rebuilds"
    );
}

#[test]
fn every_torn_variant_of_the_files_is_distinct_from_the_whole_files_at_some_step() {
    // The tearing matters: some step has files that differ between Lost and Kept.
    let mut node = Node::new(true);
    for op in WORKLOAD {
        node.run(op);
    }
    let differs = node.snaps.iter().any(|s| {
        let lost = &s.files[0];
        let kept = &s.files[1];
        (1..=4u64).any(|n| lost.bytes(&seg_path(block(n))) != kept.bytes(&seg_path(block(n))))
    });
    assert!(differs);
}
