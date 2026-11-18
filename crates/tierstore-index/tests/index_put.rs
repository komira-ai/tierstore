//! Create-only `put_if_absent`, the `indexing` state, and the refusal of a store of another
//! layout version, on the in-memory store.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_format::tomb::Named;
use tierstore_index::backend::{Backend, MemBackend, Op, Space};
use tierstore_index::{KvIndex, STORE_VERSION};
use tierstore_store_api::index::{
    DeleteOutcome, Index, IndexError, Lookup, PutOutcome, ReplayMark,
};

#[test]
fn put_on_an_empty_key_creates_and_the_entry_is_read_back() {
    let index = ready();
    let e = entry(1, 5, 64, 10, 100);
    assert_eq!(
        index.put_if_absent(b"k", e.clone()).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Live(e.clone()));
    assert_eq!(count(&index, 5), lc(1, 100));
    assert_eq!(index.list(b"", None, 10).unwrap(), vec![(b"k".to_vec(), e)]);
}

#[test]
fn put_of_the_same_generation_is_identical_at_any_stamp_and_changes_nothing() {
    let index = ready();
    let first = entry(1, 5, 64, 10, 100);
    index.put_if_absent(b"k", first.clone()).unwrap();
    let before = index.backend().dump();
    for (s, b) in [(10, 5), (3, 6), (99, 7)] {
        assert_eq!(
            index.put_if_absent(b"k", entry(1, b, 0, s, 100)).unwrap(),
            PutOutcome::Identical(first.clone())
        );
    }
    assert_eq!(index.backend().dump(), before);
    assert_eq!(count(&index, 5), lc(1, 100));
    assert_eq!(count(&index, 6), lc(0, 0));
}

#[test]
fn put_of_another_generation_conflicts_and_keeps_the_held_entry() {
    let index = ready();
    let first = entry(1, 5, 64, 10, 100);
    index.put_if_absent(b"k", first.clone()).unwrap();
    assert_eq!(
        index.put_if_absent(b"k", entry(2, 6, 0, 50, 7)).unwrap(),
        PutOutcome::Conflict(first.clone())
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Live(first));
    assert_eq!(count(&index, 6), lc(0, 0));
}

#[test]
fn put_below_a_tombstone_is_stale_and_at_or_above_it_creates() {
    let index = ready();
    index.delete(&tomb(b"k", 0, 10, vec![])).unwrap();
    assert_eq!(
        index.put_if_absent(b"k", entry(1, 5, 0, 9, 4)).unwrap(),
        PutOutcome::Stale(stamp(10))
    );
    assert_eq!(index.get(b"k").unwrap(), Lookup::Deleted(stamp(10)));
    assert_eq!(count(&index, 5), lc(0, 0));
    // A record at the tombstone's own stamp is live (the rule: no tombstone above it).
    assert_eq!(
        index.put_if_absent(b"k", entry(1, 5, 0, 10, 4)).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(count(&index, 5), lc(1, 4));

    let other = ready();
    other.delete(&tomb(b"j", 0, 10, vec![])).unwrap();
    assert_eq!(
        other.put_if_absent(b"j", entry(2, 6, 0, 11, 8)).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(live_entry(&other, b"j").stamp, stamp(11));
}

#[test]
fn a_put_over_a_tombstone_keeps_the_tombstone_row_for_its_drop() {
    let index = ready();
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 10)).unwrap();
    index.delete(&tomb(b"k", 1, 7, vec![])).unwrap();
    index.put_if_absent(b"k", entry(1, 6, 0, 8, 10)).unwrap();
    let tombs = index.tombstones().unwrap();
    assert_eq!(tombs.len(), 1);
    assert_eq!(tombs[0].stamp, stamp(7));
    assert_eq!(tombs[0].names, vec![Named::Block(block(5))]);
}

#[test]
fn get_list_and_put_answer_indexing_until_the_index_is_set_ready() {
    let index = indexing();
    assert!(!index.is_ready());
    assert!(matches!(index.get(b"k"), Err(IndexError::Indexing)));
    assert!(matches!(
        index.list(b"", None, 1),
        Err(IndexError::Indexing)
    ));
    assert!(matches!(
        index.put_if_absent(b"k", entry(1, 5, 0, 5, 10)),
        Err(IndexError::Indexing)
    ));

    // Everything recovery needs works while indexing.
    let stats = replay(&index, vec![record(b"k", entry(1, 5, 0, 5, 10))]);
    assert_eq!((stats.facts, stats.applied), (1, 1));
    assert!(matches!(
        index.delete(&tomb(b"j", 0, 3, vec![])).unwrap(),
        DeleteOutcome::NoLiveEntry
    ));
    index.flush_mark(&ReplayMark::default()).unwrap();
    assert_eq!(index.replay_mark().unwrap(), Some(ReplayMark::default()));
    assert_eq!(count(&index, 5), lc(1, 10));

    index.set_ready(true);
    assert!(index.is_ready());
    assert_eq!(
        index.get(b"k").unwrap(),
        Lookup::Live(entry(1, 5, 0, 5, 10))
    );
    assert_eq!(index.get(b"j").unwrap(), Lookup::Deleted(stamp(3)));
    assert_eq!(index.get(b"absent").unwrap(), Lookup::Absent);

    index.set_ready(false);
    assert!(matches!(index.get(b"k"), Err(IndexError::Indexing)));
}

#[test]
fn a_new_store_is_stamped_with_the_version_and_reopened() {
    let backend = MemBackend::new();
    let index = KvIndex::open(backend, config()).unwrap();
    index.set_ready(true);
    index.put_if_absent(b"k", entry(1, 5, 0, 5, 10)).unwrap();
    assert_eq!(index.config(), config());
    assert!(format!("{index:?}").contains("ready: true"));
    let again = KvIndex::open(index.backend().crashed(usize::MAX), config()).unwrap();
    again.set_ready(true);
    assert_eq!(
        again.get(b"k").unwrap(),
        Lookup::Live(entry(1, 5, 0, 5, 10))
    );
}

fn version_bytes(version: u64) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&version.to_le_bytes());
    v
}

#[test]
fn open_refuses_a_store_of_another_layout_version() {
    let backend = MemBackend::new();
    backend
        .commit(vec![Op::Put {
            space: Space::Meta,
            key: b"version".to_vec(),
            value: version_bytes(STORE_VERSION + 1),
        }])
        .unwrap();
    let err = KvIndex::open(backend, config()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "index store: open: the store is of layout version 2"
    );
}

#[test]
fn open_refuses_a_version_value_that_does_not_decode() {
    let backend = MemBackend::new();
    backend
        .commit(vec![Op::Put {
            space: Space::Meta,
            key: b"version".to_vec(),
            value: vec![9],
        }])
        .unwrap();
    let err = KvIndex::open(backend, config()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "index store: decode store version: unknown value version"
    );
}

#[test]
fn open_refuses_a_boot_epoch_and_a_block_row_that_do_not_decode() {
    for (space, key) in [
        (Space::Meta, b"boot/\x02".to_vec()),
        (Space::Blocks, vec![0; 8]),
    ] {
        let backend = MemBackend::new();
        backend
            .commit(vec![Op::Put {
                space,
                key,
                value: vec![0xff],
            }])
            .unwrap();
        assert!(matches!(
            KvIndex::open(backend, config()),
            Err(IndexError::Store { .. })
        ));
    }
}

#[test]
fn open_reports_a_failing_store_at_each_of_its_reads_and_its_stamping_write() {
    use tierstore_index::backend::mem::Call;
    for call in [Call::Get, Call::Commit] {
        let backend = MemBackend::new();
        backend.fail_next(call, 0, 1);
        assert!(matches!(
            KvIndex::open(backend, config()),
            Err(IndexError::Store { .. })
        ));
    }
    let backend = MemBackend::new();
    backend.fail_next(Call::Scan, 0, 1);
    assert!(matches!(
        KvIndex::open(backend, config()),
        Err(IndexError::Store { .. })
    ));
    // The scan of the boot epochs is the second one.
    let backend = MemBackend::new();
    backend.fail_next(Call::Scan, 1, 1);
    assert!(matches!(
        KvIndex::open(backend, config()),
        Err(IndexError::Store { .. })
    ));
}

#[test]
fn put_over_an_entry_in_a_dropped_block_creates_and_releases_the_counts_of_its_other_blocks() {
    let index = ready();
    let len = 2 * MAX_OBJECT + 100;
    index
        .put_if_absent(b"k", chain_entry(1, 10, 5, len))
        .unwrap();
    assert_eq!(count(&index, 11), lc(1, MAX_OBJECT));
    replay(&index, vec![log_drop(2, 1, 11)]);
    assert_eq!(index.get(b"k").unwrap(), Lookup::Absent);
    // The drop took block 11's row with it; blocks 10 and 12 still count the hidden entry.
    assert_eq!(count(&index, 10), lc(1, MAX_OBJECT));
    assert_eq!(
        index.put_if_absent(b"k", entry(2, 20, 0, 6, 9)).unwrap(),
        PutOutcome::Created
    );
    assert_eq!(live_entry(&index, b"k").generation, generation(2));
    assert_eq!(count(&index, 10), lc(0, 0));
    assert_eq!(count(&index, 12), lc(0, 0));
    assert_eq!(count(&index, 20), lc(1, 9));
}
