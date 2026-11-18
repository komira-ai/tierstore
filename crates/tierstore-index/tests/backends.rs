//! One conformance suite for the ordered stores under the index, written once over `&dyn Backend`
//! and run on the in-memory store, on `fjall` and on `redb`; the in-memory store's crash and
//! failure injection; and a smoke of the index on each store.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::ops::Bound;
use std::path::Path;

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{Named, Tombstone};
use tierstore_index::backend::fjall::{FjallBackend, FjallOptions};
use tierstore_index::backend::mem::Call;
use tierstore_index::backend::redb::{RedbBackend, RedbOptions};
use tierstore_index::backend::{Backend, BackendError, MemBackend, Op, Space};
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_store_api::index::{
    DeleteOutcome, EntryLocation, Index, IndexEntry, Lookup, PutOutcome, ReplayMark,
};
use tierstore_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn put(space: Space, key: &[u8], value: &[u8]) -> Op {
    Op::Put {
        space,
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn del(space: Space, key: &[u8]) -> Op {
    Op::Delete {
        space,
        key: key.to_vec(),
    }
}

fn pair(key: &[u8], value: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (key.to_vec(), value.to_vec())
}

fn keys(b: &dyn Backend, space: Space, from: Bound<&[u8]>, limit: usize) -> Vec<Vec<u8>> {
    b.scan(space, from, limit)
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

fn byte_keys(list: &[&[u8]]) -> Vec<Vec<u8>> {
    list.iter().map(|k| k.to_vec()).collect()
}

/// The conformance cases, each over one empty store.
mod cases {
    use super::*;

    pub fn get_of_a_key_never_written_is_none(b: &dyn Backend) {
        for space in Space::ALL {
            assert_eq!(b.get(space, b"missing").unwrap(), None);
            assert_eq!(b.get(space, b"").unwrap(), None);
        }
    }

    pub fn put_then_get_returns_the_value_and_overwrite_replaces_it(b: &dyn Backend) {
        b.commit(vec![put(Space::Entries, b"k", b"one")]).unwrap();
        assert_eq!(b.get(Space::Entries, b"k").unwrap(), Some(b"one".to_vec()));
        b.commit(vec![put(Space::Entries, b"k", b"two-longer")])
            .unwrap();
        assert_eq!(
            b.get(Space::Entries, b"k").unwrap(),
            Some(b"two-longer".to_vec())
        );
        assert_eq!(b.get(Space::Entries, b"other").unwrap(), None);
        // Empty values, and bytes that are not text, are ordinary.
        b.commit(vec![
            put(Space::Entries, b"v", b""),
            put(Space::Entries, &[0, 255, 0], &[255, 0]),
        ])
        .unwrap();
        assert_eq!(b.get(Space::Entries, b"v").unwrap(), Some(Vec::new()));
        assert_eq!(
            b.get(Space::Entries, &[0, 255, 0]).unwrap(),
            Some(vec![255, 0])
        );
    }

    pub fn delete_removes_the_key_and_deleting_an_absent_key_is_fine(b: &dyn Backend) {
        b.commit(vec![
            put(Space::Meta, b"a", b"1"),
            put(Space::Meta, b"b", b"2"),
        ])
        .unwrap();
        b.commit(vec![del(Space::Meta, b"a"), del(Space::Meta, b"never")])
            .unwrap();
        assert_eq!(b.get(Space::Meta, b"a").unwrap(), None);
        assert_eq!(b.get(Space::Meta, b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(
            keys(b, Space::Meta, Bound::Unbounded, 10),
            byte_keys(&[b"b"])
        );
    }

    fn five(b: &dyn Backend) {
        b.commit(
            [b"e", b"a", b"c", b"b", b"d"]
                .iter()
                .map(|k| put(Space::Entries, *k, *k))
                .collect(),
        )
        .unwrap();
    }

    pub fn scan_from_each_kind_of_bound_starts_at_the_right_key(b: &dyn Backend) {
        five(b);
        let s = Space::Entries;
        assert_eq!(
            keys(b, s, Bound::Unbounded, 10),
            byte_keys(&[b"a", b"b", b"c", b"d", b"e"])
        );
        assert_eq!(
            keys(b, s, Bound::Included(b"c"), 10),
            byte_keys(&[b"c", b"d", b"e"])
        );
        assert_eq!(
            keys(b, s, Bound::Excluded(b"c"), 10),
            byte_keys(&[b"d", b"e"])
        );
        assert_eq!(
            keys(b, s, Bound::Included(b"cc"), 10),
            byte_keys(&[b"d", b"e"])
        );
        assert_eq!(
            keys(b, s, Bound::Excluded(b"cc"), 10),
            byte_keys(&[b"d", b"e"])
        );
        assert_eq!(
            keys(b, s, Bound::Included(b""), 10),
            byte_keys(&[b"a", b"b", b"c", b"d", b"e"])
        );
        assert_eq!(
            keys(b, s, Bound::Excluded(b"a"), 10),
            byte_keys(&[b"b", b"c", b"d", b"e"])
        );
        assert!(keys(b, s, Bound::Excluded(b"e"), 10).is_empty());
        assert!(keys(b, s, Bound::Included(b"z"), 10).is_empty());
        assert_eq!(
            b.scan(s, Bound::Included(b"d"), 10).unwrap(),
            vec![pair(b"d", b"d"), pair(b"e", b"e")]
        );
    }

    pub fn scan_returns_at_most_limit_pairs(b: &dyn Backend) {
        five(b);
        let s = Space::Entries;
        assert!(keys(b, s, Bound::Unbounded, 0).is_empty());
        assert_eq!(keys(b, s, Bound::Unbounded, 1), byte_keys(&[b"a"]));
        assert_eq!(
            keys(b, s, Bound::Included(b"b"), 2),
            byte_keys(&[b"b", b"c"])
        );
        assert_eq!(keys(b, s, Bound::Unbounded, 5).len(), 5);
        assert_eq!(keys(b, s, Bound::Unbounded, 6).len(), 5);
        assert_eq!(keys(b, s, Bound::Unbounded, usize::MAX).len(), 5);
    }

    pub fn scan_orders_keys_by_bytes_whatever_the_write_order(b: &dyn Backend) {
        let written: [&[u8]; 7] = [b"ab", &[255], b"a", &[0, 1], b"a\0", b"b", &[0]];
        // One batch for the first half, one for the rest, so the order is not one write's.
        b.commit(
            written[..4]
                .iter()
                .map(|k| put(Space::Tombs, k, b"v"))
                .collect(),
        )
        .unwrap();
        b.commit(
            written[4..]
                .iter()
                .map(|k| put(Space::Tombs, k, b"v"))
                .collect(),
        )
        .unwrap();
        assert_eq!(
            keys(b, Space::Tombs, Bound::Unbounded, 100),
            byte_keys(&[&[0], &[0, 1], b"a", b"a\0", b"ab", b"b", &[255]])
        );
    }

    pub fn scan_of_one_space_never_returns_another_spaces_keys(b: &dyn Backend) {
        // Every space but `Stripes` holds the same keys with its own name as the value.
        for space in Space::ALL {
            if space != Space::Stripes {
                b.commit(vec![
                    put(space, b"a", space.name().as_bytes()),
                    put(space, b"m", space.name().as_bytes()),
                ])
                .unwrap();
            }
        }
        // `Freed` also holds a key above every other space's.
        b.commit(vec![put(Space::Freed, b"z", b"freed")]).unwrap();
        for space in Space::ALL {
            let got = b.scan(space, Bound::Unbounded, 100).unwrap();
            if space == Space::Stripes {
                assert!(got.is_empty(), "the empty space {space:?} returned {got:?}");
                continue;
            }
            let name = space.name().as_bytes();
            let mut want = vec![pair(b"a", name), pair(b"m", name)];
            if space == Space::Freed {
                want.push(pair(b"z", b"freed"));
            }
            assert_eq!(got, want, "scan of {space:?}");
            assert_eq!(
                b.scan(space, Bound::Included(b"b"), 100).unwrap(),
                want[1..].to_vec(),
                "scan of {space:?} from b"
            );
            assert_eq!(b.get(space, b"a").unwrap(), Some(name.to_vec()));
        }
        // A scan past every key of a space is empty although later spaces hold greater keys.
        assert!(
            b.scan(Space::Entries, Bound::Included(b"n"), 100)
                .unwrap()
                .is_empty()
        );
        assert!(
            b.scan(Space::Stripes, Bound::Included(b""), 100)
                .unwrap()
                .is_empty()
        );
        // A delete in one space leaves the same key of the others.
        b.commit(vec![del(Space::Live, b"a")]).unwrap();
        assert_eq!(b.get(Space::Live, b"a").unwrap(), None);
        assert_eq!(b.get(Space::Meta, b"a").unwrap(), Some(b"meta".to_vec()));
    }

    pub fn a_batch_applies_its_operations_in_order(b: &dyn Backend) {
        b.commit(vec![put(Space::Entries, b"present", b"old")])
            .unwrap();
        b.commit(vec![
            // Put then delete of one key: gone.
            put(Space::Entries, b"fresh", b"1"),
            del(Space::Entries, b"fresh"),
            // Delete then put of one key: the put.
            del(Space::Entries, b"present"),
            put(Space::Entries, b"present", b"new"),
            // Two puts: the last.
            put(Space::Entries, b"twice", b"first"),
            put(Space::Entries, b"twice", b"second"),
            // Across spaces in the same batch.
            put(Space::Blocks, b"b", b"row"),
        ])
        .unwrap();
        assert_eq!(b.get(Space::Entries, b"fresh").unwrap(), None);
        assert_eq!(
            b.get(Space::Entries, b"present").unwrap(),
            Some(b"new".to_vec())
        );
        assert_eq!(
            b.get(Space::Entries, b"twice").unwrap(),
            Some(b"second".to_vec())
        );
        assert_eq!(b.get(Space::Blocks, b"b").unwrap(), Some(b"row".to_vec()));
    }

    pub fn an_empty_batch_changes_nothing(b: &dyn Backend) {
        b.commit(vec![put(Space::Meta, b"k", b"v")]).unwrap();
        b.commit(Vec::new()).unwrap();
        assert_eq!(b.scan(Space::Meta, Bound::Unbounded, 10).unwrap().len(), 1);
    }

    pub fn persist_of_a_store_with_nothing_committed_succeeds(b: &dyn Backend) {
        b.persist().unwrap();
        b.commit(vec![put(Space::Meta, b"k", b"v")]).unwrap();
        b.persist().unwrap();
        b.persist().unwrap();
        assert_eq!(b.get(Space::Meta, b"k").unwrap(), Some(b"v".to_vec()));
    }
}

type Open = fn(&Path) -> Result<Box<dyn Backend>, BackendError>;

fn open_mem(_: &Path) -> Result<Box<dyn Backend>, BackendError> {
    Ok(Box::new(MemBackend::new()))
}

fn open_fjall(path: &Path) -> Result<Box<dyn Backend>, BackendError> {
    Ok(Box::new(FjallBackend::open(
        path,
        &FjallOptions::default(),
    )?))
}

fn open_redb(path: &Path) -> Result<Box<dyn Backend>, BackendError> {
    Ok(Box::new(RedbBackend::open(path, &RedbOptions::default())?))
}

macro_rules! conformance {
    ($engine:ident, $open:expr) => {
        mod $engine {
            use super::*;

            fn run(case: fn(&dyn Backend)) {
                let dir = scratch(stringify!($engine));
                let open: Open = $open;
                let backend = open(&dir.path().join("store")).unwrap();
                case(&*backend);
            }

            #[test]
            fn get_of_a_key_never_written_is_none() {
                run(cases::get_of_a_key_never_written_is_none);
            }

            #[test]
            fn put_then_get_returns_the_value_and_overwrite_replaces_it() {
                run(cases::put_then_get_returns_the_value_and_overwrite_replaces_it);
            }

            #[test]
            fn delete_removes_the_key_and_deleting_an_absent_key_is_fine() {
                run(cases::delete_removes_the_key_and_deleting_an_absent_key_is_fine);
            }

            #[test]
            fn scan_from_each_kind_of_bound_starts_at_the_right_key() {
                run(cases::scan_from_each_kind_of_bound_starts_at_the_right_key);
            }

            #[test]
            fn scan_returns_at_most_limit_pairs() {
                run(cases::scan_returns_at_most_limit_pairs);
            }

            #[test]
            fn scan_orders_keys_by_bytes_whatever_the_write_order() {
                run(cases::scan_orders_keys_by_bytes_whatever_the_write_order);
            }

            #[test]
            fn scan_of_one_space_never_returns_another_spaces_keys() {
                run(cases::scan_of_one_space_never_returns_another_spaces_keys);
            }

            #[test]
            fn a_batch_applies_its_operations_in_order() {
                run(cases::a_batch_applies_its_operations_in_order);
            }

            #[test]
            fn an_empty_batch_changes_nothing() {
                run(cases::an_empty_batch_changes_nothing);
            }

            #[test]
            fn persist_of_a_store_with_nothing_committed_succeeds() {
                run(cases::persist_of_a_store_with_nothing_committed_succeeds);
            }
        }
    };
}

conformance!(mem, open_mem);
conformance!(fjall, open_fjall);
conformance!(redb, open_redb);

/// What the stores that keep files add: data survives a reopen, and a path that is not a usable
/// directory is an error naming the engine.
macro_rules! durable {
    ($engine:ident, $open:expr, $name:literal) => {
        mod $engine {
            use super::*;

            #[test]
            fn committed_and_persisted_data_survives_drop_and_reopen_of_the_directory() {
                let dir = scratch($name);
                let open: Open = $open;
                let path = dir.path().join("store");
                {
                    let b = open(&path).unwrap();
                    b.commit(vec![
                        put(Space::Entries, b"keep", b"1"),
                        put(Space::Entries, b"drop", b"2"),
                        put(Space::Blocks, b"row", b"3"),
                    ])
                    .unwrap();
                    b.commit(vec![del(Space::Entries, b"drop")]).unwrap();
                    b.persist().unwrap();
                }
                let b = open(&path).unwrap();
                assert_eq!(b.get(Space::Entries, b"keep").unwrap(), Some(b"1".to_vec()));
                assert_eq!(b.get(Space::Entries, b"drop").unwrap(), None);
                assert_eq!(b.get(Space::Blocks, b"row").unwrap(), Some(b"3".to_vec()));
                assert_eq!(
                    keys(&*b, Space::Entries, Bound::Unbounded, 10),
                    byte_keys(&[b"keep"])
                );
                // The reopened store takes more writes.
                b.commit(vec![put(Space::Entries, b"later", b"4")]).unwrap();
                b.persist().unwrap();
                assert_eq!(
                    keys(&*b, Space::Entries, Bound::Unbounded, 10),
                    byte_keys(&[b"keep", b"later"])
                );
            }

            #[test]
            fn opening_on_a_path_that_is_a_regular_file_fails_naming_the_engine() {
                let dir = scratch($name);
                let file = dir.path().join("file");
                std::fs::write(&file, b"not a directory").unwrap();
                let open: Open = $open;
                let err = open(&file).err().unwrap();
                assert_eq!(err.engine, $name);
                assert!(err.to_string().starts_with(concat!($name, ": ")), "{err}");
            }

            #[test]
            fn engine_names_itself() {
                let dir = scratch($name);
                let open: Open = $open;
                assert_eq!(open(&dir.path().join("store")).unwrap().engine(), $name);
            }
        }
    };
}

mod durable {
    use super::*;

    durable!(fjall, open_fjall, "fjall");
    durable!(redb, open_redb, "redb");

    #[test]
    fn redb_database_file_is_index_redb_in_the_directory() {
        let dir = scratch("redb-file");
        let path = dir.path().join("nested").join("store");
        drop(RedbBackend::open(&path, &RedbOptions::default()).unwrap());
        assert!(path.join("index.redb").is_file());
    }

    #[test]
    fn redb_open_of_a_damaged_database_file_fails_naming_the_engine() {
        let dir = scratch("redb-damaged");
        std::fs::write(dir.path().join("index.redb"), vec![7u8; 8192]).unwrap();
        let err = RedbBackend::open(dir.path(), &RedbOptions::default()).unwrap_err();
        assert_eq!(err.engine, "redb");
        assert_eq!(err.op, "open");
    }

    #[test]
    fn redb_backend_with_a_small_cache_reads_what_it_wrote() {
        let dir = scratch("redb-cache");
        let opts = RedbOptions {
            cache_bytes: 1 << 20,
        };
        let b = RedbBackend::open(&dir.path().join("store"), &opts).unwrap();
        b.commit(vec![put(Space::Meta, b"k", b"v")]).unwrap();
        assert_eq!(b.get(Space::Meta, b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn fjall_backend_with_a_small_cache_reads_what_it_wrote() {
        let dir = scratch("fjall-cache");
        let opts = FjallOptions {
            cache_bytes: 1 << 20,
        };
        let b = FjallBackend::open(&dir.path().join("store"), &opts).unwrap();
        b.commit(vec![put(Space::Meta, b"k", b"v")]).unwrap();
        assert_eq!(b.get(Space::Meta, b"k").unwrap(), Some(b"v".to_vec()));
        assert!(format!("{b:?}").starts_with("FjallBackend"));
    }

    #[test]
    fn fjall_commit_with_an_empty_key_fails_whole_and_names_the_engine() {
        let dir = scratch("fjall-empty-key");
        let b = FjallBackend::open(&dir.path().join("store"), &FjallOptions::default()).unwrap();
        for bad in [put(Space::Meta, b"", b"v"), del(Space::Meta, b"")] {
            let err = b
                .commit(vec![put(Space::Meta, b"ok", b"v"), bad])
                .unwrap_err();
            assert_eq!(err.to_string(), "fjall: commit: an empty key");
        }
        assert_eq!(b.get(Space::Meta, b"ok").unwrap(), None);
    }

    #[test]
    fn default_options_give_a_cache_of_256_mib() {
        assert_eq!(FjallOptions::default().cache_bytes, 256 << 20);
        assert_eq!(RedbOptions::default().cache_bytes, 256 << 20);
    }
}

mod mem_specifics {
    use super::*;

    fn dump_keys(b: &MemBackend) -> Vec<(Space, Vec<u8>)> {
        b.dump().into_iter().map(|(s, k, _)| (s, k)).collect()
    }

    #[test]
    fn crashed_keeps_the_durable_state_and_the_first_k_unpersisted_batches() {
        let b = MemBackend::new();
        b.commit(vec![put(Space::Entries, b"a", b"1")]).unwrap();
        b.persist().unwrap();
        b.commit(vec![put(Space::Entries, b"b", b"2")]).unwrap();
        b.commit(vec![put(Space::Entries, b"c", b"3")]).unwrap();
        b.commit(vec![del(Space::Entries, b"a")]).unwrap();
        assert_eq!(b.unpersisted(), 3);
        let all = |s: &str| (Space::Entries, s.as_bytes().to_vec());
        assert_eq!(dump_keys(&b.crashed(0)), vec![all("a")]);
        assert_eq!(dump_keys(&b.crashed(1)), vec![all("a"), all("b")]);
        assert_eq!(dump_keys(&b.crashed(2)), vec![all("a"), all("b"), all("c")]);
        assert_eq!(dump_keys(&b.crashed(3)), vec![all("b"), all("c")]);
        assert_eq!(dump_keys(&b.crashed(4)), vec![all("b"), all("c")]);
        assert_eq!(dump_keys(&b.crashed(usize::MAX)), vec![all("b"), all("c")]);
        // The store itself is untouched by asking.
        assert_eq!(dump_keys(&b), vec![all("b"), all("c")]);
        assert_eq!(b.unpersisted(), 3);
    }

    #[test]
    fn a_crashed_store_is_all_durable_and_carries_no_scheduled_failure() {
        let b = MemBackend::new();
        b.commit(vec![put(Space::Meta, b"a", b"1")]).unwrap();
        b.fail_next(Call::Get, 0, 5);
        let c = b.crashed(1);
        assert_eq!(c.unpersisted(), 0);
        assert_eq!(c.get(Space::Meta, b"a").unwrap(), Some(b"1".to_vec()));
        // Crashing it again keeps what it held.
        assert_eq!(dump_keys(&c.crashed(0)), vec![(Space::Meta, b"a".to_vec())]);
        assert!(b.get(Space::Meta, b"a").is_err());
    }

    #[test]
    fn persist_moves_every_batch_to_the_durable_state() {
        let b = MemBackend::new();
        b.commit(vec![put(Space::Meta, b"a", b"1")]).unwrap();
        b.commit(vec![put(Space::Meta, b"b", b"2")]).unwrap();
        assert_eq!(dump_keys(&b.crashed(0)), Vec::new());
        b.persist().unwrap();
        assert_eq!(b.unpersisted(), 0);
        assert_eq!(
            dump_keys(&b.crashed(0)),
            vec![(Space::Meta, b"a".to_vec()), (Space::Meta, b"b".to_vec())]
        );
    }

    #[test]
    fn dump_lists_every_space_in_order_with_values() {
        let b = MemBackend::default();
        b.commit(vec![
            put(Space::Freed, b"x", b"7"),
            put(Space::Entries, b"b", b"2"),
            put(Space::Entries, b"a", b"1"),
        ])
        .unwrap();
        assert_eq!(
            b.dump(),
            vec![
                (Space::Entries, b"a".to_vec(), b"1".to_vec()),
                (Space::Entries, b"b".to_vec(), b"2".to_vec()),
                (Space::Freed, b"x".to_vec(), b"7".to_vec()),
            ]
        );
    }

    fn failed(r: Result<impl std::fmt::Debug, BackendError>, op: &str) {
        let err = r.unwrap_err();
        assert_eq!(err.engine, "memory");
        assert_eq!(err.op, op);
        assert_eq!(err.to_string(), format!("memory: {op}: injected failure"));
    }

    #[test]
    fn fail_next_get_fails_the_given_number_of_gets_then_recovers() {
        let b = MemBackend::new();
        b.fail_next(Call::Get, 0, 2);
        failed(b.get(Space::Meta, b"k"), "get");
        failed(b.get(Space::Meta, b"k"), "get");
        assert_eq!(b.get(Space::Meta, b"k").unwrap(), None);
        // Other kinds of call were not affected.
        b.commit(vec![put(Space::Meta, b"k", b"v")]).unwrap();
    }

    #[test]
    fn fail_next_scan_fails_scans_only() {
        let b = MemBackend::new();
        b.fail_next(Call::Scan, 0, 1);
        assert_eq!(b.get(Space::Meta, b"k").unwrap(), None);
        failed(b.scan(Space::Meta, Bound::Unbounded, 1), "scan");
        assert!(b.scan(Space::Meta, Bound::Unbounded, 1).unwrap().is_empty());
    }

    #[test]
    fn a_failed_commit_applies_nothing_and_a_failed_persist_keeps_the_batches_unpersisted() {
        let b = MemBackend::new();
        b.fail_next(Call::Commit, 0, 1);
        failed(b.commit(vec![put(Space::Meta, b"k", b"v")]), "commit");
        assert_eq!(b.get(Space::Meta, b"k").unwrap(), None);
        assert_eq!(b.unpersisted(), 0);
        b.commit(vec![put(Space::Meta, b"k", b"v")]).unwrap();
        b.fail_next(Call::Persist, 0, 1);
        failed(b.persist(), "persist");
        assert_eq!(b.unpersisted(), 1);
        b.persist().unwrap();
        assert_eq!(b.unpersisted(), 0);
    }

    #[test]
    fn fail_next_lets_after_calls_through_first() {
        let b = MemBackend::new();
        b.fail_next(Call::Get, 2, 1);
        assert!(b.get(Space::Meta, b"k").is_ok());
        assert!(b.get(Space::Meta, b"k").is_ok());
        failed(b.get(Space::Meta, b"k"), "get");
        assert!(b.get(Space::Meta, b"k").is_ok());
    }

    #[test]
    fn schedules_of_several_kinds_and_several_windows_apply_independently() {
        let b = MemBackend::new();
        b.fail_next(Call::Scan, 0, 1);
        b.fail_next(Call::Get, 0, 1);
        b.fail_next(Call::Get, 1, 1);
        // Get 1 is the first window's, get 2 passes the second's countdown, get 3 is its failure.
        failed(b.get(Space::Meta, b"k"), "get");
        assert!(b.get(Space::Meta, b"k").is_ok());
        failed(b.get(Space::Meta, b"k"), "get");
        assert!(b.get(Space::Meta, b"k").is_ok());
        failed(b.scan(Space::Meta, Bound::Unbounded, 1), "scan");
        assert!(b.scan(Space::Meta, Bound::Unbounded, 1).is_ok());
    }

    #[test]
    fn engine_is_memory() {
        assert_eq!(MemBackend::new().engine(), "memory");
    }
}

/// The index on each store, through the `Index` trait.
mod index_smoke {
    use super::*;

    fn cfg() -> IndexConfig {
        IndexConfig {
            node: NodeId(1),
            geometry: BlockGeometry::default(),
        }
    }

    fn block() -> BlockId {
        BlockId::new(NodeId(1), 7).unwrap()
    }

    fn stamp(ms: u64) -> Stamp {
        Stamp::from_parts(ms, 0).unwrap()
    }

    fn entry(tag: u8, offset: u64, length: u64) -> IndexEntry {
        IndexEntry {
            generation: Generation([tag; 16]),
            location: EntryLocation::Block {
                block: block(),
                offset,
            },
            length,
            stamp: stamp(1000 + u64::from(tag)),
            flags: RecordFlags(0),
            prior_blocks: Vec::new(),
            promoted: None,
        }
    }

    fn mark() -> ReplayMark {
        ReplayMark {
            open_segments: vec![(block(), 4096)],
            tomb_cursors: Vec::new(),
        }
    }

    /// Writes through the trait, checking each answer.
    fn write<B: Backend>(index: &KvIndex<B>) {
        assert!(matches!(
            index.get(b"a/1"),
            Err(tierstore_store_api::index::IndexError::Indexing)
        ));
        index.set_ready(true);
        assert_eq!(index.get(b"a/1").unwrap(), Lookup::Absent);
        for (key, tag, off, len) in [
            (&b"a/1"[..], 1u8, 0u64, 10u64),
            (b"a/2", 2, 100, 20),
            (b"b/1", 3, 200, 30),
        ] {
            assert_eq!(
                index.put_if_absent(key, entry(tag, off, len)).unwrap(),
                PutOutcome::Created
            );
        }
        assert_eq!(index.get(b"a/2").unwrap(), Lookup::Live(entry(2, 100, 20)));
        let listed: Vec<Vec<u8>> = index
            .list(b"a/", None, 10)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(listed, byte_keys(&[b"a/1", b"a/2"]));
        let after: Vec<Vec<u8>> = index
            .list(b"a/", Some(b"a/1"), 10)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(after, byte_keys(&[b"a/2"]));
        let live = index.live(block()).unwrap();
        assert_eq!((live.entries, live.bytes), (3, 60));
        let tomb = Tombstone {
            key: b"a/2".to_vec(),
            generation: Generation([2; 16]),
            stamp: stamp(2000),
            names: vec![Named::Block(block())],
        };
        assert_eq!(
            index.delete(&tomb).unwrap(),
            DeleteOutcome::Deleted(entry(2, 100, 20))
        );
        assert_eq!(index.get(b"a/2").unwrap(), Lookup::Deleted(stamp(2000)));
        assert_eq!(index.list(b"a/", None, 10).unwrap().len(), 1);
        index.flush_mark(&mark()).unwrap();
        assert_eq!(index.replay_mark().unwrap(), Some(mark()));
    }

    /// Reads what [`write`] left, on a reopened store.
    fn read<B: Backend>(index: &KvIndex<B>) {
        assert_eq!(index.replay_mark().unwrap(), Some(mark()));
        index.set_ready(true);
        assert_eq!(index.get(b"a/1").unwrap(), Lookup::Live(entry(1, 0, 10)));
        assert_eq!(index.get(b"a/2").unwrap(), Lookup::Deleted(stamp(2000)));
        assert_eq!(index.get(b"b/1").unwrap(), Lookup::Live(entry(3, 200, 30)));
        assert_eq!(index.list(b"", None, 10).unwrap().len(), 2);
        let live = index.live(block()).unwrap();
        assert_eq!((live.entries, live.bytes), (2, 40));
    }

    #[test]
    fn index_on_the_memory_store_serves_the_trait_and_keeps_the_mark_across_a_crash() {
        let index = KvIndex::open(MemBackend::new(), cfg()).unwrap();
        write(&index);
        let after = KvIndex::open(index.backend().crashed(0), cfg()).unwrap();
        read(&after);
    }

    #[test]
    fn index_on_fjall_serves_the_trait_and_keeps_the_mark_across_reopen() {
        let dir = scratch("smoke-fjall");
        let path = dir.path().join("store");
        let open = || FjallBackend::open(&path, &FjallOptions::default()).unwrap();
        let index = KvIndex::open(open(), cfg()).unwrap();
        write(&index);
        drop(index);
        read(&KvIndex::open(open(), cfg()).unwrap());
    }

    #[test]
    fn index_on_redb_serves_the_trait_and_keeps_the_mark_across_reopen() {
        let dir = scratch("smoke-redb");
        let path = dir.path().join("store");
        let open = || RedbBackend::open(&path, &RedbOptions::default()).unwrap();
        let index = KvIndex::open(open(), cfg()).unwrap();
        write(&index);
        drop(index);
        read(&KvIndex::open(open(), cfg()).unwrap());
    }
}
