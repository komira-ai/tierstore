//! `delete`, `relocate`, the promoted-copy hint and the boot epoch, on the in-memory store.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_format::ids::NodeId;
use tierstore_format::record::RecordFlags;
use tierstore_format::tomb::Named;
use tierstore_index::KvIndex;
use tierstore_store_api::index::{DeleteOutcome, HotClean, Index, Lookup};

fn copy(node: u8, boot: u64) -> HotClean {
    HotClean {
        node: NodeId(node),
        boot,
        seq: 3,
        offset: 9,
    }
}

#[test]
fn delete_of_a_live_entry_returns_it_lowers_the_counts_and_leaves_a_tombstone_entry() {
    let index = ready();
    let e = entry(1, 5, 0, 5, 100);
    index.put_if_absent(b"k", e.clone()).unwrap();
    assert_eq!(
        index.delete(&tomb(b"k", 1, 9, vec![])).unwrap(),
        DeleteOutcome::Deleted(e)
    );
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(9)));
    assert!(index.list(b"", None, 10).unwrap().is_empty());
    // The tombstone names the block of the entry it deleted.
    let tombs = index.tombstones().unwrap();
    assert_eq!(tombs.len(), 1);
    assert_eq!(tombs[0].names, vec![Named::Block(block(5))]);
    assert_eq!(tombs[0].generation, generation(1));
}

#[test]
fn delete_is_idempotent_per_key_and_stamp() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    let t = tomb(b"k", 1, 9, vec![]);
    assert!(matches!(
        index.delete(&t).unwrap(),
        DeleteOutcome::Deleted(_)
    ));
    let after_first = index.backend().dump();
    assert_eq!(index.delete(&t).unwrap(), DeleteOutcome::NoLiveEntry);
    assert_eq!(index.backend().dump(), after_first);
    assert_eq!(index.tombstones().unwrap().len(), 1);
}

#[test]
fn delete_of_an_absent_key_keeps_the_tombstone_and_over_a_tombstone_the_larger_stamp() {
    let index = ready();
    assert_eq!(
        index.delete(&tomb(b"k", 0, 4, vec![])).unwrap(),
        DeleteOutcome::NoLiveEntry
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(4)));
    assert_eq!(
        index.delete(&tomb(b"k", 0, 8, vec![])).unwrap(),
        DeleteOutcome::NoLiveEntry
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(8)));
    assert_eq!(
        index.delete(&tomb(b"k", 0, 6, vec![])).unwrap(),
        DeleteOutcome::NoLiveEntry
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(8)));
    let stamps: Vec<_> = index
        .tombstones()
        .unwrap()
        .iter()
        .map(|t| t.stamp)
        .collect();
    assert_eq!(stamps, vec![stamp(4), stamp(6), stamp(8)]);
}

#[test]
fn delete_at_or_below_the_entry_stamp_answers_newer_and_the_entry_stays_live() {
    let index = ready();
    let e = entry(1, 5, 0, 10, 100);
    index.put_if_absent(b"k", e.clone()).unwrap();
    for s in [10, 7] {
        assert_eq!(
            index.delete(&tomb(b"k", 1, s, vec![])).unwrap(),
            DeleteOutcome::Newer(e.clone())
        );
    }
    assert_eq!(index.get(b"k").unwrap(), Lookup::Live(e));
    assert_eq!(count(&index, 5), lc(1, 100));
}

#[test]
fn a_tombstone_keeps_the_names_it_came_with_sorted_and_without_duplicates() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .delete(&tomb(
            b"k",
            1,
            9,
            vec![Named::Block(block(7)), Named::Block(block(5))],
        ))
        .unwrap();
    assert_eq!(
        index.tombstones().unwrap()[0].names,
        vec![Named::Block(block(5)), Named::Block(block(7))]
    );
}

#[test]
fn relocate_moves_the_entry_while_generation_and_stamp_match() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    assert!(
        index
            .relocate(b"k", generation(1), stamp(5), at(6, 64))
            .unwrap()
    );
    let moved = live_entry(&index, b"k");
    assert_eq!(moved.location, at(6, 64));
    assert_eq!(moved.stamp, stamp(5));
    assert_eq!(moved.generation, generation(1));
    assert!(moved.flags.contains(RecordFlags::RELOCATE));
    assert_eq!(moved.prior_blocks, vec![block(5)]);
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 100));
}

#[test]
fn relocate_is_idempotent_and_skipped_for_another_generation_stamp_or_key_state() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    assert!(
        index
            .relocate(b"k", generation(1), stamp(5), at(6, 0))
            .unwrap()
    );
    let after = index.backend().dump();
    // Already there.
    assert!(
        !index
            .relocate(b"k", generation(1), stamp(5), at(6, 0))
            .unwrap()
    );
    // Another generation, another stamp, an absent key.
    assert!(
        !index
            .relocate(b"k", generation(2), stamp(5), at(7, 0))
            .unwrap()
    );
    assert!(
        !index
            .relocate(b"k", generation(1), stamp(6), at(7, 0))
            .unwrap()
    );
    assert!(
        !index
            .relocate(b"nope", generation(1), stamp(5), at(7, 0))
            .unwrap()
    );
    assert_eq!(index.backend().dump(), after);
}

#[test]
fn relocate_back_to_the_first_block_drops_it_from_the_prior_blocks() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .relocate(b"k", generation(1), stamp(5), at(6, 0))
        .unwrap();
    index
        .relocate(b"k", generation(1), stamp(5), at(5, 128))
        .unwrap();
    assert_eq!(live_entry(&index, b"k").prior_blocks, vec![block(6)]);
    assert_eq!(count(&index, 5), lc(1, 100));
    assert_eq!(count(&index, 6), lc(0, 0));
}

#[test]
fn a_relocate_after_a_delete_is_skipped_and_the_key_stays_deleted() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index.delete(&tomb(b"k", 1, 9, vec![])).unwrap();
    assert!(
        !index
            .relocate(b"k", generation(1), stamp(5), at(6, 0))
            .unwrap()
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(9)));
    assert_eq!(count(&index, 6), lc(0, 0));
}

#[test]
fn a_delete_after_a_relocate_deletes_the_moved_entry_and_names_both_blocks() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .relocate(b"k", generation(1), stamp(5), at(6, 0))
        .unwrap();
    let DeleteOutcome::Deleted(e) = index.delete(&tomb(b"k", 1, 9, vec![])).unwrap() else {
        panic!("the moved entry was not deleted");
    };
    assert_eq!(e.location, at(6, 0));
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(9)));
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(0, 0));
    assert_eq!(
        index.tombstones().unwrap()[0].names,
        vec![Named::Block(block(5)), Named::Block(block(6))]
    );
}

#[test]
fn a_relocate_does_not_apply_over_a_newer_record() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    replay(&index, vec![record(b"k", entry(2, 8, 0, 9, 50))]);
    assert!(
        !index
            .relocate(b"k", generation(1), stamp(5), at(6, 0))
            .unwrap()
    );
    assert_eq!(live_entry(&index, b"k").location, at(8, 0));
    assert_eq!(count(&index, 6), lc(0, 0));
}

#[test]
fn promote_sets_the_hint_only_while_generation_and_stamp_match() {
    let index = ready();
    index.set_boot(NodeId(2), 7).unwrap();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    assert!(
        !index
            .promote(b"k", generation(2), stamp(5), copy(2, 7))
            .unwrap()
    );
    assert!(
        !index
            .promote(b"k", generation(1), stamp(6), copy(2, 7))
            .unwrap()
    );
    assert!(
        !index
            .promote(b"nope", generation(1), stamp(5), copy(2, 7))
            .unwrap()
    );
    assert_eq!(live_entry(&index, b"k").promoted, None);
    assert!(
        index
            .promote(b"k", generation(1), stamp(5), copy(2, 7))
            .unwrap()
    );
    assert_eq!(live_entry(&index, b"k").promoted, Some(copy(2, 7)));
    // The same hint again changes nothing.
    assert!(
        !index
            .promote(b"k", generation(1), stamp(5), copy(2, 7))
            .unwrap()
    );
    // A tombstone that raced the promotion wins.
    index.delete(&tomb(b"k", 1, 9, vec![])).unwrap();
    assert!(
        !index
            .promote(b"k", generation(1), stamp(5), copy(2, 7))
            .unwrap()
    );
}

#[test]
fn a_hint_is_returned_only_while_the_boot_epoch_of_its_node_is_the_recorded_one() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .promote(b"k", generation(1), stamp(5), copy(2, 7))
        .unwrap();
    // No epoch recorded for node 2 yet.
    assert_eq!(live_entry(&index, b"k").promoted, None);
    index.set_boot(NodeId(2), 7).unwrap();
    assert_eq!(live_entry(&index, b"k").promoted, Some(copy(2, 7)));
    assert_eq!(
        index.list(b"", None, 10).unwrap()[0].1.promoted,
        Some(copy(2, 7))
    );
    index.set_boot(NodeId(2), 8).unwrap();
    assert_eq!(live_entry(&index, b"k").promoted, None);
    assert_eq!(index.list(b"", None, 10).unwrap()[0].1.promoted, None);
    // The hint is still in the store: back to epoch 7 shows it again.
    index.set_boot(NodeId(2), 7).unwrap();
    assert_eq!(live_entry(&index, b"k").promoted, Some(copy(2, 7)));
    // Another node's epoch does not matter.
    index.set_boot(NodeId(3), 1).unwrap();
    assert_eq!(live_entry(&index, b"k").promoted, Some(copy(2, 7)));
}

#[test]
fn identical_and_conflict_answers_show_the_hint_by_the_same_epoch_rule() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .promote(b"k", generation(1), stamp(5), copy(2, 7))
        .unwrap();
    index.set_boot(NodeId(2), 7).unwrap();
    let hinted = live_entry(&index, b"k");
    assert_eq!(
        index.put_if_absent(b"k", entry(1, 9, 0, 6, 100)).unwrap(),
        tierstore_store_api::index::PutOutcome::Identical(hinted.clone())
    );
    assert_eq!(
        index.put_if_absent(b"k", entry(2, 9, 0, 6, 100)).unwrap(),
        tierstore_store_api::index::PutOutcome::Conflict(hinted)
    );
}

#[test]
fn a_relocate_keeps_the_promoted_hint_and_a_newer_record_drops_it() {
    let index = ready();
    index.set_boot(NodeId(2), 7).unwrap();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .promote(b"k", generation(1), stamp(5), copy(2, 7))
        .unwrap();
    index
        .relocate(b"k", generation(1), stamp(5), at(6, 0))
        .unwrap();
    assert_eq!(live_entry(&index, b"k").promoted, Some(copy(2, 7)));
    // The relocate record of a trailer, at the same stamp, keeps it too.
    replay(&index, vec![record(b"k", flagged(entry(1, 7, 0, 5, 100)))]);
    let e = live_entry(&index, b"k");
    assert_eq!(e.location, at(7, 0));
    assert_eq!(e.promoted, Some(copy(2, 7)));
    // A newer record of another generation replaces the entry and its hint.
    replay(&index, vec![record(b"k", entry(2, 9, 0, 11, 40))]);
    assert_eq!(live_entry(&index, b"k").promoted, None);
}

#[test]
fn a_boot_epoch_survives_a_restart() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .promote(b"k", generation(1), stamp(5), copy(2, 7))
        .unwrap();
    index.set_boot(NodeId(2), 7).unwrap();
    let restarted = KvIndex::open(index.backend().crashed(usize::MAX), config()).unwrap();
    restarted.set_ready(true);
    assert_eq!(live_entry(&restarted, b"k").promoted, Some(copy(2, 7)));
}
