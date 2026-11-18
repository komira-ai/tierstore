//! The block table: block rows, stripe descriptors and frees, and block drops (10.2, 10.5, 10.14).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fmt::Debug;

use tierstore_format::block::BlockGeometry;
use tierstore_format::descriptor::{Code, PlacedShard, ShardLoc, StripeDescriptor};
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, Generation, NodeId, StripeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{BlockDrop, LogEntry, LogRecord, Named, Tombstone};
use tierstore_index::backend::mem::Call;
use tierstore_index::backend::{Backend, MemBackend, Op, Space};
use tierstore_index::blocks::{BlockInfo, BlockState, StripePut, StripeRow};
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_store_api::index::{
    EntryLocation, Fact, Index, IndexEntry, IndexError, Lookup, PutOutcome,
};

type Ix = KvIndex<MemBackend>;

const ME: NodeId = NodeId(1);
const PEER: NodeId = NodeId(2);

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

/// The index a crash leaves with everything committed so far, opened again.
fn reopened(ix: &Ix) -> Ix {
    let again = KvIndex::open(ix.backend().crashed(usize::MAX), cfg()).expect("reopens");
    again.set_ready(true);
    again
}

fn blk(n: u64) -> BlockId {
    BlockId::new(ME, n).expect("a counter that fits")
}

fn sid(n: u64) -> StripeId {
    StripeId::new(ME, n).expect("a counter that fits")
}

fn st(n: u64) -> Stamp {
    Stamp::from_raw(n)
}

fn gn(b: u8) -> Generation {
    Generation([b; 16])
}

fn entry(location: EntryLocation, stamp: u64, length: u64) -> IndexEntry {
    IndexEntry {
        generation: gn(1),
        location,
        length,
        stamp: st(stamp),
        flags: RecordFlags(0),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

fn in_block(block: u64) -> EntryLocation {
    EntryLocation::Block {
        block: blk(block),
        offset: 0,
    }
}

fn info(state: BlockState) -> BlockInfo {
    BlockInfo::new(state)
}

fn loc(drive: u8) -> ShardLoc {
    ShardLoc {
        node: ME,
        drive: DriveId([drive; 16]),
    }
}

/// A 3+1 stripe of the given data blocks (each `(counter, length)`), data shards and the parity shard 3 stored.
fn desc(stripe: u64, revision: u32, blocks: &[(u64, u64)]) -> StripeDescriptor {
    StripeDescriptor {
        stripe: sid(stripe),
        revision,
        code: Code::Ec { k: 3, m: 1 },
        blocks: blocks.iter().map(|(b, l)| (blk(*b), *l)).collect(),
        shards: (0..blocks.len() as u8)
            .chain([3])
            .map(|index| PlacedShard {
                index,
                loc: loc(index),
            })
            .collect(),
        expiry: ExpiryClass(0),
        coordinator: ME,
    }
}

fn free_of(stripe: u64, revision: u32) -> StripeFree {
    StripeFree {
        stripe: sid(stripe),
        revision,
        owner: ME,
        stamp: st(500),
    }
}

fn drop_fact(seq: u64, block: u64) -> Fact {
    Fact::Log(LogRecord {
        origin: PEER,
        seq,
        entry: LogEntry::BlockDrop(BlockDrop {
            block: blk(block),
            stamp: st(1),
        }),
    })
}

fn replay(ix: &Ix, facts: Vec<Fact>) -> u64 {
    ix.replay(&mut facts.into_iter()).expect("replay").applied
}

fn row(ix: &Ix, block: u64) -> BlockInfo {
    ix.block(blk(block)).expect("block").expect("a row")
}

fn put(ix: &Ix, key: &[u8], location: EntryLocation, length: u64) {
    let out = ix
        .put_if_absent(key, entry(location, 10, length))
        .expect("put");
    assert_eq!(out, PutOutcome::Created);
}

fn store_reason<T: Debug>(r: Result<T, IndexError>) -> (&'static str, String) {
    match r {
        Err(IndexError::Store { op, reason }) => (op, reason),
        other => panic!("expected a store error, got {other:?}"),
    }
}

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

fn put_raw(ix: &Ix, space: Space, key: &[u8], value: &[u8]) {
    ix.backend()
        .commit(vec![Op::Put {
            space,
            key: key.to_vec(),
            value: value.to_vec(),
        }])
        .expect("raw put");
}

fn raw(ix: &Ix, space: Space, key: &[u8]) -> Vec<u8> {
    ix.backend()
        .get(space, key)
        .expect("get")
        .expect("a stored value")
}

#[test]
fn block_rows_round_trip_in_every_state_and_shape() {
    let ix = index();
    let states = [
        BlockState::Open,
        BlockState::Sealed,
        BlockState::Cold,
        BlockState::Freed,
        BlockState::Dropped,
    ];
    for (i, state) in states.into_iter().enumerate() {
        let n = i as u64 + 1;
        let shapes = [
            BlockInfo {
                state,
                length: 4096 * n,
                hot_copies: vec![ME, PEER, NodeId(255)],
                stripe: Some(sid(n)),
                shard: Some(2),
            },
            BlockInfo {
                state,
                length: 7,
                hot_copies: Vec::new(),
                stripe: Some(sid(n)),
                shard: None,
            },
            BlockInfo {
                state,
                length: 0,
                hot_copies: vec![PEER],
                stripe: None,
                shard: None,
            },
        ];
        for (j, shape) in shapes.into_iter().enumerate() {
            let id = n * 10 + j as u64;
            ix.put_block(blk(id), &shape).expect("put");
            assert_eq!(ix.block(blk(id)).expect("block"), Some(shape));
        }
    }
    assert_eq!(ix.block(blk(999)).expect("block"), None);
}

#[test]
fn a_new_block_row_has_only_its_state() {
    assert_eq!(
        BlockInfo::new(BlockState::Sealed),
        BlockInfo {
            state: BlockState::Sealed,
            length: 0,
            hot_copies: Vec::new(),
            stripe: None,
            shard: None,
        }
    );
}

#[test]
fn putting_a_block_row_again_replaces_it() {
    let ix = index();
    ix.put_block(blk(1), &info(BlockState::Open)).expect("open");
    let sealed = BlockInfo {
        length: 99,
        ..info(BlockState::Sealed)
    };
    ix.put_block(blk(1), &sealed).expect("sealed");
    assert_eq!(row(&ix, 1), sealed);
}

#[test]
fn blocks_pages_in_id_order_after_a_cursor_across_more_than_one_scan_page() {
    let ix = index();
    for n in 1..=2500u64 {
        let b = BlockInfo {
            length: n,
            ..info(BlockState::Sealed)
        };
        ix.put_block(blk(n), &b).expect("put");
    }
    let first = ix.blocks(None, 1024).expect("first page");
    assert_eq!(first.len(), 1024);
    assert_eq!(first[0].0, blk(1));
    assert_eq!(first[1023].0, blk(1024));
    assert_eq!(first[1023].1.length, 1024);

    let mut all = first;
    loop {
        let after = all.last().map(|(b, _)| *b);
        let page = ix.blocks(after, 1024).expect("page");
        if page.is_empty() {
            break;
        }
        all.extend(page);
    }
    let ids: Vec<u64> = all.iter().map(|(b, _)| b.counter()).collect();
    assert_eq!(ids, (1..=2500).collect::<Vec<u64>>());

    let tail = ix.blocks(Some(blk(2499)), 10).expect("tail");
    assert_eq!(
        tail.iter().map(|(b, _)| b.counter()).collect::<Vec<_>>(),
        vec![2500]
    );
    assert_eq!(ix.blocks(Some(blk(2500)), 10).expect("end"), Vec::new());
    assert_eq!(ix.blocks(None, 0).expect("none"), Vec::new());
    let mid = ix.blocks(Some(blk(7)), 3).expect("mid");
    assert_eq!(
        mid.iter().map(|(b, _)| b.counter()).collect::<Vec<_>>(),
        vec![8, 9, 10]
    );
}

#[test]
fn a_store_with_thousands_of_block_rows_reopens_and_finds_its_dropped_blocks() {
    let ix = index();
    for n in 1..=2100u64 {
        let state = if n == 2050 {
            BlockState::Dropped
        } else {
            BlockState::Sealed
        };
        ix.put_block(blk(n), &info(state)).expect("put");
    }
    put(&ix, b"late", in_block(2050), 5);
    put(&ix, b"early", in_block(3), 5);
    let again = reopened(&ix);
    assert_eq!(again.get(b"late").expect("get"), Lookup::Absent);
    assert!(matches!(again.get(b"early").expect("get"), Lookup::Live(_)));
}

#[test]
fn a_dropped_row_hides_its_keys_and_a_later_row_in_another_state_unhides_them() {
    let ix = index();
    let size = 2 * BlockGeometry::DEFAULT.max_object() + 1;
    put(&ix, b"a", in_block(5), 100);
    put(&ix, b"b", in_block(6), 100);
    put(&ix, b"c", EntryLocation::Chain { first: blk(20) }, size);
    ix.put_block(blk(5), &info(BlockState::Sealed))
        .expect("sealed");
    assert!(matches!(ix.get(b"a").expect("get"), Lookup::Live(_)));

    ix.put_block(blk(5), &info(BlockState::Dropped))
        .expect("drop");
    assert_eq!(ix.get(b"a").expect("get"), Lookup::Absent);
    assert!(matches!(ix.get(b"b").expect("get"), Lookup::Live(_)));
    let keys: Vec<Vec<u8>> = ix
        .list(b"", None, 10)
        .expect("list")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys, vec![b"b".to_vec(), b"c".to_vec()]);

    ix.put_block(blk(21), &info(BlockState::Dropped))
        .expect("drop a middle block of the chain");
    assert_eq!(ix.get(b"c").expect("get"), Lookup::Absent);

    ix.put_block(blk(5), &info(BlockState::Cold)).expect("cold");
    ix.put_block(blk(21), &info(BlockState::Sealed))
        .expect("sealed");
    assert!(matches!(ix.get(b"a").expect("get"), Lookup::Live(_)));
    assert!(matches!(ix.get(b"c").expect("get"), Lookup::Live(_)));
}

#[test]
fn a_large_file_is_not_hidden_by_a_block_drop() {
    use tierstore_format::ids::KeyHash;
    use tierstore_format::tomb::LargeFile;
    let ix = index();
    let f = LargeFile {
        key_hash: KeyHash([1; 16]),
        generation: gn(1),
        size: 1 << 33,
    };
    put(&ix, b"big", EntryLocation::Large(f), 1 << 33);
    ix.put_block(blk(1), &info(BlockState::Dropped))
        .expect("drop");
    assert!(matches!(ix.get(b"big").expect("get"), Lookup::Live(_)));
}

#[test]
fn a_key_in_a_dropped_block_can_be_put_again_as_if_absent() {
    let ix = index();
    put(&ix, b"k", in_block(5), 100);
    ix.put_block(blk(5), &info(BlockState::Dropped))
        .expect("drop");
    let again = entry(in_block(9), 20, 50);
    assert_eq!(
        ix.put_if_absent(b"k", again.clone()).expect("put"),
        PutOutcome::Created
    );
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Live(again));
}

#[test]
fn put_stripe_stores_then_reports_stale_for_the_same_or_a_lower_revision_then_stores_a_higher() {
    let ix = index();
    assert_eq!(
        ix.put_stripe(&desc(1, 3, &[(10, 100)])).expect("first"),
        StripePut::Stored
    );
    assert_eq!(
        ix.put_stripe(&desc(1, 3, &[(10, 100)])).expect("same"),
        StripePut::Stale { held: 3 }
    );
    assert_eq!(
        ix.put_stripe(&desc(1, 1, &[(10, 100)])).expect("lower"),
        StripePut::Stale { held: 3 }
    );
    assert_eq!(
        ix.stripe(sid(1))
            .expect("stripe")
            .map(|r| r.descriptor.revision),
        Some(3)
    );
    let newer = desc(1, 4, &[(10, 100), (11, 200)]);
    assert_eq!(ix.put_stripe(&newer).expect("higher"), StripePut::Stored);
    assert_eq!(
        ix.stripe(sid(1)).expect("stripe"),
        Some(StripeRow {
            descriptor: newer,
            free: None
        })
    );
    assert_eq!(ix.stripe(sid(2)).expect("other"), None);
}

#[test]
fn a_descriptor_marks_its_data_blocks_cold_with_stripe_length_and_shard_index() {
    let ix = index();
    ix.put_stripe(&desc(7, 0, &[(30, 111), (31, 222), (32, 333)]))
        .expect("put");
    for (i, (b, len)) in [(30u64, 111u64), (31, 222), (32, 333)].iter().enumerate() {
        assert_eq!(
            row(&ix, *b),
            BlockInfo {
                state: BlockState::Cold,
                length: *len,
                hot_copies: Vec::new(),
                stripe: Some(sid(7)),
                shard: Some(i as u8),
            }
        );
    }
}

#[test]
fn a_descriptor_keeps_the_hot_copies_of_a_block_row_that_was_sealed() {
    let ix = index();
    let sealed = BlockInfo {
        state: BlockState::Sealed,
        length: 5,
        hot_copies: vec![ME, PEER],
        stripe: None,
        shard: None,
    };
    ix.put_block(blk(30), &sealed).expect("sealed");
    ix.put_stripe(&desc(7, 0, &[(30, 111)])).expect("put");
    assert_eq!(
        row(&ix, 30),
        BlockInfo {
            state: BlockState::Cold,
            length: 111,
            hot_copies: vec![ME, PEER],
            stripe: Some(sid(7)),
            shard: Some(0),
        }
    );
}

#[test]
fn a_descriptor_that_does_not_encode_is_refused_and_leaves_the_table_unchanged() {
    let ix = index();
    let before = ix.backend().dump();
    let (op, reason) = store_reason(ix.put_stripe(&desc(7, 0, &[])));
    assert_eq!(op, "encode stripe row");
    assert!(reason.contains("block count"), "{reason}");
    assert_eq!(ix.backend().dump(), before);
}

#[test]
fn a_stripe_free_marks_its_blocks_freed_and_refuses_every_later_descriptor() {
    let ix = index();
    ix.put_stripe(&desc(7, 2, &[(30, 111), (31, 222)]))
        .expect("put");
    let f = free_of(7, 2);
    assert!(ix.apply_stripe_free(&f).expect("free"));
    for b in [30, 31] {
        let r = row(&ix, b);
        assert_eq!(r.state, BlockState::Freed);
        assert_eq!(r.stripe, Some(sid(7)), "the rest of the row is kept");
    }
    assert_eq!(
        ix.stripe(sid(7)).expect("stripe").map(|r| r.free),
        Some(Some(f))
    );
    for revision in [0, 2, 9] {
        assert_eq!(
            ix.put_stripe(&desc(7, revision, &[(30, 111)]))
                .expect("put"),
            StripePut::Freed
        );
    }
    assert_eq!(row(&ix, 30).state, BlockState::Freed);
}

#[test]
fn a_stripe_free_creates_the_row_of_a_data_block_the_table_never_saw() {
    let ix = index();
    ix.put_stripe(&desc(7, 0, &[(30, 111)])).expect("put");
    ix.backend()
        .commit(vec![Op::Delete {
            space: Space::Blocks,
            key: blk(30).raw().to_be_bytes().to_vec(),
        }])
        .expect("remove the row");
    assert!(ix.apply_stripe_free(&free_of(7, 0)).expect("free"));
    assert_eq!(row(&ix, 30), info(BlockState::Freed));
}

#[test]
fn a_free_of_an_unknown_stripe_returns_false_and_changes_nothing() {
    let ix = index();
    ix.put_stripe(&desc(7, 0, &[(30, 111)])).expect("put");
    let before = ix.backend().dump();
    assert!(!ix.apply_stripe_free(&free_of(8, 0)).expect("free"));
    assert_eq!(ix.backend().dump(), before);
    assert_eq!(
        ix.put_stripe(&desc(8, 0, &[(40, 1)])).expect("descriptor"),
        StripePut::Stored,
        "nothing was recorded for the stripe"
    );
}

#[test]
fn a_block_drop_of_a_block_with_no_row_makes_a_dropped_row_and_counts_as_its_free() {
    let ix = index();
    put(&ix, b"k", in_block(5), 100);
    assert_eq!(ix.live(blk(5)).expect("live").entries, 1);
    assert_eq!(replay(&ix, vec![drop_fact(1, 5)]), 1);
    assert_eq!(row(&ix, 5), info(BlockState::Dropped));
    assert_eq!(ix.get(b"k").expect("get"), Lookup::Absent);
    let freed = Named::Block(blk(5));
    assert!(ix.is_freed(&freed).expect("freed"));
    assert_eq!(ix.live(blk(5)).expect("live").entries, 0);
}

#[test]
fn a_block_drop_turns_a_row_of_any_state_into_dropped_and_keeps_its_other_fields() {
    for state in [
        BlockState::Open,
        BlockState::Sealed,
        BlockState::Cold,
        BlockState::Freed,
    ] {
        let ix = index();
        let before = BlockInfo {
            state,
            length: 4096,
            hot_copies: vec![ME, PEER],
            stripe: Some(sid(3)),
            shard: Some(1),
        };
        ix.put_block(blk(5), &before).expect("row");
        assert_eq!(replay(&ix, vec![drop_fact(1, 5)]), 1, "{state:?}");
        assert_eq!(
            row(&ix, 5),
            BlockInfo {
                state: BlockState::Dropped,
                ..before
            }
        );
    }
}

#[test]
fn a_block_drop_of_a_dropped_row_changes_nothing() {
    let ix = index();
    ix.put_block(blk(5), &info(BlockState::Dropped))
        .expect("row");
    assert_eq!(replay(&ix, vec![drop_fact(1, 5)]), 0);
    assert_eq!(row(&ix, 5), info(BlockState::Dropped));
    assert!(ix.is_freed(&Named::Block(blk(5))).expect("freed"));
}

#[test]
fn the_dropped_set_survives_a_reopen_and_a_later_row_still_unhides() {
    let ix = index();
    put(&ix, b"k", in_block(5), 100);
    replay(&ix, vec![drop_fact(1, 5)]);
    let again = reopened(&ix);
    assert_eq!(again.get(b"k").expect("get"), Lookup::Absent);
    assert_eq!(again.list(b"", None, 10).expect("list"), Vec::new());
    again
        .put_block(blk(5), &info(BlockState::Sealed))
        .expect("sealed");
    assert!(matches!(again.get(b"k").expect("get"), Lookup::Live(_)));
}

#[test]
fn a_block_drop_in_a_log_lets_a_tombstone_naming_the_block_be_reported() {
    let ix = index();
    let tomb = Fact::Log(LogRecord {
        origin: PEER,
        seq: 1,
        entry: LogEntry::Tombstone(Tombstone {
            key: b"k".to_vec(),
            generation: gn(1),
            stamp: st(20),
            names: vec![Named::Block(blk(5))],
        }),
    });
    replay(&ix, vec![tomb]);
    assert_eq!(ix.pending_reports().expect("pending"), Vec::new());
    replay(&ix, vec![drop_fact(2, 5)]);
    assert_eq!(ix.pending_reports().expect("pending"), vec![(PEER, 1)]);
}

#[test]
fn a_block_drop_that_fails_to_commit_leaves_the_block_visible() {
    let ix = index();
    put(&ix, b"k", in_block(5), 100);
    ix.backend().fail_next(Call::Commit, 0, 1);
    assert!(ix.replay(&mut vec![drop_fact(1, 5)].into_iter()).is_err());
    assert!(matches!(ix.get(b"k").expect("get"), Lookup::Live(_)));
}

#[test]
fn failing_store_calls_surface_as_store_errors_in_every_block_operation() {
    let setup = |ix: &Ix| {
        ix.put_block(blk(1), &info(BlockState::Sealed))
            .expect("row");
        ix.put_stripe(&desc(1, 0, &[(10, 5)])).expect("stripe");
    };
    for call in [Call::Get, Call::Scan, Call::Commit] {
        every_failing_call_surfaces(call, setup, |ix| {
            ix.put_block(blk(2), &info(BlockState::Open))
        });
        every_failing_call_surfaces(call, setup, |ix| ix.block(blk(1)));
        every_failing_call_surfaces(call, setup, |ix| ix.blocks(None, 10));
        every_failing_call_surfaces(call, setup, |ix| ix.put_stripe(&desc(2, 0, &[(20, 5)])));
        every_failing_call_surfaces(call, setup, |ix| ix.put_stripe(&desc(1, 1, &[(10, 5)])));
        every_failing_call_surfaces(call, setup, |ix| ix.stripe(sid(1)));
        every_failing_call_surfaces(call, setup, |ix| ix.apply_stripe_free(&free_of(1, 0)));
        every_failing_call_surfaces(call, setup, |ix| {
            ix.replay(&mut vec![drop_fact(1, 1)].into_iter())
        });
    }
}

#[test]
fn opening_a_store_surfaces_a_failing_scan_of_the_block_table() {
    let ix = index();
    for n in 1..=1100u64 {
        ix.put_block(blk(n), &info(BlockState::Sealed))
            .expect("put");
    }
    for after in 0..3 {
        let backend = ix.backend().crashed(usize::MAX);
        backend.fail_next(Call::Scan, after, 1);
        let err = KvIndex::open(backend, cfg()).expect_err("the scan fails");
        assert!(matches!(err, IndexError::Store { .. }), "{err:?}");
    }
}

fn block_row_bytes(ix: &Ix, block: u64) -> Vec<u8> {
    raw(ix, Space::Blocks, &blk(block).raw().to_be_bytes())
}

fn put_block_bytes(ix: &Ix, block: u64, bytes: &[u8]) {
    put_raw(ix, Space::Blocks, &blk(block).raw().to_be_bytes(), bytes);
}

#[test]
fn a_block_row_with_an_unknown_state_is_a_store_error() {
    let ix = index();
    ix.put_block(blk(1), &info(BlockState::Sealed))
        .expect("row");
    let mut bytes = block_row_bytes(&ix, 1);
    bytes[1] = 9;
    put_block_bytes(&ix, 1, &bytes);
    let (op, reason) = store_reason(ix.block(blk(1)));
    assert_eq!(
        (op, reason.as_str()),
        ("decode block row", "unknown block state")
    );
    assert_eq!(store_reason(ix.blocks(None, 10)).0, "decode block row");
}

#[test]
fn a_block_row_of_another_value_version_is_a_store_error() {
    let ix = index();
    ix.put_block(blk(1), &info(BlockState::Sealed))
        .expect("row");
    let mut bytes = block_row_bytes(&ix, 1);
    bytes[0] += 1;
    put_block_bytes(&ix, 1, &bytes);
    let (op, reason) = store_reason(ix.block(blk(1)));
    assert_eq!(
        (op, reason.as_str()),
        ("decode block row", "unknown value version")
    );
}

#[test]
fn a_block_row_with_an_unknown_stripe_tag_is_a_store_error() {
    let ix = index();
    ix.put_block(blk(1), &info(BlockState::Sealed))
        .expect("row");
    let mut bytes = block_row_bytes(&ix, 1);
    let last = bytes.len() - 1;
    bytes[last] = 9;
    put_block_bytes(&ix, 1, &bytes);
    assert_eq!(store_reason(ix.block(blk(1))).1, "unknown stripe tag");
}

#[test]
fn a_block_row_with_trailing_bytes_or_cut_short_is_a_store_error() {
    let ix = index();
    let full = BlockInfo {
        stripe: Some(sid(4)),
        shard: Some(1),
        hot_copies: vec![ME],
        ..info(BlockState::Cold)
    };
    ix.put_block(blk(1), &full).expect("row");
    let bytes = block_row_bytes(&ix, 1);
    let mut longer = bytes.clone();
    longer.push(0);
    put_block_bytes(&ix, 1, &longer);
    assert_eq!(store_reason(ix.block(blk(1))).1, "trailing bytes");
    for n in 0..bytes.len() {
        put_block_bytes(&ix, 1, &bytes[..n]);
        assert_eq!(
            store_reason(ix.block(blk(1))).1,
            "value is truncated",
            "cut at {n}"
        );
    }
}

#[test]
fn a_block_row_key_that_is_not_an_id_is_a_store_error_in_blocks() {
    let ix = index();
    let good = {
        ix.put_block(blk(1), &info(BlockState::Sealed))
            .expect("row");
        block_row_bytes(&ix, 1)
    };
    put_raw(&ix, Space::Blocks, &[0xff, 0xff, 0xff], &good);
    let (op, reason) = store_reason(ix.blocks(None, 10));
    assert_eq!(
        (op, reason.as_str()),
        ("decode id key", "key is not 8 bytes")
    );
}

fn stripe_bytes(ix: &Ix) -> Vec<u8> {
    raw(ix, Space::Stripes, &sid(1).raw().to_be_bytes())
}

fn put_stripe_bytes(ix: &Ix, bytes: &[u8]) {
    put_raw(ix, Space::Stripes, &sid(1).raw().to_be_bytes(), bytes);
}

#[test]
fn a_stripe_row_with_an_unknown_free_tag_is_a_store_error() {
    let ix = index();
    ix.put_stripe(&desc(1, 0, &[(10, 5)])).expect("put");
    let mut bytes = stripe_bytes(&ix);
    let last = bytes.len() - 1;
    bytes[last] = 9;
    put_stripe_bytes(&ix, &bytes);
    let (op, reason) = store_reason(ix.stripe(sid(1)));
    assert_eq!(
        (op, reason.as_str()),
        ("decode stripe row", "unknown free tag")
    );
    assert_eq!(
        store_reason(ix.put_stripe(&desc(1, 5, &[(10, 5)]))).0,
        "decode stripe row"
    );
    assert_eq!(
        store_reason(ix.apply_stripe_free(&free_of(1, 0))).0,
        "decode stripe row"
    );
}

#[test]
fn a_stripe_row_whose_descriptor_or_free_is_damaged_is_a_store_error() {
    let ix = index();
    ix.put_stripe(&desc(1, 0, &[(10, 5)])).expect("put");
    let good = stripe_bytes(&ix);
    let mut damaged = good.clone();
    damaged[10] ^= 0xff;
    put_stripe_bytes(&ix, &damaged);
    assert_eq!(store_reason(ix.stripe(sid(1))).0, "decode stripe row");

    put_stripe_bytes(&ix, &good);
    assert!(ix.apply_stripe_free(&free_of(1, 0)).expect("free"));
    let mut freed = stripe_bytes(&ix);
    let last = freed.len() - 1;
    freed[last] ^= 0xff;
    put_stripe_bytes(&ix, &freed);
    assert_eq!(store_reason(ix.stripe(sid(1))).0, "decode stripe row");
}

#[test]
fn a_stripe_row_with_trailing_bytes_or_cut_short_is_a_store_error() {
    let ix = index();
    ix.put_stripe(&desc(1, 0, &[(10, 5)])).expect("put");
    let bytes = stripe_bytes(&ix);
    let mut longer = bytes.clone();
    longer.push(0);
    put_stripe_bytes(&ix, &longer);
    assert_eq!(store_reason(ix.stripe(sid(1))).1, "trailing bytes");
    for n in 0..bytes.len() {
        put_stripe_bytes(&ix, &bytes[..n]);
        assert!(ix.stripe(sid(1)).is_err(), "cut at {n}");
    }
}
