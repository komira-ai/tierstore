//! Replay: the newest fact wins in every order, the tomb log's cursors and entries, block drops,
//! the replay mark, and the stamp rows of the liveness rule at index level.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::tomb::{LogEntry, Named};
use tierstore_index::KvIndex;
use tierstore_index::backend::MemBackend;
use tierstore_index::blocks::BlockState;
use tierstore_store_api::index::{
    DeleteOutcome, Fact, Index, IndexEntry, IndexError, Lookup, PutOutcome, ReplayMark,
};

/// What the index holds for `key` and the counts of blocks 5 to 9, for comparing orders.
fn state_of(index: &KvIndex<MemBackend>, key: &[u8]) -> (Lookup, Vec<(u64, u64)>) {
    let counts = (5..=9)
        .map(|b| {
            let c = count(index, b);
            (c.entries, c.bytes)
        })
        .collect();
    (index.get(key).unwrap(), counts)
}

fn run_in_order(facts: &[Fact]) -> KvIndex<MemBackend> {
    let index = ready();
    replay(&index, facts.to_vec());
    index
}

#[test]
fn replay_stats_count_every_fact_and_those_that_changed_an_entry() {
    let index = ready();
    let e1 = entry(1, 5, 0, 5, 100);
    let stats = replay(
        &index,
        vec![
            record(b"k", e1.clone()),
            record(b"k", e1.clone()),
            relocate(b"k", 1, 5, at(6, 0)),
            relocate(b"k", 1, 5, at(6, 0)),
            // The record the relocate moved, seen again after it, and an older other generation.
            record(b"k", e1),
            record(b"k", entry(2, 9, 0, 3, 7)),
            log(2, 1, LogEntry::Unknown { kind: 200, body: vec![1] }),
        ],
    );
    assert_eq!((stats.facts, stats.applied), (7, 2));
    let e = live_entry(&index, b"k");
    assert_eq!(e.location, at(6, 0));
    assert_eq!(e.prior_blocks, vec![block(5)]);
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 100));
    assert_eq!(count(&index, 9), lc(0, 0));
}

#[test]
fn an_empty_replay_changes_nothing() {
    let index = ready();
    let before = index.backend().dump();
    let stats = replay(&index, vec![]);
    assert_eq!((stats.facts, stats.applied), (0, 0));
    assert_eq!(index.backend().dump(), before);
}

#[test]
fn record_relocate_and_tombstone_of_one_key_give_one_result_in_every_order() {
    let r = record(b"k", entry(1, 5, 0, 5, 100));
    let f = record(b"k", flagged(entry(1, 6, 64, 5, 100)));
    let t = log_tomb(2, 1, tomb(b"k", 1, 9, vec![]));
    let mut results = Vec::new();
    for order in permutations(&[r, f, t]) {
        let index = run_in_order(&order);
        let names = index.tombstones().unwrap()[0].names.clone();
        results.push((state_of(&index, b"k"), names));
    }
    assert_eq!(results.len(), 6);
    for got in &results {
        assert_eq!(got.0 .0, Lookup::Deleted(stamp(9)));
        assert_eq!(got.0 .1, vec![(0, 0); 5]);
        assert_eq!(got.1, vec![Named::Block(block(5)), Named::Block(block(6))]);
    }
}

#[test]
fn a_record_its_relocate_records_and_no_tombstone_give_one_entry_in_every_order() {
    let r = record(b"k", entry(1, 5, 0, 5, 100));
    let f = record(b"k", flagged(entry(1, 6, 0, 5, 100)));
    let g = record(b"k", flagged(entry(1, 7, 0, 5, 100)));
    let mut finals: Vec<IndexEntry> = Vec::new();
    for order in permutations(&[r, f, g]) {
        let index = run_in_order(&order);
        let (lookup, counts) = state_of(&index, b"k");
        assert_eq!(counts, vec![(0, 0), (0, 0), (1, 100), (0, 0), (0, 0)]);
        let Lookup::Live(e) = lookup else {
            panic!("the key is not live");
        };
        finals.push(e);
    }
    assert_eq!(finals.len(), 6);
    assert_eq!(finals[0].location, at(7, 0));
    assert_eq!(finals[0].prior_blocks, vec![block(5), block(6)]);
    assert!(finals.iter().all(|e| *e == finals[0]));
}

#[test]
fn a_relocate_fact_and_the_relocate_record_make_the_same_entry() {
    let a = ready();
    replay(
        &a,
        vec![
            record(b"k", entry(1, 5, 0, 5, 100)),
            relocate(b"k", 1, 5, at(6, 0)),
        ],
    );
    let b = ready();
    replay(
        &b,
        vec![
            record(b"k", entry(1, 5, 0, 5, 100)),
            record(b"k", flagged(entry(1, 6, 0, 5, 100))),
        ],
    );
    assert_eq!(live_entry(&a, b"k"), live_entry(&b, b"k"));
}

#[test]
fn a_record_older_than_the_held_one_adds_its_block_to_the_prior_blocks_once() {
    let index = ready();
    replay(&index, vec![record(b"k", flagged(entry(1, 6, 0, 5, 100)))]);
    let stats = replay(&index, vec![record(b"k", entry(1, 5, 0, 5, 100))]);
    assert_eq!(stats.applied, 1);
    assert_eq!(live_entry(&index, b"k").prior_blocks, vec![block(5)]);
    // The block of the held entry itself is not a prior block.
    let stats = replay(&index, vec![record(b"k", entry(1, 6, 8, 5, 100))]);
    assert_eq!(stats.applied, 0);
    assert_eq!(live_entry(&index, b"k").prior_blocks, vec![block(5)]);
    assert_eq!(live_entry(&index, b"k").location, at(6, 0));
}

#[test]
fn records_of_equal_stamp_and_different_generations_resolve_alike_in_every_order() {
    let facts = [
        record(b"k", entry(1, 5, 0, 5, 10)),
        record(b"k", entry(2, 6, 0, 5, 20)),
        record(b"k", entry(3, 7, 0, 5, 30)),
    ];
    for order in permutations(&facts) {
        let index = run_in_order(&order);
        let (lookup, counts) = state_of(&index, b"k");
        assert_eq!(lookup, Lookup::Live(entry(3, 7, 0, 5, 30)));
        assert_eq!(counts, vec![(0, 0), (0, 0), (1, 30), (0, 0), (0, 0)]);
    }
}

#[test]
fn a_relocate_flagged_record_wins_at_equal_stamp_over_a_larger_generation() {
    let facts = [
        record(b"k", entry(3, 7, 0, 5, 30)),
        record(b"k", flagged(entry(1, 5, 0, 5, 10))),
    ];
    for order in permutations(&facts) {
        let index = run_in_order(&order);
        assert_eq!(
            live_entry(&index, b"k"),
            flagged(entry(1, 5, 0, 5, 10))
        );
        assert_eq!(count(&index, 7), lc(0, 0));
        assert_eq!(count(&index, 5), lc(1, 10));
    }
}

#[test]
fn equal_stamp_and_generation_resolve_by_the_larger_location_in_every_order() {
    let facts = [
        record(b"k", entry(1, 5, 0, 5, 10)),
        record(b"k", entry(1, 6, 0, 5, 10)),
    ];
    let a = run_in_order(&facts);
    let b = run_in_order(&[facts[1].clone(), facts[0].clone()]);
    assert_eq!(live_entry(&a, b"k").location, live_entry(&b, b"k").location);
}

#[test]
fn a_newer_record_replaces_the_entry_in_every_order_of_two_generations() {
    let facts = [
        record(b"k", entry(1, 5, 0, 5, 10)),
        record(b"k", entry(2, 6, 0, 8, 20)),
    ];
    for order in permutations(&facts) {
        let index = run_in_order(&order);
        assert_eq!(live_entry(&index, b"k"), entry(2, 6, 0, 8, 20));
        assert_eq!(count(&index, 5), lc(0, 0));
        assert_eq!(count(&index, 6), lc(1, 20));
    }
}

#[test]
fn a_tombstone_applied_before_the_record_it_deletes_extends_to_name_the_record_block() {
    let index = ready();
    replay(&index, vec![log_tomb(2, 1, tomb(b"k", 1, 9, vec![]))]);
    assert!(index.tombstones().unwrap()[0].names.is_empty());
    let stats = replay(&index, vec![record(b"k", entry(1, 5, 0, 5, 10))]);
    // The record is dead on arrival: nothing changes in the entry, the tombstone learns its block.
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(9)));
    assert_eq!(stats.applied, 0);
    assert_eq!(
        index.tombstones().unwrap()[0].names,
        vec![Named::Block(block(5))]
    );
    assert_eq!(count(&index, 5), lc(0, 0));
}

#[test]
fn stamp_row_a_a_tombstone_from_another_node_deletes_the_record_in_any_order() {
    // Node A holds K's record; node B deleted K with a stamp above and A applies B's log entry.
    let r = record(b"k", entry(1, 5, 0, 5, 10));
    let again = record(b"k", entry(1, 5, 0, 5, 10));
    let t = log_tomb(2, 1, tomb(b"k", 1, 9, vec![Named::Block(block(5))]));
    for order in permutations(&[r, again, t]) {
        let index = run_in_order(&order);
        assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(9)));
        assert_eq!(count(&index, 5), lc(0, 0));
        assert!(index.list(b"", None, 10).unwrap().is_empty());
    }
}

#[test]
fn stamp_row_b_delete_then_reput_of_the_same_generation_is_live_at_the_new_record() {
    let index = ready();
    let old = entry(1, 5, 0, 5, 10);
    index.put_if_absent(b"k", old.clone()).unwrap();
    assert!(matches!(
        index.delete(&tomb(b"k", 1, 7, vec![])).unwrap(),
        DeleteOutcome::Deleted(_)
    ));
    let new = entry(1, 6, 0, 8, 10);
    assert_eq!(index.put_if_absent(b"k", new.clone()).unwrap(), PutOutcome::Created);
    assert_eq!(index.get(b"k").unwrap(), Lookup::Live(new.clone()));
    let before = index.backend().dump();
    // The old record seen again (a recovery scan of its block) changes nothing.
    let stats = replay(&index, vec![record(b"k", old)]);
    assert_eq!(stats.applied, 0);
    assert_eq!(index.backend().dump(), before);
    assert_eq!(index.get(b"k").unwrap(), Lookup::Live(new));
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(count(&index, 6), lc(1, 10));
}

#[test]
fn a_tomb_log_cursor_moves_with_each_entry_and_a_sequence_at_or_below_it_is_skipped() {
    let index = ready();
    let t = |s| tomb(b"k", 0, s, vec![]);
    // Cursor 0 takes any first sequence (a log whose head was compacted).
    let stats = replay(&index, vec![log_tomb(2, 5, t(4))]);
    assert_eq!((stats.facts, stats.applied), (1, 1));
    assert_eq!(index.tomb_cursor(NodeId(2)).unwrap(), 5);
    assert_eq!(index.tomb_cursor(NodeId(3)).unwrap(), 0);
    let stats = replay(&index, vec![log_tomb(2, 6, t(6)), log_tomb(2, 6, t(8)), log_tomb(2, 3, t(9))]);
    assert_eq!((stats.facts, stats.applied), (3, 1));
    assert_eq!(index.tomb_cursor(NodeId(2)).unwrap(), 6);
    // The skipped entries did not apply.
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(6)));
    replay(&index, vec![log_tomb(7, 1, t(2))]);
    assert_eq!(
        index.tomb_cursors().unwrap(),
        vec![(NodeId(2), 6), (NodeId(7), 1)]
    );
}

#[test]
fn a_gap_in_a_tomb_log_is_an_error_naming_the_origin_and_the_batch_is_not_applied() {
    let index = ready();
    replay(&index, vec![log_tomb(2, 1, tomb(b"a", 0, 4, vec![]))]);
    let err = index
        .replay(&mut vec![log_tomb(2, 2, tomb(b"b", 0, 4, vec![])), log_tomb(2, 4, tomb(b"c", 0, 4, vec![]))].into_iter())
        .unwrap_err();
    let IndexError::Store { op, reason } = err else {
        panic!("not a store error");
    };
    assert_eq!(op, "replay tomb log");
    assert_eq!(reason, "origin 2 jumps from sequence 2 to 4");
    // The batch is one commit: neither entry of it is in the index.
    assert_eq!(index.tomb_cursor(NodeId(2)).unwrap(), 1);
    assert_eq!(index.get(b"b").unwrap(), Lookup::Absent);
}

#[test]
fn an_entry_of_an_unknown_kind_moves_the_cursor_and_nothing_else() {
    let index = ready();
    let before: Vec<_> = index
        .backend()
        .dump()
        .into_iter()
        .filter(|(s, _, _)| *s != tierstore_index::backend::Space::Meta)
        .collect();
    let stats = replay(
        &index,
        vec![log(2, 1, LogEntry::Unknown { kind: 77, body: vec![1, 2] })],
    );
    assert_eq!((stats.facts, stats.applied), (1, 0));
    assert_eq!(index.tomb_cursor(NodeId(2)).unwrap(), 1);
    let after: Vec<_> = index
        .backend()
        .dump()
        .into_iter()
        .filter(|(s, _, _)| *s != tierstore_index::backend::Space::Meta)
        .collect();
    assert_eq!(before, after);
}

#[test]
fn a_block_drop_hides_the_keys_of_the_block_and_a_later_put_creates() {
    let index = ready();
    index.put_if_absent(b"a", entry(1, 5, 0, 5, 10)).unwrap();
    index.put_if_absent(b"b", entry(1, 5, 64, 5, 10)).unwrap();
    index.put_if_absent(b"c", entry(1, 6, 0, 5, 10)).unwrap();
    index.put_if_absent(b"d", large_entry(2, 5, 4096)).unwrap();
    let len = 2 * MAX_OBJECT + 1;
    index.put_if_absent(b"e", chain_entry(3, 7, 5, len)).unwrap();
    let stats = replay(&index, vec![log_drop(2, 1, 5), log_drop(2, 2, 5), log_drop(2, 3, 8)]);
    assert_eq!((stats.facts, stats.applied), (3, 2));
    assert_eq!(index.get(b"a").unwrap(), Lookup::Absent);
    assert_eq!(index.get(b"b").unwrap(), Lookup::Absent);
    assert!(matches!(index.get(b"c").unwrap(), Lookup::Live(_)));
    assert!(matches!(index.get(b"d").unwrap(), Lookup::Live(_)));
    // The chain spans blocks 7, 8 and 9: the drop of block 8 hides it.
    assert_eq!(index.get(b"e").unwrap(), Lookup::Absent);
    let listed: Vec<Vec<u8>> = index
        .list(b"", None, 10)
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(listed, vec![b"c".to_vec(), b"d".to_vec()]);
    assert_eq!(count(&index, 5), lc(0, 0));
    assert_eq!(index.block(block(5)).unwrap().unwrap().state, BlockState::Dropped);
    assert!(index.is_freed(&Named::Block(block(5))).unwrap());
    // A later put of a hidden key is a create, not an identical or a conflict.
    assert_eq!(
        index.put_if_absent(b"a", entry(9, 20, 0, 6, 3)).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(live_entry(&index, b"a").generation, generation(9));
    // The drop is in the table, so a restart still hides the keys.
    let again = KvIndex::open(index.backend().crashed(usize::MAX), config()).unwrap();
    again.set_ready(true);
    assert_eq!(again.get(b"b").unwrap(), Lookup::Absent);
    assert!(matches!(again.get(b"a").unwrap(), Lookup::Live(_)));
}

#[test]
fn a_block_drop_in_a_replay_longer_than_a_batch_hides_the_keys_after_the_replay() {
    let index = ready();
    let mut facts: Vec<Fact> = (0..1500)
        .map(|i| record(format!("k{i:05}").as_bytes(), entry(1, 5, i, 5, 1)))
        .collect();
    facts.push(log_drop(2, 1, 5));
    let stats = replay(&index, facts);
    assert_eq!((stats.facts, stats.applied), (1501, 1501));
    assert_eq!(index.get(b"k00007").unwrap(), Lookup::Absent);
    assert!(index.list(b"", None, 5000).unwrap().is_empty());
}

#[test]
fn a_tomb_drop_records_its_reporter_once() {
    let index = ready();
    replay(&index, vec![log_tomb(2, 1, tomb(b"k", 0, 4, vec![]))]);
    let stats = replay(
        &index,
        vec![
            log_report(3, 1, 2, 1),
            log_report(3, 2, 2, 1),
            log_report(4, 1, 2, 1),
        ],
    );
    assert_eq!((stats.facts, stats.applied), (3, 2));
    let tombs = index.tombstones().unwrap();
    assert_eq!(tombs[0].position, Some((NodeId(2), 1)));
    assert_eq!(tombs[0].reports, vec![NodeId(3), NodeId(4)]);
}

#[test]
fn replay_batches_of_more_than_a_thousand_facts_apply_every_fact_once() {
    for n in [1023usize, 1024, 1025, 2048, 2500] {
        let index = ready();
        let facts: Vec<Fact> = (0..n)
            .map(|i| record(format!("k{i:05}").as_bytes(), entry(1, 5, i as u64, 5, 2)))
            .collect();
        let stats = replay(&index, facts);
        assert_eq!((stats.facts, stats.applied), (n as u64, n as u64));
        assert_eq!(count(&index, 5), lc(n as u64, 2 * n as u64));
    }
}

#[test]
fn flush_mark_makes_every_batch_durable_and_replay_mark_returns_it() {
    let index = ready();
    assert_eq!(index.replay_mark().unwrap(), None);
    replay(&index, vec![record(b"k", entry(1, 5, 0, 5, 10))]);
    assert!(index.backend().unpersisted() > 0);
    let mark = ReplayMark {
        open_segments: vec![(block(5), 4096), (block(6), 12)],
        tomb_cursors: vec![],
    };
    index.flush_mark(&mark).unwrap();
    assert_eq!(index.backend().unpersisted(), 0);
    assert_eq!(index.replay_mark().unwrap(), Some(mark.clone()));
    // What a crash keeps is the flushed state.
    let restarted = KvIndex::open(index.backend().crashed(0), config()).unwrap();
    assert_eq!(restarted.replay_mark().unwrap(), Some(mark));
    restarted.set_ready(true);
    assert!(matches!(restarted.get(b"k").unwrap(), Lookup::Live(_)));
}

#[test]
fn replay_mark_cursors_are_the_larger_of_the_marks_and_the_stores_own() {
    let index = ready();
    replay(
        &index,
        vec![
            log_tomb(2, 1, tomb(b"a", 0, 4, vec![])),
            log_tomb(2, 2, tomb(b"a", 0, 5, vec![])),
            log_tomb(2, 3, tomb(b"a", 0, 6, vec![])),
            log_tomb(3, 1, tomb(b"b", 0, 4, vec![])),
        ],
    );
    let mark = ReplayMark {
        open_segments: vec![],
        tomb_cursors: vec![(NodeId(2), 2), (NodeId(3), 9), (NodeId(4), 5)],
    };
    index.flush_mark(&mark).unwrap();
    assert_eq!(
        index.replay_mark().unwrap().unwrap().tomb_cursors,
        vec![(NodeId(2), 3), (NodeId(3), 9), (NodeId(4), 5)]
    );
}

#[test]
fn live_counts_are_kept_per_block_id_of_any_node() {
    let index = ready();
    let other = BlockId::new(NodeId(4), 1).unwrap();
    let mut e = entry(1, 5, 0, 5, 10);
    e.location = tierstore_store_api::index::EntryLocation::Block { block: other, offset: 0 };
    replay(&index, vec![record(b"k", e)]);
    assert_eq!(index.live(other).unwrap(), lc(1, 10));
    assert_eq!(index.live(block(5)).unwrap(), lc(0, 0));
}
