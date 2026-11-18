//! `index-sync`: export pages, import with the newest fact winning, and the wire bytes of a page.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, Generation, KeyHash, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{BlockDrop, LargeFile, LogEntry, LogRecord, Tombstone};
use tierstore_index::backend::{Backend, MemBackend, Op, Space, mem::Call};
use tierstore_index::sync::{SyncItem, SyncPage, SyncValue, decode_page, encode_page};
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_store_api::index::{
    DeleteOutcome, EntryLocation, Fact, HotClean, Index, IndexEntry, IndexError, LiveCount, Lookup,
    PutOutcome,
};

type Node = KvIndex<MemBackend>;

fn node(id: u8) -> Node {
    let index = KvIndex::open(
        MemBackend::new(),
        IndexConfig {
            node: NodeId(id),
            geometry: BlockGeometry::DEFAULT,
        },
    )
    .expect("open");
    index.set_ready(true);
    index
}

fn entry(stamp: u64, block: u64) -> IndexEntry {
    IndexEntry {
        generation: Generation([stamp as u8; 16]),
        location: EntryLocation::Block {
            block: BlockId::from_raw(block),
            offset: stamp,
        },
        length: 100 + stamp,
        stamp: Stamp::from_raw(stamp),
        flags: RecordFlags(0),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

fn put(n: &Node, key: &str, stamp: u64, block: u64) {
    assert_eq!(
        n.put_if_absent(key.as_bytes(), entry(stamp, block))
            .expect("put"),
        PutOutcome::Created
    );
}

fn tombstone(key: &str, stamp: u64) -> Tombstone {
    Tombstone {
        key: key.as_bytes().to_vec(),
        generation: Generation([0; 16]),
        stamp: Stamp::from_raw(stamp),
        names: Vec::new(),
    }
}

fn live_item(key: &str, stamp: u64, block: u64) -> SyncItem {
    SyncItem {
        key: key.as_bytes().to_vec(),
        value: SyncValue::Live(entry(stamp, block)),
    }
}

fn dead_item(key: &str, stamp: u64) -> SyncItem {
    SyncItem {
        key: key.as_bytes().to_vec(),
        value: SyncValue::Deleted(Stamp::from_raw(stamp)),
    }
}

fn all(_: &[u8]) -> bool {
    true
}

fn keys(page: &SyncPage) -> Vec<String> {
    page.items
        .iter()
        .map(|i| String::from_utf8(i.key.clone()).expect("utf8"))
        .collect()
}

fn count(n: &Node, block: u64) -> LiveCount {
    n.live(BlockId::from_raw(block)).expect("live")
}

/// Ten keys `k00` to `k09` in block 1, with `k03` and `k07` deleted.
fn source() -> Node {
    let n = node(0);
    for i in 0..10_u64 {
        put(&n, &format!("k{i:02}"), 10 + i, 1);
    }
    for key in ["k03", "k07"] {
        n.delete(&tombstone(key, 50)).expect("delete");
    }
    n
}

#[test]
fn export_pages_come_in_key_order_with_live_and_tombstone_entries() {
    let n = source();
    let page = n.export(None, 100, &all).expect("export");
    assert_eq!(
        keys(&page),
        [
            "k00", "k01", "k02", "k03", "k04", "k05", "k06", "k07", "k08", "k09"
        ]
    );
    assert_eq!(page.next, None);
    assert_eq!(page.items[3], dead_item("k03", 50));
    assert_eq!(page.items[0], live_item("k00", 10, 1));
}

#[test]
fn export_pages_honour_want_limit_and_next() {
    let n = source();
    let even = |k: &[u8]| (k[2] - b'0').is_multiple_of(2);
    let first = n.export(None, 3, &even).expect("export");
    assert_eq!(keys(&first), ["k00", "k02", "k04"]);
    assert_eq!(first.next, Some(b"k04".to_vec()));
    let second = n.export(first.next.as_deref(), 3, &even).expect("export");
    assert_eq!(keys(&second), ["k06", "k08"]);
    assert_eq!(second.next, None);
}

#[test]
fn export_with_limit_zero_is_an_empty_page() {
    let page = source().export(None, 0, &all).expect("export");
    assert_eq!(page, SyncPage::default());
}

#[test]
fn export_scans_past_a_store_page_to_find_wanted_keys() {
    let n = node(0);
    for i in 0..2100_u64 {
        put(&n, &format!("k{i:05}"), 10, 1);
    }
    let two = |k: &[u8]| k == b"k01500" || k == b"k02050";
    let page = n.export(None, 10, &two).expect("export");
    assert_eq!(keys(&page), ["k01500", "k02050"]);
    assert_eq!(page.next, None);
    let page = n.export(None, 1500, &all).expect("export");
    assert_eq!(page.items.len(), 1500);
    assert_eq!(page.next, Some(b"k01499".to_vec()));
    let rest = n.export(page.next.as_deref(), 1500, &all).expect("export");
    assert_eq!(rest.items.len(), 600);
    assert_eq!(rest.next, None);
}

#[test]
fn export_of_exactly_one_store_page_ends_without_a_next() {
    let n = node(0);
    for i in 0..1024_u64 {
        put(&n, &format!("k{i:05}"), 10, 1);
    }
    let page = n.export(None, 2000, &|_: &[u8]| false).expect("export");
    assert!(page.items.is_empty());
    assert_eq!(page.next, None);
}

#[test]
fn export_works_while_the_index_is_being_built() {
    let n = source();
    n.set_ready(false);
    assert!(matches!(n.get(b"k00"), Err(IndexError::Indexing)));
    let page = n.export(None, 100, &all).expect("export");
    assert_eq!(page.items.len(), 10);
}

#[test]
fn export_leaves_out_the_promoted_copy_hint_and_import_never_takes_one() {
    let n = node(0);
    put(&n, "k", 10, 1);
    let hint = HotClean {
        node: NodeId(0),
        boot: 5,
        seq: 6,
        offset: 7,
    };
    n.set_boot(NodeId(0), 5).expect("boot");
    assert!(
        n.promote(b"k", Generation([10; 16]), Stamp::from_raw(10), hint)
            .expect("promote")
    );
    match n.get(b"k").expect("get") {
        Lookup::Live(e) => assert_eq!(e.promoted, Some(hint)),
        other => panic!("{other:?}"),
    }
    let page = n.export(None, 10, &all).expect("export");
    assert_eq!(page.items, vec![live_item("k", 10, 1)]);
    let target = node(1);
    let with_hint = IndexEntry {
        promoted: Some(hint),
        ..entry(10, 1)
    };
    target
        .import(&[SyncItem {
            key: b"k".to_vec(),
            value: SyncValue::Live(with_hint),
        }])
        .expect("import");
    match target.get(b"k").expect("get") {
        Lookup::Live(e) => assert_eq!(e.promoted, None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn export_leaves_out_entries_in_a_dropped_block() {
    let n = node(0);
    put(&n, "in-dropped", 10, 5);
    put(&n, "in-kept", 11, 6);
    let drop = Fact::Log(LogRecord {
        origin: NodeId(0),
        seq: 1,
        entry: LogEntry::BlockDrop(BlockDrop {
            block: BlockId::from_raw(5),
            stamp: Stamp::from_raw(20),
        }),
    });
    n.replay(&mut std::iter::once(drop)).expect("replay");
    let page = n.export(None, 10, &all).expect("export");
    assert_eq!(keys(&page), ["in-kept"]);
}

#[test]
fn export_of_an_undecodable_entry_is_an_error() {
    let n = node(0);
    n.backend()
        .commit(vec![Op::Put {
            space: Space::Entries,
            key: b"bad".to_vec(),
            value: vec![0xee],
        }])
        .expect("commit");
    assert!(matches!(
        n.export(None, 10, &all),
        Err(IndexError::Store { .. })
    ));
}

#[test]
fn export_of_a_failing_store_is_an_error() {
    let n = source();
    n.backend().fail_next(Call::Scan, 0, 1);
    assert!(matches!(
        n.export(None, 10, &all),
        Err(IndexError::Store { op: "scan", .. })
    ));
}

#[test]
fn import_into_an_empty_index_gives_the_sources_wanted_entries_and_counts() {
    let src = source();
    let wanted = |k: &[u8]| k != b"k05" && k != b"k07";
    let dst = node(1);
    let mut after: Option<Vec<u8>> = None;
    let (mut facts, mut applied) = (0, 0);
    loop {
        let page = src.export(after.as_deref(), 3, &wanted).expect("export");
        let stats = dst.import(&page.items).expect("import");
        facts += stats.facts;
        applied += stats.applied;
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!((facts, applied), (8, 8));
    for i in 0..10_u64 {
        let key = format!("k{i:02}");
        let got = dst.get(key.as_bytes()).expect("get");
        match key.as_str() {
            "k05" | "k07" => assert_eq!(got, Lookup::Absent, "{key}"),
            "k03" => assert_eq!(got, Lookup::Deleted(Stamp::from_raw(50))),
            _ => assert_eq!(got, src.get(key.as_bytes()).expect("get"), "{key}"),
        }
    }
    // Eight items were imported, one of them a tombstone: seven live entries in block 1, the
    // block the source holds eight of.
    assert_eq!(
        count(&dst, 1),
        LiveCount {
            entries: 7,
            bytes: 800
        }
    );
    assert_eq!(count(&src, 1).entries, 8);
}

#[test]
fn import_is_newest_wins_against_older_and_newer_local_entries() {
    let dst = node(1);
    put(&dst, "older", 10, 1);
    put(&dst, "newer", 30, 1);
    let stats = dst
        .import(&[
            live_item("older", 20, 2),
            live_item("newer", 20, 2),
            live_item("fresh", 20, 2),
        ])
        .expect("import");
    assert_eq!((stats.facts, stats.applied), (3, 2));
    assert_eq!(dst.get(b"older").expect("get"), Lookup::Live(entry(20, 2)));
    assert_eq!(dst.get(b"newer").expect("get"), Lookup::Live(entry(30, 1)));
    assert_eq!(dst.get(b"fresh").expect("get"), Lookup::Live(entry(20, 2)));
    assert_eq!(count(&dst, 1).entries, 1);
    assert_eq!(count(&dst, 2).entries, 2);
}

#[test]
fn import_of_a_live_item_older_than_a_local_tombstone_stays_deleted() {
    let dst = node(1);
    dst.delete(&tombstone("k", 40)).expect("delete");
    let stats = dst.import(&[live_item("k", 20, 1)]).expect("import");
    assert_eq!(stats.applied, 0);
    assert_eq!(
        dst.get(b"k").expect("get"),
        Lookup::Deleted(Stamp::from_raw(40))
    );
    let stats = dst.import(&[live_item("k", 60, 1)]).expect("import");
    assert_eq!(stats.applied, 1);
    assert_eq!(dst.get(b"k").expect("get"), Lookup::Live(entry(60, 1)));
}

#[test]
fn import_of_a_deleted_item_over_an_older_live_entry_deletes_it_and_counts_down() {
    let dst = node(1);
    put(&dst, "k", 10, 1);
    put(&dst, "other", 11, 1);
    assert_eq!(count(&dst, 1).entries, 2);
    let stats = dst.import(&[dead_item("k", 30)]).expect("import");
    assert_eq!((stats.facts, stats.applied), (1, 1));
    assert_eq!(
        dst.get(b"k").expect("get"),
        Lookup::Deleted(Stamp::from_raw(30))
    );
    assert_eq!(
        count(&dst, 1),
        LiveCount {
            entries: 1,
            bytes: 111
        }
    );
    // The same item again changes nothing.
    let stats = dst.import(&[dead_item("k", 30)]).expect("import");
    assert_eq!(stats.applied, 0);
}

#[test]
fn import_of_a_deleted_item_older_than_a_live_entry_keeps_the_entry() {
    let dst = node(1);
    put(&dst, "k", 50, 1);
    dst.import(&[dead_item("k", 30)]).expect("import");
    assert_eq!(dst.get(b"k").expect("get"), Lookup::Live(entry(50, 1)));
    assert_eq!(count(&dst, 1).entries, 1);
    assert!(matches!(
        dst.delete(&tombstone("k", 40)).expect("delete"),
        DeleteOutcome::Newer(_)
    ));
}

#[test]
fn import_while_indexing_applies_the_page() {
    let dst = node(1);
    dst.set_ready(false);
    dst.import(&[live_item("k", 10, 1)]).expect("import");
    dst.set_ready(true);
    assert_eq!(dst.get(b"k").expect("get"), Lookup::Live(entry(10, 1)));
}

#[test]
fn import_of_a_page_is_one_batch_and_a_failing_commit_changes_nothing() {
    let dst = node(1);
    dst.backend().fail_next(Call::Commit, 0, 1);
    assert!(matches!(
        dst.import(&[live_item("a", 10, 1), live_item("b", 11, 1)]),
        Err(IndexError::Store { .. })
    ));
    assert_eq!(dst.get(b"a").expect("get"), Lookup::Absent);
    assert_eq!(dst.get(b"b").expect("get"), Lookup::Absent);
    let before = dst.backend().unpersisted();
    dst.import(&[live_item("a", 10, 1), live_item("b", 11, 1)])
        .expect("import");
    assert_eq!(dst.backend().unpersisted(), before + 1);
}

#[test]
fn import_with_an_unreadable_local_entry_is_an_error() {
    let dst = node(1);
    dst.backend()
        .commit(vec![Op::Put {
            space: Space::Entries,
            key: b"k".to_vec(),
            value: vec![0xee],
        }])
        .expect("commit");
    assert!(matches!(
        dst.import(&[live_item("k", 10, 1)]),
        Err(IndexError::Store { .. })
    ));
    assert!(matches!(
        dst.import(&[dead_item("k", 10)]),
        Err(IndexError::Store { .. })
    ));
}

fn rich_page() -> SyncPage {
    let chain = IndexEntry {
        location: EntryLocation::Chain {
            first: BlockId::from_raw(9),
        },
        prior_blocks: vec![BlockId::from_raw(3), BlockId::from_raw(4)],
        flags: RecordFlags(1),
        ..entry(12, 9)
    };
    let large = IndexEntry {
        location: EntryLocation::Large(LargeFile {
            key_hash: KeyHash([7; 16]),
            generation: Generation([8; 16]),
            size: 5_000_000_000,
        }),
        ..entry(13, 1)
    };
    let hinted = IndexEntry {
        promoted: Some(HotClean {
            node: NodeId(2),
            boot: 3,
            seq: 4,
            offset: 5,
        }),
        ..entry(14, 1)
    };
    SyncPage {
        items: vec![
            live_item("a", 10, 1),
            SyncItem {
                key: b"b".to_vec(),
                value: SyncValue::Live(chain),
            },
            SyncItem {
                key: b"c".to_vec(),
                value: SyncValue::Live(large),
            },
            SyncItem {
                key: b"d".to_vec(),
                value: SyncValue::Live(hinted),
            },
            dead_item("e", 99),
            live_item("", 1, 1),
        ],
        next: Some(b"e".to_vec()),
    }
}

#[test]
fn an_encoded_page_decodes_to_the_same_page() {
    let page = rich_page();
    assert_eq!(decode_page(&encode_page(&page)).expect("decode"), page);
    let last = SyncPage {
        next: None,
        ..rich_page()
    };
    assert_eq!(decode_page(&encode_page(&last)).expect("decode"), last);
    let empty = SyncPage::default();
    assert_eq!(decode_page(&encode_page(&empty)).expect("decode"), empty);
}

#[test]
fn every_truncation_of_an_encoded_page_is_an_error() {
    for page in [rich_page(), SyncPage::default()] {
        let bytes = encode_page(&page);
        for len in 0..bytes.len() {
            assert!(
                matches!(decode_page(&bytes[..len]), Err(IndexError::Store { .. })),
                "length {len} of {}",
                bytes.len()
            );
        }
    }
}

#[test]
fn a_bad_version_byte_trailing_bytes_and_bad_tags_are_errors() {
    let bytes = encode_page(&rich_page());
    let mut bad = bytes.clone();
    bad[0] ^= 0xff;
    assert!(matches!(decode_page(&bad), Err(IndexError::Store { .. })));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(
        decode_page(&trailing),
        Err(IndexError::Store { .. })
    ));
    // The next flag is the byte before the 2-byte length and the 1-byte cursor "e".
    let flag = bytes.len() - 4;
    let mut bad_flag = bytes.clone();
    bad_flag[flag] = 7;
    assert!(matches!(
        decode_page(&bad_flag),
        Err(IndexError::Store { .. })
    ));
    // A page of one deleted item: version, count (4), key length (2), key, value length (4), value.
    let one = encode_page(&SyncPage {
        items: vec![dead_item("k", 5)],
        next: None,
    });
    let tag = 1 + 4 + 2 + 1 + 4 + 1;
    let mut bad_tag = one.clone();
    bad_tag[tag] = 0xee;
    assert!(matches!(
        decode_page(&bad_tag),
        Err(IndexError::Store { .. })
    ));
}
