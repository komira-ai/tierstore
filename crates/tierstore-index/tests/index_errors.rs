//! A failing store and values that do not decode: every method answers `IndexError::Store`, and a
//! write that fails leaves the visible state as it was.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_format::ids::NodeId;
use tierstore_index::KvIndex;
use tierstore_index::backend::mem::Call;
use tierstore_index::backend::{Backend, MemBackend, Op, Space};
use tierstore_store_api::index::{Fact, HotClean, Index, IndexError, ReplayMark};

const FAIL: &str = "memory: ";

/// An index with a live block entry, a chain, a large file, a tombstone with a log position, a
/// dropped block, a promoted hint and a boot epoch.
fn prepared() -> KvIndex<MemBackend> {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 100)).unwrap();
    index
        .put_if_absent(b"ch", chain_entry(2, 10, 5, 2 * MAX_OBJECT + 100))
        .unwrap();
    index
        .put_if_absent(b"big", large_entry(3, 5, 1 << 30))
        .unwrap();
    replay(
        &index,
        vec![
            log_tomb(2, 1, tomb(b"t", 0, 4, vec![])),
            log_drop(2, 2, 20),
            record(b"hidden", entry(4, 20, 0, 5, 1)),
        ],
    );
    index.set_boot(NodeId(2), 7).unwrap();
    index
        .promote(
            b"k",
            generation(1),
            stamp(5),
            HotClean {
                node: NodeId(2),
                boot: 7,
                seq: 1,
                offset: 2,
            },
        )
        .unwrap();
    index
}

/// Runs `op` on a prepared index with the `n`-th call of each kind in `calls` failing, for every
/// `n` the operation reaches. Each failure is a store error, and when `atomic` the visible state
/// is unchanged. Returns how many failures were injected.
fn sweep<T>(
    calls: &[Call],
    atomic: bool,
    op: impl Fn(&KvIndex<MemBackend>) -> Result<T, IndexError>,
) -> usize {
    let mut injected = 0;
    for call in calls {
        for after in 0..400 {
            let index = prepared();
            let before = index.backend().dump();
            index.backend().fail_next(*call, after, 1);
            match op(&index) {
                Ok(_) => break,
                Err(IndexError::Store { reason, .. }) => {
                    assert!(reason.starts_with(FAIL), "{reason}");
                    assert!(reason.ends_with("injected failure"), "{reason}");
                    if atomic {
                        assert_eq!(index.backend().dump(), before, "{call:?} after {after}");
                    }
                    injected += 1;
                }
                Err(other) => panic!("{other:?}"),
            }
        }
    }
    injected
}

const ALL: [Call; 4] = [Call::Get, Call::Scan, Call::Commit, Call::Persist];

#[test]
fn a_failing_store_fails_put_if_absent_and_leaves_the_state_unchanged() {
    // Created over nothing, over a tombstone, identical, conflicting, and over a hidden entry.
    assert!(sweep(&ALL, true, |i| i.put_if_absent(b"n", entry(5, 5, 8, 6, 3))) >= 3);
    assert!(sweep(&ALL, true, |i| i.put_if_absent(b"t", entry(5, 6, 0, 9, 3))) >= 3);
    assert!(sweep(&ALL, true, |i| i.put_if_absent(b"k", entry(1, 6, 0, 9, 3))) >= 1);
    assert!(sweep(&ALL, true, |i| i.put_if_absent(b"k", entry(2, 6, 0, 9, 3))) >= 1);
    assert!(
        sweep(&ALL, true, |i| i
            .put_if_absent(b"hidden", entry(6, 6, 0, 9, 3)))
            >= 3
    );
    assert!(
        sweep(&ALL, true, |i| i.put_if_absent(
            b"cn",
            chain_entry(7, 30, 6, 2 * MAX_OBJECT + 1)
        )) >= 3
    );
}

#[test]
fn a_failing_store_fails_delete_and_leaves_the_state_unchanged() {
    assert!(sweep(&ALL, true, |i| i.delete(&tomb(b"k", 1, 9, vec![]))) >= 4);
    assert!(sweep(&ALL, true, |i| i.delete(&tomb(b"ch", 2, 9, vec![]))) >= 6);
    assert!(sweep(&ALL, true, |i| i.delete(&tomb(b"nope", 0, 9, vec![]))) >= 2);
    assert!(sweep(&ALL, true, |i| i.delete(&tomb(b"t", 0, 8, vec![]))) >= 2);
    assert!(sweep(&ALL, true, |i| i.delete(&tomb(b"k", 1, 2, vec![]))) >= 2);
}

#[test]
fn a_failing_store_fails_relocate_and_promote_and_leaves_the_state_unchanged() {
    assert!(
        sweep(&ALL, true, |i| i.relocate(
            b"k",
            generation(1),
            stamp(5),
            at(6, 0)
        )) >= 4
    );
    assert!(
        sweep(&ALL, true, |i| i.relocate(
            b"ch",
            generation(2),
            stamp(5),
            at(6, 0)
        )) >= 4
    );
    assert!(
        sweep(&ALL, true, |i| i.relocate(
            b"nope",
            generation(1),
            stamp(5),
            at(6, 0)
        )) >= 1
    );
    let copy = HotClean {
        node: NodeId(3),
        boot: 1,
        seq: 1,
        offset: 1,
    };
    assert!(
        sweep(&ALL, true, |i| i.promote(
            b"k",
            generation(1),
            stamp(5),
            copy
        )) >= 2
    );
    assert!(
        sweep(&ALL, true, |i| i.promote(
            b"nope",
            generation(1),
            stamp(5),
            copy
        )) >= 1
    );
}

#[test]
fn a_failing_store_fails_replay_of_each_kind_of_fact_and_leaves_the_state_unchanged() {
    let one = |f: Fact| move |i: &KvIndex<MemBackend>| i.replay(&mut vec![f.clone()].into_iter());
    assert!(sweep(&ALL, true, one(record(b"k", entry(2, 6, 0, 9, 3)))) >= 4);
    assert!(sweep(&ALL, true, one(record(b"t", entry(2, 6, 0, 2, 3)))) >= 3);
    assert!(
        sweep(
            &ALL,
            true,
            one(record(b"new", chain_entry(2, 40, 9, 2 * MAX_OBJECT + 1)))
        ) >= 3
    );
    assert!(sweep(&ALL, true, one(relocate(b"k", 1, 5, at(6, 0)))) >= 4);
    assert!(sweep(&ALL, true, one(log_tomb(2, 3, tomb(b"k", 1, 9, vec![])))) >= 4);
    assert!(sweep(&ALL, true, one(log_tomb(3, 1, tomb(b"k", 1, 9, vec![])))) >= 4);
    assert!(sweep(&ALL, true, one(log_drop(2, 3, 21))) >= 1);
    assert!(sweep(&ALL, true, one(log_report(3, 1, 2, 1))) >= 1);
}

#[test]
fn a_failing_store_fails_the_reads_of_every_method() {
    assert!(sweep(&ALL, true, |i| i.get(b"k")) >= 1);
    assert!(sweep(&ALL, true, |i| i.get(b"ch")) >= 1);
    assert!(sweep(&ALL, true, |i| i.list(b"", None, 100)) >= 1);
    assert!(sweep(&ALL, true, |i| i.live(block(5))) >= 1);
    assert!(sweep(&ALL, true, |i| i.replay_mark()) >= 1);
    assert!(sweep(&ALL, true, |i| i.tomb_cursor(NodeId(2))) >= 1);
    assert!(sweep(&ALL, true, |i| i.tomb_cursors()) >= 1);
    assert!(sweep(&ALL, true, |i| i.set_boot(NodeId(9), 1)) >= 1);
}

#[test]
fn a_failing_commit_of_flush_mark_is_an_error_and_the_mark_is_not_recorded() {
    let mark = ReplayMark {
        open_segments: vec![(block(5), 7)],
        tomb_cursors: vec![],
    };
    assert!(sweep(&[Call::Commit], true, |i| i.flush_mark(&mark)) >= 1);
    let index = prepared();
    index.backend().fail_next(Call::Commit, 0, 1);
    assert!(index.flush_mark(&mark).is_err());
    assert_eq!(index.replay_mark().unwrap(), None);
}

#[test]
fn a_failing_persist_of_flush_mark_is_an_error() {
    let index = prepared();
    index.backend().fail_next(Call::Persist, 0, 1);
    let err = index.flush_mark(&ReplayMark::default()).unwrap_err();
    assert!(matches!(err, IndexError::Store { op: "persist", .. }));
    // The next flush succeeds and makes everything durable.
    index.flush_mark(&ReplayMark::default()).unwrap();
    assert_eq!(index.backend().unpersisted(), 0);
}

#[test]
fn a_failed_boot_epoch_write_leaves_the_hint_visible_by_the_old_epoch() {
    let index = prepared();
    assert!(live_entry(&index, b"k").promoted.is_some());
    index.backend().fail_next(Call::Commit, 0, 1);
    assert!(index.set_boot(NodeId(2), 8).is_err());
    assert!(live_entry(&index, b"k").promoted.is_some());
}

#[test]
fn a_failed_batch_of_a_long_replay_keeps_the_batches_before_it() {
    let index = ready();
    let facts: Vec<Fact> = (0..1500)
        .map(|i| record(format!("k{i:05}").as_bytes(), entry(1, 5, i, 5, 1)))
        .collect();
    // The first commit is the version stamp's, the second is the first batch, the third the second.
    index.backend().fail_next(Call::Commit, 1, 1);
    assert!(index.replay(&mut facts.clone().into_iter()).is_err());
    assert_eq!(count(&index, 5), lc(1024, 1024));
    // Replaying again finishes the job: the first batch is already applied.
    let stats = replay(&index, facts);
    assert_eq!((stats.facts, stats.applied), (1500, 476));
    assert_eq!(count(&index, 5), lc(1500, 1500));
}

fn corrupt(index: &KvIndex<MemBackend>, space: Space, key: &[u8]) {
    index
        .backend()
        .commit(vec![Op::Put {
            space,
            key: key.to_vec(),
            value: vec![0xff],
        }])
        .unwrap();
}

fn store_error<T: std::fmt::Debug>(r: Result<T, IndexError>) -> (&'static str, String) {
    match r {
        Err(IndexError::Store { op, reason }) => (op, reason),
        other => panic!("expected a store error, found {other:?}"),
    }
}

#[test]
fn an_entry_that_does_not_decode_is_a_store_error_for_every_method_that_reads_it() {
    let bad = ("decode entry", "unknown value version".to_owned());
    let index = prepared();
    corrupt(&index, Space::Entries, b"k");
    assert_eq!(store_error(index.get(b"k")), bad);
    assert_eq!(store_error(index.list(b"", None, 10)), bad);
    assert_eq!(
        store_error(index.put_if_absent(b"k", entry(1, 5, 0, 5, 1))),
        bad
    );
    assert_eq!(store_error(index.delete(&tomb(b"k", 1, 9, vec![]))), bad);
    assert_eq!(
        store_error(index.relocate(b"k", generation(1), stamp(5), at(6, 0))),
        bad
    );
    assert_eq!(
        store_error(index.promote(
            b"k",
            generation(1),
            stamp(5),
            HotClean {
                node: NodeId(2),
                boot: 7,
                seq: 1,
                offset: 1
            }
        )),
        bad
    );
    assert_eq!(
        store_error(index.replay(&mut vec![record(b"k", entry(2, 6, 0, 9, 3))].into_iter())),
        bad
    );
}

#[test]
fn a_live_count_that_does_not_decode_is_a_store_error_where_the_count_is_read() {
    let bad = ("decode live count", "unknown value version".to_owned());
    let index = prepared();
    corrupt(&index, Space::Live, &block(5).raw().to_be_bytes());
    assert_eq!(store_error(index.live(block(5))), bad);
    assert_eq!(
        store_error(index.put_if_absent(b"n", entry(2, 5, 8, 6, 3))),
        bad
    );
    assert_eq!(store_error(index.delete(&tomb(b"k", 1, 9, vec![]))), bad);
    assert_eq!(
        store_error(index.relocate(b"k", generation(1), stamp(5), at(6, 0))),
        bad
    );
}

#[test]
fn a_cursor_or_mark_that_does_not_decode_is_a_store_error() {
    let index = prepared();
    corrupt(&index, Space::Meta, b"cursor/\x02");
    assert_eq!(
        store_error(index.tomb_cursor(NodeId(2))),
        ("decode tomb cursor", "unknown value version".to_owned())
    );
    assert_eq!(
        store_error(index.tomb_cursors()),
        ("decode tomb cursor", "unknown value version".to_owned())
    );
    assert_eq!(
        store_error(index.replay(&mut vec![log_tomb(2, 3, tomb(b"x", 0, 4, vec![]))].into_iter())),
        ("decode tomb cursor", "unknown value version".to_owned())
    );
    let other = prepared();
    corrupt(&other, Space::Meta, b"mark");
    assert_eq!(
        store_error(other.replay_mark()),
        ("decode replay mark", "unknown value version".to_owned())
    );
}

#[test]
fn a_mark_with_trailing_bytes_or_a_cut_value_is_a_store_error() {
    let index = prepared();
    // Version 1, one open segment announced and none present.
    index
        .backend()
        .commit(vec![Op::Put {
            space: Space::Meta,
            key: b"mark".to_vec(),
            value: vec![1, 1, 0, 0, 0],
        }])
        .unwrap();
    assert_eq!(
        store_error(index.replay_mark()),
        ("decode replay mark", "value is truncated".to_owned())
    );
}

#[test]
fn a_failing_scan_of_the_cursors_fails_replay_mark_after_a_flush() {
    let index = prepared();
    index.flush_mark(&ReplayMark::default()).unwrap();
    index.backend().fail_next(Call::Scan, 0, 1);
    assert!(matches!(index.replay_mark(), Err(IndexError::Store { .. })));
    assert_eq!(
        index.replay_mark().unwrap().unwrap().tomb_cursors,
        vec![(NodeId(2), 2)]
    );
}
