//! `list`: order, prefix, `after`, `limit` and paging past runs of tombstones and hidden keys
//! longer than one scan page.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_index::KvIndex;
use tierstore_index::backend::MemBackend;
use tierstore_store_api::index::Index;

fn keys(index: &KvIndex<MemBackend>, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> Vec<String> {
    index
        .list(prefix, after, limit)
        .unwrap()
        .into_iter()
        .map(|(k, _)| String::from_utf8(k).unwrap())
        .collect()
}

/// Live keys `a/1`, `a/2`, `b/1`..`b/N` (tombstones), `c/1`..`c/N` (in a dropped block), `d/1`.
fn index_with_long_dead_runs(n: usize) -> KvIndex<MemBackend> {
    let index = ready();
    index.put_if_absent(b"a/1", entry(1, 5, 0, 5, 1)).unwrap();
    index.put_if_absent(b"a/2", entry(1, 5, 8, 5, 1)).unwrap();
    for i in 0..n {
        index
            .delete(&tomb(format!("b/{i:05}").as_bytes(), 0, 4, vec![]))
            .unwrap();
    }
    let hidden: Vec<_> = (0..n)
        .map(|i| record(format!("c/{i:05}").as_bytes(), entry(2, 8, i as u64, 5, 1)))
        .collect();
    replay(&index, hidden);
    replay(&index, vec![log_drop(2, 1, 8)]);
    index.put_if_absent(b"d/1", entry(3, 9, 0, 5, 1)).unwrap();
    index
}

#[test]
fn list_returns_the_live_keys_with_the_prefix_in_key_order() {
    let index = ready();
    for k in ["b/2", "a/1", "b/1", "c/1", "b/3"] {
        index.put_if_absent(k.as_bytes(), entry(1, 5, 0, 5, 1)).unwrap();
    }
    assert_eq!(keys(&index, b"b/", None, 10), ["b/1", "b/2", "b/3"]);
    assert_eq!(keys(&index, b"", None, 10), ["a/1", "b/1", "b/2", "b/3", "c/1"]);
    assert_eq!(keys(&index, b"x", None, 10), Vec::<String>::new());
    assert_eq!(keys(&index, b"b/", None, 2), ["b/1", "b/2"]);
}

#[test]
fn list_with_limit_zero_returns_nothing() {
    let index = ready();
    index.put_if_absent(b"a", entry(1, 5, 0, 5, 1)).unwrap();
    assert!(index.list(b"", None, 0).unwrap().is_empty());
}

#[test]
fn list_after_a_key_inside_the_prefix_continues_strictly_after_it() {
    let index = ready();
    for k in ["b/1", "b/2", "b/3"] {
        index.put_if_absent(k.as_bytes(), entry(1, 5, 0, 5, 1)).unwrap();
    }
    assert_eq!(keys(&index, b"b/", Some(b"b/1"), 10), ["b/2", "b/3"]);
    assert_eq!(keys(&index, b"b/", Some(b"b/3"), 10), Vec::<String>::new());
    // Strictly after the prefix itself (a key equal to the prefix is not listed).
    index.put_if_absent(b"b/", entry(2, 5, 0, 6, 1)).unwrap();
    assert_eq!(keys(&index, b"b/", None, 10), ["b/", "b/1", "b/2", "b/3"]);
    assert_eq!(keys(&index, b"b/", Some(b"b/"), 10), ["b/1", "b/2", "b/3"]);
}

#[test]
fn list_after_a_key_before_the_prefix_starts_at_the_prefix() {
    let index = ready();
    for k in ["a/1", "b/1", "b/2"] {
        index.put_if_absent(k.as_bytes(), entry(1, 5, 0, 5, 1)).unwrap();
    }
    assert_eq!(keys(&index, b"b/", Some(b"a/9"), 10), ["b/1", "b/2"]);
    assert_eq!(keys(&index, b"b/", Some(b""), 10), ["b/1", "b/2"]);
}

#[test]
fn list_after_a_key_past_the_prefix_returns_nothing() {
    let index = ready();
    for k in ["b/1", "b/2", "c/1"] {
        index.put_if_absent(k.as_bytes(), entry(1, 5, 0, 5, 1)).unwrap();
    }
    assert!(keys(&index, b"b/", Some(b"b/9"), 10).is_empty());
    assert!(keys(&index, b"b/", Some(b"c/0"), 10).is_empty());
    assert!(keys(&index, b"b/", Some(b"zzz"), 10).is_empty());
}

#[test]
fn list_skips_tombstone_entries_and_keys_of_dropped_blocks() {
    let index = ready();
    index.put_if_absent(b"a", entry(1, 5, 0, 5, 1)).unwrap();
    index.put_if_absent(b"b", entry(1, 6, 0, 5, 1)).unwrap();
    index.put_if_absent(b"c", entry(1, 7, 0, 5, 1)).unwrap();
    index.delete(&tomb(b"a", 1, 9, vec![])).unwrap();
    replay(&index, vec![log_drop(2, 1, 6)]);
    assert_eq!(keys(&index, b"", None, 10), ["c"]);
}

#[test]
fn list_pages_past_more_tombstones_and_hidden_keys_than_one_scan_page() {
    let index = index_with_long_dead_runs(1100);
    // Several limits: the scan page is twice the limit up to 1024, and at least 16.
    assert_eq!(keys(&index, b"", None, 1), ["a/1"]);
    assert_eq!(keys(&index, b"", None, 2), ["a/1", "a/2"]);
    for limit in [3, 500, 5000] {
        assert_eq!(keys(&index, b"", None, limit), ["a/1", "a/2", "d/1"]);
    }
    assert_eq!(keys(&index, b"", Some(b"a/1"), 1), ["a/2"]);
    assert_eq!(keys(&index, b"", Some(b"a/2"), 1), ["d/1"]);
    assert_eq!(keys(&index, b"", Some(b"a/2"), 1000), ["d/1"]);
    assert!(keys(&index, b"", Some(b"d/1"), 1).is_empty());
    assert!(keys(&index, b"b/", None, 1).is_empty());
    assert!(keys(&index, b"c/", None, 100).is_empty());
    // The tombstones and their count are visible to inspection.
    assert_eq!(index.tombstones().unwrap().len(), 1100);
}

#[test]
fn list_of_a_prefix_that_ends_the_keyspace_with_a_full_page_of_dead_keys_ends_cleanly() {
    let index = ready();
    for i in 0..16 {
        index.delete(&tomb(format!("t/{i:02}").as_bytes(), 0, 4, vec![])).unwrap();
    }
    assert!(keys(&index, b"t/", None, 1).is_empty());
}

#[test]
fn list_of_a_run_of_live_keys_longer_than_a_page_returns_them_all_in_order() {
    let index = ready();
    let facts: Vec<_> = (0..2500)
        .map(|i| record(format!("k{i:05}").as_bytes(), entry(1, 5, i, 5, 1)))
        .collect();
    replay(&index, facts);
    let all = keys(&index, b"k", None, 10_000);
    assert_eq!(all.len(), 2500);
    assert!(all.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(keys(&index, b"k", Some(b"k02497"), 10), ["k02498", "k02499"]);
    // Pages of 700: each continues after the last key returned.
    let mut after: Option<Vec<u8>> = None;
    let mut seen = 0;
    while let Some((k, _)) = index
        .list(b"k", after.as_deref(), 700)
        .unwrap()
        .into_iter()
        .last()
    {
        seen += 700.min(2500 - seen);
        after = Some(k);
    }
    assert_eq!(seen, 2500);
}
