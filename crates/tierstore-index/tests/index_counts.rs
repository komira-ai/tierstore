//! Per-block live counts: exact for blocks and chains, none for large files, across put, delete,
//! relocate, record replacement and block drop.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_format::ids::BlockId;
use tierstore_format::tomb::Named;
use tierstore_store_api::index::{EntryLocation, Index};

#[test]
fn entries_of_one_block_add_up_and_a_delete_takes_only_its_own() {
    let index = ready();
    index.put_if_absent(b"a", entry(1, 5, 0, 5, 100)).unwrap();
    index.put_if_absent(b"b", entry(2, 5, 200, 5, 50)).unwrap();
    index.put_if_absent(b"c", entry(3, 6, 0, 5, 7)).unwrap();
    assert_eq!(count(&index, 5), lc(2, 150));
    assert_eq!(count(&index, 6), lc(1, 7));
    index.delete(&tomb(b"a", 1, 9, vec![])).unwrap();
    assert_eq!(count(&index, 5), lc(1, 50));
    index.delete(&tomb(b"b", 2, 9, vec![])).unwrap();
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 7));
}

#[test]
fn a_chain_counts_one_entry_and_its_slice_in_each_of_its_blocks() {
    let index = ready();
    let len = 2 * MAX_OBJECT + 100;
    index
        .put_if_absent(b"k", chain_entry(1, 10, 5, len))
        .unwrap();
    assert_eq!(count(&index, 10), lc(1, MAX_OBJECT));
    assert_eq!(count(&index, 11), lc(1, MAX_OBJECT));
    assert_eq!(count(&index, 12), lc(1, 100));
    assert_eq!(count(&index, 13), lc(0, 0));
    index.delete(&tomb(b"k", 1, 9, vec![])).unwrap();
    for b in 10..=12 {
        assert_eq!(count(&index, b), lc(0, 0));
    }
    // The tombstone names every block of the chain.
    assert_eq!(
        index.tombstones().unwrap()[0].names,
        vec![
            Named::Block(block(10)),
            Named::Block(block(11)),
            Named::Block(block(12))
        ]
    );
}

#[test]
fn a_chain_relocated_to_another_chain_moves_its_counts_block_by_block() {
    let index = ready();
    let len = 2 * MAX_OBJECT + 100;
    index
        .put_if_absent(b"k", chain_entry(1, 10, 5, len))
        .unwrap();
    assert!(
        index
            .relocate(
                b"k",
                generation(1),
                stamp(5),
                EntryLocation::Chain { first: block(11) }
            )
            .unwrap()
    );
    // The new chain is 11, 12, 13: block 10 is left, block 13 is joined.
    assert_eq!(count(&index, 10), lc(0, 0));
    assert_eq!(count(&index, 11), lc(1, MAX_OBJECT));
    assert_eq!(count(&index, 12), lc(1, MAX_OBJECT));
    assert_eq!(count(&index, 13), lc(1, 100));
    assert_eq!(live_entry(&index, b"k").prior_blocks, vec![block(10)]);
}

#[test]
fn a_chain_relocated_to_a_block_counts_the_whole_length_in_that_block() {
    let index = ready();
    let len = 2 * MAX_OBJECT + 100;
    index
        .put_if_absent(b"k", chain_entry(1, 10, 5, len))
        .unwrap();
    index
        .relocate(b"k", generation(1), stamp(5), at(20, 0))
        .unwrap();
    for b in 10..=12 {
        assert_eq!(count(&index, b), lc(0, 0));
    }
    assert_eq!(count(&index, 20), lc(1, len));
    assert_eq!(
        live_entry(&index, b"k").prior_blocks,
        vec![block(10), block(11), block(12)]
    );
}

#[test]
fn an_entry_the_geometry_does_not_see_as_a_chain_counts_on_its_first_block_alone() {
    let index = ready();
    // A chain location of a size that fits one block.
    index
        .put_if_absent(b"small", chain_entry(1, 30, 5, 500))
        .unwrap();
    assert_eq!(count(&index, 30), lc(1, 500));
    assert_eq!(count(&index, 31), lc(0, 0));
    // A chain whose later block ids would pass the node's counter.
    let last = BlockId::from_raw((1 << 56) - 1);
    let mut e = chain_entry(2, 1, 5, 3 * MAX_OBJECT);
    e.location = EntryLocation::Chain { first: last };
    index.put_if_absent(b"edge", e).unwrap();
    assert_eq!(index.live(last).unwrap(), lc(1, 3 * MAX_OBJECT));
    index.delete(&tomb(b"edge", 2, 9, vec![])).unwrap();
    assert_eq!(index.live(last).unwrap(), lc(0, 0));
}

#[test]
fn a_large_file_counts_in_no_block_and_a_tombstone_names_the_file() {
    let index = ready();
    index
        .put_if_absent(b"k", large_entry(4, 5, 1 << 30))
        .unwrap();
    assert_eq!(
        index
            .backend()
            .dump()
            .iter()
            .filter(|(s, _, _)| *s == tierstore_index::backend::Space::Live)
            .count(),
        0
    );
    index.delete(&tomb(b"k", 4, 9, vec![])).unwrap();
    assert_eq!(
        index.tombstones().unwrap()[0].names,
        vec![Named::LargeFile(large_file(4, 1 << 30))]
    );
    // A large file relocated into a block starts counting there.
    index.put_if_absent(b"j", large_entry(5, 10, 4096)).unwrap();
    index
        .relocate(b"j", generation(5), stamp(10), at(6, 0))
        .unwrap();
    assert_eq!(count(&index, 6), lc(1, 4096));
    assert_eq!(live_entry(&index, b"j").prior_blocks, Vec::<BlockId>::new());
}

#[test]
fn a_newer_record_replaces_the_entry_and_moves_the_counts() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    replay(&index, vec![record(b"k", entry(2, 6, 0, 9, 40))]);
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 40));
    // An older record changes no count.
    replay(&index, vec![record(b"k", entry(3, 7, 0, 3, 11))]);
    assert_eq!(count(&index, 7), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 40));
}

#[test]
fn a_record_that_replaces_an_entry_of_the_same_block_keeps_the_count_exact() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    replay(&index, vec![record(b"k", entry(2, 5, 300, 9, 40))]);
    assert_eq!(count(&index, 5), lc(1, 40));
}

#[test]
fn a_block_drop_removes_the_count_of_the_block_and_leaves_the_others() {
    let index = ready();
    index.put_if_absent(b"a", entry(1, 5, 0, 5, 100)).unwrap();
    index.put_if_absent(b"b", entry(2, 6, 0, 5, 30)).unwrap();
    replay(&index, vec![log_drop(2, 1, 5)]);
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 30));
}

#[test]
fn a_delete_of_a_hidden_entry_after_its_block_drop_leaves_no_count_behind() {
    let index = ready();
    index.put_if_absent(b"a", entry(1, 5, 0, 5, 100)).unwrap();
    replay(&index, vec![log_drop(2, 1, 5)]);
    index.delete(&tomb(b"a", 1, 9, vec![])).unwrap();
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(
        index
            .backend()
            .dump()
            .iter()
            .filter(|(s, _, _)| *s == tierstore_index::backend::Space::Live)
            .count(),
        0
    );
}
