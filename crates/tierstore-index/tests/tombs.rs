//! The tombstone table: what a tombstone names, how it grows, and the drop rule (10.2).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fmt::Debug;

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, Generation, KeyHash, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LargeFile, LogEntry, LogRecord, Named, TombDrop, Tombstone};
use tierstore_format::trailer::TrailerEntry;
use tierstore_index::backend::mem::Call;
use tierstore_index::backend::{Backend, MemBackend, Op, Space};
use tierstore_index::tombs::TombInfo;
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_store_api::index::{
    DeleteOutcome, EntryLocation, Fact, Index, IndexEntry, IndexError, Lookup,
};

type Ix = KvIndex<MemBackend>;

const ME: NodeId = NodeId(1);
const PEER: NodeId = NodeId(2);
const OTHER: NodeId = NodeId(3);

fn cfg() -> IndexConfig {
    IndexConfig {
        node: ME,
        geometry: BlockGeometry::DEFAULT,
    }
}

fn index() -> Ix {
    let ix = KvIndex::open(MemBackend::new(), cfg()).expect("a new store opens");
    ix.set_ready(true);
    ix
}

fn blk(n: u64) -> BlockId {
    BlockId::new(ME, n).expect("a counter that fits")
}

fn st(n: u64) -> Stamp {
    Stamp::from_raw(n)
}

fn gn(b: u8) -> Generation {
    Generation([b; 16])
}

fn at(block: u64) -> EntryLocation {
    EntryLocation::Block {
        block: blk(block),
        offset: 0,
    }
}

fn entry(location: EntryLocation, stamp: u64) -> IndexEntry {
    IndexEntry {
        generation: gn(1),
        location,
        length: 100,
        stamp: st(stamp),
        flags: RecordFlags(0),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

fn record(key: &[u8], location: EntryLocation, stamp: u64) -> Fact {
    Fact::Record {
        key: key.to_vec(),
        entry: entry(location, stamp),
    }
}

fn replay(ix: &Ix, facts: Vec<Fact>) {
    ix.replay(&mut facts.into_iter()).expect("replay");
}

fn tombstone(key: &[u8], stamp: u64, names: Vec<Named>) -> Tombstone {
    Tombstone {
        key: key.to_vec(),
        generation: gn(1),
        stamp: st(stamp),
        names,
    }
}

fn log(origin: NodeId, seq: u64, entry: LogEntry) -> Fact {
    Fact::Log(LogRecord { origin, seq, entry })
}

fn tomb_at(origin: NodeId, seq: u64, key: &[u8], stamp: u64, names: Vec<Named>) -> Fact {
    log(
        origin,
        seq,
        LogEntry::Tombstone(tombstone(key, stamp, names)),
    )
}

fn report(from: NodeId, seq: u64, target: NodeId, target_seq: u64) -> Fact {
    log(
        from,
        seq,
        LogEntry::TombDrop(TombDrop {
            target_origin: target,
            target_seq,
        }),
    )
}

fn nb(n: u64) -> Named {
    Named::Block(blk(n))
}

fn large_file() -> LargeFile {
    LargeFile {
        key_hash: KeyHash([6; 16]),
        generation: gn(1),
        size: 9_000_000_000,
    }
}

fn trailer_entry(key: &[u8], stamp: u64) -> TrailerEntry {
    TrailerEntry {
        key: key.to_vec(),
        stamp: st(stamp),
        offset: 0,
        length: 100,
        generation: gn(1),
        crc32c: 0,
        flags: RecordFlags(0),
    }
}

fn tombs(ix: &Ix) -> Vec<TombInfo> {
    ix.tombstones().expect("tombstones")
}

fn names_of_only(ix: &Ix) -> Vec<Named> {
    let t = tombs(ix);
    assert_eq!(t.len(), 1, "one tombstone: {t:?}");
    t[0].names.clone()
}

/// Runs `op` on a fresh index prepared by `setup`, failing the `after`-th call of kind `call`,
/// for every `after` up to the op's last call of that kind; each failure must surface as a
/// store error.
fn every_failing_call_surfaces<T: Debug>(
    call: Call,
    setup: impl Fn(&Ix),
    op: impl Fn(&Ix) -> Result<T, IndexError>,
) {
    for after in 0..64 {
        let ix = index();
        setup(&ix);
        ix.backend().fail_next(call, after, 1);
        match op(&ix) {
            Err(IndexError::Store { reason, .. }) => {
                assert!(reason.contains("injected failure"), "{reason}");
            }
            Err(other) => panic!("not a store error: {other:?}"),
            // The operation has no call of this kind, or no more of them.
            Ok(_) => return,
        }
    }
    panic!("the operation never finished");
}

#[test]
fn delete_records_a_tombstone_naming_the_tombstone_names_the_entry_block_and_prior_blocks() {
    let ix = index();
    let mut e = entry(at(5), 10);
    e.prior_blocks = vec![blk(3)];
    assert!(ix.put_if_absent(b"k", e.clone()).is_ok());
    let t = tombstone(b"k", 20, vec![nb(9)]);
    assert_eq!(
        ix.delete(&t).expect("delete"),
        DeleteOutcome::Deleted(e.clone())
    );
    assert_eq!(
        tombs(&ix),
        vec![TombInfo {
            key: b"k".to_vec(),
            stamp: st(20),
            generation: gn(1),
            names: vec![nb(3), nb(5), nb(9)],
            position: None,
            reports: Vec::new(),
        }]
    );
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Deleted(st(20)));
}

#[test]
fn delete_of_a_chain_names_every_block_of_the_chain() {
    let ix = index();
    let size = 2 * BlockGeometry::DEFAULT.max_object() + 1;
    let mut e = entry(EntryLocation::Chain { first: blk(10) }, 10);
    e.length = size;
    assert!(ix.put_if_absent(b"chain", e).is_ok());
    ix.delete(&tombstone(b"chain", 20, Vec::new()))
        .expect("delete");
    assert_eq!(names_of_only(&ix), vec![nb(10), nb(11), nb(12)]);
}

#[test]
fn delete_of_a_large_file_names_the_large_file() {
    let ix = index();
    let f = large_file();
    assert!(
        ix.put_if_absent(b"big", entry(EntryLocation::Large(f), 10))
            .is_ok()
    );
    ix.delete(&tombstone(b"big", 20, Vec::new()))
        .expect("delete");
    assert_eq!(names_of_only(&ix), vec![Named::LargeFile(f)]);
}

#[test]
fn delete_of_an_absent_key_records_a_tombstone_with_only_the_given_names() {
    let ix = index();
    let out = ix
        .delete(&tombstone(b"nothing", 20, vec![nb(2), nb(1)]))
        .expect("delete");
    assert_eq!(out, DeleteOutcome::NoLiveEntry);
    assert_eq!(names_of_only(&ix), vec![nb(1), nb(2)]);
}

#[test]
fn the_same_key_and_stamp_twice_unions_the_names() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, vec![nb(1), nb(2)]))
        .expect("first");
    ix.delete(&tombstone(b"k", 20, vec![nb(2), nb(3)]))
        .expect("second");
    assert_eq!(names_of_only(&ix), vec![nb(1), nb(2), nb(3)]);
}

#[test]
fn a_tombstone_with_no_names_and_no_position_is_still_recorded_once() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, Vec::new())).expect("first");
    ix.delete(&tombstone(b"k", 20, Vec::new())).expect("again");
    assert_eq!(names_of_only(&ix), Vec::new());
}

#[test]
fn tombstones_of_one_key_are_listed_in_stamp_order_each_with_its_own_names() {
    let ix = index();
    ix.delete(&tombstone(b"k", 30, vec![nb(3)])).expect("30");
    ix.delete(&tombstone(b"k", 10, vec![nb(1)])).expect("10");
    ix.delete(&tombstone(b"a", 50, vec![nb(5)])).expect("a");
    let got: Vec<(Vec<u8>, Stamp, Vec<Named>)> = tombs(&ix)
        .into_iter()
        .map(|t| (t.key, t.stamp, t.names))
        .collect();
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), st(50), vec![nb(5)]),
            (b"k".to_vec(), st(10), vec![nb(1)]),
            (b"k".to_vec(), st(30), vec![nb(3)]),
        ]
    );
}

#[test]
fn a_logged_tombstone_sets_its_position_and_the_origins_cursor() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, vec![nb(1)])).expect("local");
    assert_eq!(tombs(&ix)[0].position, None);
    replay(&ix, vec![tomb_at(PEER, 7, b"k", 20, vec![nb(2)])]);
    let t = tombs(&ix);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].position, Some((PEER, 7)));
    assert_eq!(t[0].names, vec![nb(1), nb(2)]);
    assert_eq!(ix.tomb_cursor(PEER).expect("cursor"), 7);
}

#[test]
fn a_tombstone_keeps_the_first_position_it_got() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
    replay(&ix, vec![tomb_at(OTHER, 4, b"k", 20, vec![nb(2)])]);
    let t = tombs(&ix);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].position, Some((PEER, 1)));
    assert_eq!(t[0].names, vec![nb(1), nb(2)]);
}

#[test]
fn extend_tombstones_at_a_seal_names_the_block_only_for_keys_deleted_after_the_trailer_stamp() {
    let ix = index();
    ix.delete(&tombstone(b"above", 20, Vec::new())).expect("1");
    ix.delete(&tombstone(b"equal", 10, Vec::new())).expect("2");
    ix.delete(&tombstone(b"below", 5, Vec::new())).expect("3");
    let entries = [
        trailer_entry(b"above", 10),
        trailer_entry(b"equal", 10),
        trailer_entry(b"below", 10),
        trailer_entry(b"untouched", 10),
    ];
    assert_eq!(ix.extend_tombstones(blk(7), &entries).expect("extend"), 1);
    let names: Vec<(Vec<u8>, Vec<Named>)> =
        tombs(&ix).into_iter().map(|t| (t.key, t.names)).collect();
    assert_eq!(
        names,
        vec![
            (b"above".to_vec(), vec![nb(7)]),
            (b"below".to_vec(), Vec::new()),
            (b"equal".to_vec(), Vec::new()),
        ]
    );
    assert_eq!(
        ix.extend_tombstones(blk(7), &entries).expect("again"),
        0,
        "naming the same block again changes nothing"
    );
    assert_eq!(ix.extend_tombstones(blk(8), &[]).expect("empty"), 0);
}

#[test]
fn extend_tombstones_names_the_block_in_each_tombstone_of_a_key_above_the_stamp() {
    let ix = index();
    ix.delete(&tombstone(b"k", 5, Vec::new())).expect("5");
    ix.delete(&tombstone(b"k", 15, Vec::new())).expect("15");
    ix.delete(&tombstone(b"k", 25, Vec::new())).expect("25");
    ix.extend_tombstones(blk(4), &[trailer_entry(b"k", 10)])
        .expect("extend");
    let got: Vec<(Stamp, Vec<Named>)> =
        tombs(&ix).into_iter().map(|t| (t.stamp, t.names)).collect();
    assert_eq!(
        got,
        vec![
            (st(5), Vec::new()),
            (st(15), vec![nb(4)]),
            (st(25), vec![nb(4)])
        ]
    );
}

#[test]
fn extend_tombstone_checks_a_late_tombstone_against_the_record_stamp() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, Vec::new())).expect("delete");
    let names = [nb(2), Named::LargeFile(large_file())];
    assert!(!ix.extend_tombstone(b"k", st(20), &names).expect("equal"));
    assert!(!ix.extend_tombstone(b"k", st(30), &names).expect("newer"));
    assert!(!ix.extend_tombstone(b"other", st(1), &names).expect("key"));
    assert!(ix.extend_tombstone(b"k", st(10), &names).expect("older"));
    assert!(
        !ix.extend_tombstone(b"k", st(10), &names).expect("again"),
        "the same names are not new"
    );
    assert_eq!(
        names_of_only(&ix),
        vec![nb(2), Named::LargeFile(large_file())]
    );
}

#[test]
fn replaying_an_older_record_of_a_deleted_key_extends_the_tombstone_with_the_records_block() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, vec![nb(1)]))
        .expect("delete");
    replay(&ix, vec![record(b"k", at(8), 10)]);
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Deleted(st(20)));
    assert_eq!(names_of_only(&ix), vec![nb(1), nb(8)]);
}

#[test]
fn replaying_a_record_at_or_above_the_tombstone_stamp_does_not_extend_it() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, vec![nb(1)]))
        .expect("delete");
    replay(&ix, vec![record(b"k", at(8), 20), record(b"k", at(9), 30)]);
    assert_eq!(names_of_only(&ix), vec![nb(1)]);
    assert!(matches!(ix.get(b"k").expect("get"), Lookup::Live(_)));
}

#[test]
fn replaying_an_older_record_of_a_key_deleted_twice_extends_only_the_tombstones_above_it() {
    let ix = index();
    ix.delete(&tombstone(b"k", 10, Vec::new())).expect("10");
    ix.delete(&tombstone(b"k", 30, Vec::new())).expect("30");
    replay(&ix, vec![record(b"k", at(6), 20)]);
    let got: Vec<(Stamp, Vec<Named>)> =
        tombs(&ix).into_iter().map(|t| (t.stamp, t.names)).collect();
    assert_eq!(got, vec![(st(10), Vec::new()), (st(30), vec![nb(6)])]);
}

#[test]
fn a_freed_name_is_reported_freed_and_an_unknown_name_never_is() {
    let ix = index();
    let big = Named::LargeFile(large_file());
    let unknown = Named::Unknown {
        kind: 9,
        bytes: vec![1],
    };
    for n in [&nb(1), &big, &unknown] {
        assert!(!ix.is_freed(n).expect("before"));
    }
    for n in [&nb(1), &big, &unknown] {
        ix.apply_free(n).expect("free");
    }
    assert!(ix.is_freed(&nb(1)).expect("block"));
    assert!(ix.is_freed(&big).expect("large file"));
    assert!(
        !ix.is_freed(&unknown).expect("unknown"),
        "a name of a kind this release does not know is never freed"
    );
    assert!(!ix.is_freed(&nb(2)).expect("another block"));
}

#[test]
fn freeing_a_block_clears_its_live_count() {
    let ix = index();
    assert!(ix.put_if_absent(b"k", entry(at(5), 10)).is_ok());
    assert_eq!(ix.live(blk(5)).expect("live").entries, 1);
    ix.apply_free(&nb(5)).expect("free");
    assert_eq!(ix.live(blk(5)).expect("live").entries, 0);
}

fn pending(ix: &Ix) -> Vec<(NodeId, u64)> {
    ix.pending_reports().expect("pending")
}

#[test]
fn pending_reports_lists_a_positioned_tombstone_once_every_name_is_freed() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1), nb(2)])]);
    assert_eq!(pending(&ix), Vec::new());
    ix.apply_free(&nb(1)).expect("free 1");
    assert_eq!(pending(&ix), Vec::new());
    ix.apply_free(&nb(2)).expect("free 2");
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
    assert_eq!(pending(&ix), vec![(PEER, 1)], "asking twice gives the same");
}

#[test]
fn pending_reports_skips_a_tombstone_that_has_no_position() {
    let ix = index();
    ix.delete(&tombstone(b"k", 20, vec![nb(1)]))
        .expect("delete");
    ix.apply_free(&nb(1)).expect("free");
    assert_eq!(pending(&ix), Vec::new());
}

#[test]
fn pending_reports_lists_a_tombstone_with_no_names_at_once() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
}

#[test]
fn pending_reports_never_lists_a_tombstone_that_names_an_unknown_kind() {
    let ix = index();
    let unknown = Named::Unknown {
        kind: 9,
        bytes: vec![1, 2],
    };
    replay(
        &ix,
        vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1), unknown.clone()])],
    );
    ix.apply_free(&nb(1)).expect("free");
    ix.apply_free(&unknown).expect("free the unknown name");
    assert_eq!(pending(&ix), Vec::new());
}

#[test]
fn pending_reports_stops_listing_once_this_nodes_own_report_is_replayed() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
    ix.apply_free(&nb(1)).expect("free");
    replay(&ix, vec![report(OTHER, 1, PEER, 1)]);
    assert_eq!(
        pending(&ix),
        vec![(PEER, 1)],
        "another node's report is not this node's"
    );
    replay(&ix, vec![report(ME, 1, PEER, 1)]);
    assert_eq!(pending(&ix), Vec::new());
    assert_eq!(tombs(&ix)[0].reports, vec![ME, OTHER]);
}

#[test]
fn reports_that_arrive_before_the_tombstone_are_kept_for_it() {
    let ix = index();
    replay(&ix, vec![report(ME, 1, PEER, 5), report(OTHER, 1, PEER, 5)]);
    assert_eq!(tombs(&ix), Vec::new());
    assert!(
        !ix.drop_tombstone(PEER, 5).expect("drop"),
        "no tombstone is there yet"
    );
    replay(&ix, vec![tomb_at(PEER, 5, b"k", 20, Vec::new())]);
    let t = tombs(&ix);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].reports, vec![ME, OTHER]);
    assert_eq!(pending(&ix), Vec::new(), "this node reported already");
    assert_eq!(
        ix.droppable(&[ME, OTHER]).expect("droppable"),
        vec![(PEER, 5)]
    );
}

#[test]
fn a_repeated_report_changes_nothing() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    replay(&ix, vec![report(OTHER, 1, PEER, 1)]);
    let before = ix.backend().dump();
    replay(&ix, vec![report(OTHER, 2, PEER, 1)]);
    let after: Vec<_> = ix
        .backend()
        .dump()
        .into_iter()
        .filter(|(s, _, _)| *s == Space::TombPos)
        .collect();
    let before: Vec<_> = before
        .into_iter()
        .filter(|(s, _, _)| *s == Space::TombPos)
        .collect();
    assert_eq!(after, before);
}

#[test]
fn droppable_needs_a_report_from_every_node_not_out() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    let both = [ME, PEER];
    assert_eq!(ix.droppable(&both).expect("none"), Vec::new());
    replay(&ix, vec![report(ME, 1, PEER, 1)]);
    assert_eq!(ix.droppable(&both).expect("one"), Vec::new());
    assert_eq!(
        ix.droppable(&[ME]).expect("the other is out"),
        vec![(PEER, 1)],
        "a node that is out is not waited for"
    );
    replay(&ix, vec![report(PEER, 2, PEER, 1)]);
    assert_eq!(ix.droppable(&both).expect("both"), vec![(PEER, 1)]);
}

#[test]
fn droppable_with_no_node_not_out_lists_every_tombstone_that_has_a_position() {
    let ix = index();
    replay(
        &ix,
        vec![
            tomb_at(PEER, 1, b"a", 20, Vec::new()),
            report(ME, 1, PEER, 9),
        ],
    );
    assert_eq!(ix.droppable(&[]).expect("droppable"), vec![(PEER, 1)]);
}

#[test]
fn droppable_pages_through_every_position_in_order() {
    for count in [1023u64, 1024, 1025, 2048, 2500] {
        let ix = index();
        let mut facts = Vec::new();
        for seq in 1..=count {
            facts.push(tomb_at(
                PEER,
                seq,
                format!("key-{seq}").as_bytes(),
                20,
                Vec::new(),
            ));
        }
        replay(&ix, facts);
        let got = ix.droppable(&[]).expect("droppable");
        let want: Vec<(NodeId, u64)> = (1..=count).map(|s| (PEER, s)).collect();
        assert_eq!(got.len(), want.len(), "count {count}");
        assert_eq!(got, want, "count {count}");
    }
}

#[test]
fn drop_tombstone_removes_the_tombstone_its_position_and_the_keys_tombstone_entry() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Deleted(st(20)));
    assert!(ix.drop_tombstone(PEER, 1).expect("drop"));
    assert_eq!(tombs(&ix), Vec::new());
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Absent);
    assert_eq!(ix.droppable(&[]).expect("droppable"), Vec::new());
    assert!(
        ix.backend()
            .dump()
            .iter()
            .all(|(s, _, _)| !matches!(s, Space::Tombs | Space::TombPos | Space::Entries)),
        "nothing of the tombstone is left in the store"
    );
    assert!(!ix.drop_tombstone(PEER, 1).expect("again"));
}

#[test]
fn drop_tombstone_of_an_unknown_position_returns_false_and_changes_nothing() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
    let before = ix.backend().dump();
    assert!(!ix.drop_tombstone(PEER, 2).expect("unknown seq"));
    assert!(!ix.drop_tombstone(OTHER, 1).expect("unknown origin"));
    assert_eq!(ix.backend().dump(), before);
}

#[test]
fn drop_tombstone_keeps_a_newer_tombstone_entry_of_the_key() {
    let ix = index();
    replay(
        &ix,
        vec![
            tomb_at(PEER, 1, b"k", 20, vec![nb(1)]),
            tomb_at(PEER, 2, b"k", 30, vec![nb(2)]),
        ],
    );
    assert!(ix.drop_tombstone(PEER, 1).expect("drop the older"));
    let t = tombs(&ix);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].stamp, st(30));
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Deleted(st(30)));
    assert!(ix.drop_tombstone(PEER, 2).expect("drop the newer"));
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Absent);
}

#[test]
fn drop_tombstone_keeps_a_live_entry_written_after_the_delete() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    assert!(ix.put_if_absent(b"k", entry(at(4), 25)).is_ok());
    assert!(ix.drop_tombstone(PEER, 1).expect("drop"));
    assert!(matches!(ix.get(b"k").expect("get"), Lookup::Live(_)));
}

#[test]
fn drop_tombstone_of_a_position_whose_row_is_gone_still_clears_the_position() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    ix.backend()
        .commit(vec![Op::Delete {
            space: Space::Tombs,
            key: b"k".to_vec(),
        }])
        .expect("remove the row");
    assert!(ix.drop_tombstone(PEER, 1).expect("drop"));
    assert_eq!(ix.droppable(&[]).expect("droppable"), Vec::new());
}

#[test]
fn a_key_relocated_then_deleted_names_both_blocks_and_is_reported_when_both_are_freed() {
    let ix = index();
    let e = entry(at(100), 10);
    assert!(ix.put_if_absent(b"k", e.clone()).is_ok());
    assert!(ix.relocate(b"k", gn(1), st(10), at(200)).expect("relocate"));
    ix.delete(&tombstone(b"k", 20, Vec::new())).expect("delete");
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    assert_eq!(names_of_only(&ix), vec![nb(100), nb(200)]);
    assert_eq!(pending(&ix), Vec::new());
    ix.apply_free(&nb(200)).expect("free Y");
    assert_eq!(pending(&ix), Vec::new(), "X is not freed yet");
    ix.apply_free(&nb(100)).expect("free X");
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
}

#[test]
fn a_node_returning_with_an_old_copy_extends_the_tombstone_and_holds_the_drop() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
    ix.apply_free(&nb(1)).expect("free");
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
    // The trailer of a block this node holds lists an older record of the key.
    assert_eq!(
        ix.extend_tombstones(blk(50), &[trailer_entry(b"k", 10)])
            .expect("extend"),
        1
    );
    assert_eq!(pending(&ix), Vec::new(), "the new name is not freed");
    ix.apply_free(&nb(50)).expect("free the returned block");
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
}

#[test]
fn a_node_returning_with_an_old_record_in_a_replay_holds_the_drop() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
    ix.apply_free(&nb(1)).expect("free");
    replay(&ix, vec![record(b"k", at(60), 10)]);
    assert_eq!(pending(&ix), Vec::new());
    ix.apply_free(&nb(60)).expect("free");
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
}

#[test]
fn failing_store_calls_surface_as_store_errors_in_every_tombstone_operation() {
    let with_tomb = |ix: &Ix| {
        replay(ix, vec![tomb_at(PEER, 1, b"k", 20, vec![nb(1)])]);
        replay(ix, vec![report(ME, 1, PEER, 1)]);
    };
    for call in [Call::Get, Call::Scan, Call::Commit] {
        every_failing_call_surfaces(call, with_tomb, |ix| {
            ix.extend_tombstones(blk(7), &[trailer_entry(b"k", 10)])
        });
        every_failing_call_surfaces(call, with_tomb, |ix| {
            ix.extend_tombstone(b"k", st(10), &[nb(7)])
        });
        every_failing_call_surfaces(call, with_tomb, |ix| {
            ix.delete(&tombstone(b"k", 40, vec![nb(2)]))
        });
        every_failing_call_surfaces(call, with_tomb, |ix| ix.apply_free(&nb(3)));
        every_failing_call_surfaces(call, with_tomb, |ix| ix.is_freed(&nb(3)));
        every_failing_call_surfaces(call, with_tomb, |ix| ix.tombstones());
        every_failing_call_surfaces(call, with_tomb, |ix| ix.pending_reports());
        every_failing_call_surfaces(call, with_tomb, |ix| ix.droppable(&[ME]));
        every_failing_call_surfaces(call, with_tomb, |ix| ix.drop_tombstone(PEER, 1));
        every_failing_call_surfaces(call, with_tomb, |ix| {
            ix.replay(&mut vec![tomb_at(PEER, 2, b"j", 5, Vec::new())].into_iter())
        });
        every_failing_call_surfaces(call, with_tomb, |ix| {
            ix.replay(&mut vec![report(OTHER, 1, PEER, 1)].into_iter())
        });
    }
}

#[test]
fn a_failing_pending_report_scan_still_lists_nothing_wrong_after_the_store_recovers() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    ix.backend().fail_next(Call::Get, 0, 1);
    assert!(ix.pending_reports().is_err());
    assert_eq!(pending(&ix), vec![(PEER, 1)]);
}

fn put_raw(ix: &Ix, space: Space, key: &[u8], value: &[u8]) {
    ix.backend()
        .commit(vec![Op::Put {
            space,
            key: key.to_vec(),
            value: value.to_vec(),
        }])
        .expect("raw put");
}

fn store_reason<T: Debug>(r: Result<T, IndexError>) -> (&'static str, String) {
    match r {
        Err(IndexError::Store { op, reason }) => (op, reason),
        other => panic!("expected a store error, got {other:?}"),
    }
}

#[test]
fn damaged_tombstone_rows_surface_as_store_errors_that_name_the_value() {
    let ix = index();
    put_raw(&ix, Space::Tombs, b"k", &[1, 1]);
    let (op, reason) = store_reason(ix.tombstones());
    assert_eq!(
        (op, reason.as_str()),
        ("decode tombstones", "value is truncated")
    );
    let (op, _) = store_reason(ix.extend_tombstone(b"k", st(1), &[nb(1)]));
    assert_eq!(op, "decode tombstones");
}

#[test]
fn a_damaged_position_row_surfaces_as_a_store_error_in_droppable_and_tombstones() {
    let ix = index();
    replay(&ix, vec![tomb_at(PEER, 1, b"k", 20, Vec::new())]);
    put_raw(&ix, Space::TombPos, &[PEER.0, 0, 0, 0, 0, 0, 0, 0, 1], &[1]);
    let (op, reason) = store_reason(ix.droppable(&[]));
    assert_eq!(
        (op, reason.as_str()),
        ("decode tomb position row", "value is truncated")
    );
    assert_eq!(store_reason(ix.tombstones()).0, "decode tomb position row");
}

#[test]
fn a_position_key_of_the_wrong_length_surfaces_as_a_store_error_in_droppable() {
    let ix = index();
    let mut row = vec![1u8, 1, 1, 0, 0, 0, b'x'];
    row.extend_from_slice(&1u64.to_le_bytes());
    row.extend_from_slice(&[0; 32]);
    put_raw(&ix, Space::TombPos, &[1, 2, 3], &row);
    let (op, reason) = store_reason(ix.droppable(&[]));
    assert_eq!(
        (op, reason.as_str()),
        ("decode tomb position", "key is not 9 bytes")
    );
}

/// A stored row of one tombstone at stamp 5 whose only name is `name_bytes`.
fn tomb_row_with_name_bytes(name_bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&5u64.to_le_bytes());
    v.extend_from_slice(&[1; 16]);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
    v.extend_from_slice(name_bytes);
    v.push(0);
    v
}

#[test]
fn a_stored_tombstone_whose_name_is_cut_short_or_has_extra_bytes_is_a_store_error() {
    let ix = index();
    put_raw(&ix, Space::Tombs, b"k", &tomb_row_with_name_bytes(&[1, 0]));
    let (op, reason) = store_reason(ix.tombstones());
    assert_eq!((op, reason.as_str()), ("decode name", "name is truncated"));

    let mut block_name = vec![1u8, 0, 8];
    block_name.extend_from_slice(&7u64.to_be_bytes());
    put_raw(
        &ix,
        Space::Tombs,
        b"k",
        &tomb_row_with_name_bytes(&block_name),
    );
    assert_eq!(ix.tombstones().expect("a good name").len(), 1);
    block_name.push(0);
    put_raw(
        &ix,
        Space::Tombs,
        b"k",
        &tomb_row_with_name_bytes(&block_name),
    );
    let (op, reason) = store_reason(ix.tombstones());
    assert_eq!(
        (op, reason.as_str()),
        ("decode tombstones", "name has trailing bytes")
    );
}
