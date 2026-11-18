//! Rebuild: the index made from hot trailers, `blk/*.idx` files and cold shard trailers, with the
//! tomb logs, the stamp rows of the liveness rule, drops, frees and the key-order bulk load.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod recover_support;

use std::sync::Arc;

use recover_support::*;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, NodeId, StripeId};
use tierstore_format::names::HotName;
use tierstore_format::record::RecordFlags;
use tierstore_format::tomb::{BlockDrop, LogEntry, LogRecord, Named, Tombstone};
use tierstore_format::trailer::BlockKind;
use tierstore_index::fs::Fs;
use tierstore_index::fs::fake::{FakeFs, FsOp, Torn};
use tierstore_index::recover::{Mode, OpenSegment};
use tierstore_store_api::index::{EntryLocation, Index, Lookup};

/// The workload of the rebuild tests: three sealed segments and an open one, a key deleted and put
/// again with the same bytes, and a key deleted.
const OPS: [Op; 10] = [
    Op::Put("a", "A1", 10),
    Op::Put("b", "B1", 11),
    Op::Put("c", "C1", 12),
    Op::Seal,
    Op::Delete("b", 20),
    Op::Put("d", "D1", 21),
    Op::Put("b", "B1", 22),
    Op::Seal,
    Op::Put("e", "E1", 30),
    Op::Seal,
];

const OPEN_OPS: [Op; 2] = [Op::Put("zz", "Z1", 31), Op::Delete("c", 32)];

fn workload() -> Node {
    let mut node = Node::new(false);
    for op in OPS.iter().chain(OPEN_OPS.iter()) {
        node.run(*op);
    }
    node
}

/// A copy of the node's files, as a crash leaves them (everything was synced).
fn files(node: &Node) -> FakeFs {
    node.fs.crash(Torn::Lost)
}

fn read_idx(fs: &FakeFs, b: BlockId) -> Vec<u8> {
    fs.bytes(&idx_path(b)).unwrap()
}

/// Takes block `b` off the hot disk, returning its bytes and its trailer.
fn take_block(fs: &FakeFs, b: BlockId) -> (Vec<u8>, Vec<u8>) {
    let out = (fs.bytes(&seg_path(b)).unwrap(), read_idx(fs, b));
    fs.remove(&seg_path(b)).unwrap();
    fs.remove(&idx_path(b)).unwrap();
    out
}

fn assert_matches_never_restarted(
    node: &Node,
    index: &tierstore_index::kv::KvIndex<impl tierstore_index::backend::Backend>,
) {
    assert_eq!(
        view(index, &KEYS, &blocks()),
        view(&node.reference(node.acked()), &KEYS, &blocks())
    );
}

fn open_scan(node: &Node) -> OpenSegment {
    let b = node.open_block();
    OpenSegment {
        block: b,
        valid_len: node.fs.len(&seg_path(b)).unwrap(),
        torn: false,
    }
}

#[test]
fn rebuild_from_hot_idx_files_equals_the_index_of_a_node_that_never_restarted() {
    let node = workload();
    let disks = Disks::hot_only(files(&node));
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.mode, Mode::Rebuild);
    assert_matches_never_restarted(&node, &index);
    assert_eq!(
        (report.idx_files, report.trailers, report.shard_trailers),
        (3, 0, 0)
    );
    assert_eq!(report.open_segments, vec![open_scan(&node)]);
    assert_eq!(report.log_entries, 2);
    // The record of "b" put again with the same bytes after its delete is the live one.
    let Lookup::Live(e) = index.get(b"b").unwrap() else {
        panic!("b is live");
    };
    assert_eq!(e.stamp, stamp(22));
    assert!(matches!(e.location, EntryLocation::Block { block: b, .. } if b == block(2)));
    // The tombstone of "b" holds its position in the log of its origin.
    let t = index
        .tombstones()
        .unwrap()
        .into_iter()
        .find(|t| t.key == b"b")
        .unwrap();
    assert_eq!(t.position, Some((NODE, 1)));
}

#[test]
fn rebuild_from_segment_tails_when_the_idx_files_are_gone() {
    let node = workload();
    let fs = files(&node);
    for n in 1..=3 {
        fs.remove(&idx_path(block(n))).unwrap();
    }
    let disks = Disks::hot_only(fs);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_matches_never_restarted(&node, &index);
    assert_eq!(
        (report.idx_files, report.trailers, report.shard_trailers),
        (0, 3, 0)
    );
}

#[test]
fn rebuild_from_blk_idx_files_when_the_segments_are_gone() {
    let node = workload();
    let fs = files(&node);
    for n in 1..=3 {
        let (_, trailer) = take_block(&fs, block(n));
        fs.put(&blk_path(block(n)), &trailer);
    }
    let disks = Disks::hot_only(fs);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_matches_never_restarted(&node, &index);
    assert_eq!(
        (report.idx_files, report.trailers, report.shard_trailers),
        (3, 0, 0)
    );
}

/// The cold drives of the node: stripe 1 holds blocks 1 and 2 (data shards 0 and 1 on drives A
/// and B, parity on A); the hot copies of these blocks are removed.
fn cold_disks(node: &Node, with_descriptors: bool) -> (Disks, Arc<FakeFs>, Arc<FakeFs>) {
    let fs = files(node);
    let (b1, _) = take_block(&fs, block(1));
    let (b2, _) = take_block(&fs, block(2));
    let (b3, _) = take_block(&fs, block(3));
    let desc = stripe(
        1,
        &[(block(1), b1.len() as u64), (block(2), b2.len() as u64)],
        0,
    );
    if with_descriptors {
        fs.put(
            &HotName::Descriptor(desc.stripe).path(),
            &desc.encode().unwrap(),
        );
    }
    let a = Arc::new(FakeFs::new());
    let b = Arc::new(FakeFs::new());
    let (path, bytes) = shard_file(&desc, 0, &b1);
    a.put(&path, &bytes);
    let (path, bytes) = shard_file(&desc, 1, &b2);
    b.put(&path, &bytes);
    let (path, bytes) = shard_file(&desc, 2, &[9u8; 64]);
    a.put(&path, &bytes);
    // Block 3 is a copy.
    let (path, bytes) = copy_file(block(3), &b3);
    b.put(&path, &bytes);
    let disks = Disks::new(Arc::new(fs), vec![a.clone(), b.clone()]);
    (disks, a, b)
}

#[test]
fn rebuild_from_cold_shard_trailers_reads_one_footer_and_one_trailer_per_file() {
    let node = workload();
    let (disks, a, b) = cold_disks(&node, true);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_matches_never_restarted(&node, &index);
    assert_eq!(
        (report.idx_files, report.trailers, report.shard_trailers),
        (0, 0, 3)
    );
    // Drive A: shard 0 (a footer and a trailer); the parity shard is known from the descriptor and
    // is not read. Drive B: shard 1 and the copy of block 3, a footer and a trailer each.
    assert_eq!(a.calls(FsOp::Read), 2);
    assert_eq!(a.calls(FsOp::Len), 1);
    assert_eq!(b.calls(FsOp::Read), 4);
    assert_eq!(b.calls(FsOp::Len), 2);
    // The descriptor was put: the block table knows the stripe.
    let row = index
        .stripe(StripeId::new(NODE, 1).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row.descriptor.blocks.len(), 2);
}

#[test]
fn rebuild_from_cold_shard_trailers_without_descriptors_reads_the_footer_of_a_parity_shard() {
    let node = workload();
    let (disks, a, b) = cold_disks(&node, false);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_matches_never_restarted(&node, &index);
    assert_eq!(report.shard_trailers, 3);
    // Shard 0 (footer, trailer) and the parity shard (footer only, its footer names no block).
    assert_eq!(a.calls(FsOp::Read), 3);
    assert_eq!(b.calls(FsOp::Read), 4);
}

#[test]
fn each_block_is_read_once_from_the_first_source_that_has_it() {
    let node = workload();
    let fs = files(&node);
    // Block 1 stays on the hot disk and also has a blk file and a cold copy; block 2 has a blk file
    // and a cold copy; block 3 has a cold copy only.
    let (b1, t1) = (
        fs.bytes(&seg_path(block(1))).unwrap(),
        read_idx(&fs, block(1)),
    );
    let (b2, t2) = take_block(&fs, block(2));
    let (b3, _) = take_block(&fs, block(3));
    fs.put(&blk_path(block(1)), &t1);
    fs.put(&blk_path(block(2)), &t2);
    let cold = Arc::new(FakeFs::new());
    for (b, bytes) in [(block(1), &b1), (block(2), &b2), (block(3), &b3)] {
        let (path, file) = copy_file(b, bytes);
        cold.put(&path, &file);
    }
    let disks = Disks::new(Arc::new(fs), vec![cold.clone()]);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_matches_never_restarted(&node, &index);
    assert_eq!(
        (report.idx_files, report.trailers, report.shard_trailers),
        (2, 0, 1)
    );
    assert_eq!(
        cold.calls(FsOp::Read),
        2,
        "only block 3 is read from the cold drive"
    );
}

#[test]
fn a_blk_idx_file_that_is_not_a_trailer_leaves_its_block_to_the_cold_drives() {
    let node = workload();
    let fs = files(&node);
    let (b1, _) = take_block(&fs, block(1));
    fs.put(&blk_path(block(1)), b"torn");
    let cold = Arc::new(FakeFs::new());
    let (path, file) = copy_file(block(1), &b1);
    cold.put(&path, &file);
    let disks = Disks::new(Arc::new(fs), vec![cold]);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_matches_never_restarted(&node, &index);
    assert_eq!((report.idx_files, report.shard_trailers), (2, 1));
}

#[test]
fn a_rebuild_feeds_the_index_one_replay_in_key_order() {
    let node = workload();
    let disks = Disks::hot_only(files(&node));
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    // Every record of the trailers and of the open segment, then the two log entries: 3 + 3 + 1
    // records of the sealed segments (a, b, c; d, b; e), one of the open one (zz), 2 log entries.
    assert_eq!(report.facts, 6 + 1 + 2);
    let keys: Vec<Vec<u8>> = index
        .list(b"", None, usize::MAX)
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        keys,
        vec![
            b"a".to_vec(),
            b"b".to_vec(),
            b"d".to_vec(),
            b"e".to_vec(),
            b"zz".to_vec()
        ]
    );
}

#[test]
fn a_delete_served_by_another_node_while_this_one_was_down_holds_on_both_after_rebuild() {
    // Row (a): K is put on node A; node B serves the DELETE with a stamp above while A is down.
    let mut a = Node::new(false);
    a.put(b"k", b"v1", 10);
    a.seal();
    let generation = gen_of(b"v1");
    let b_log = LogRecord {
        origin: NodeId(2),
        seq: 1,
        entry: LogEntry::Tombstone(Tombstone {
            key: b"k".to_vec(),
            generation,
            stamp: stamp(20),
            names: vec![Named::Block(block(1))],
        }),
    };
    // A, restarted, has pulled B's log entry; its rebuild reads the trailer and the log.
    let disks_a = Disks::hot_only(files(&a));
    disks_a.tomb.store(std::slice::from_ref(&b_log)).unwrap();
    let index_a = index_on(tierstore_index::backend::MemBackend::new());
    disks_a.recover(&index_a).unwrap();
    assert_eq!(index_a.get(b"k").unwrap(), Lookup::Deleted(stamp(20)));
    // B holds the tombstone in its own log and no record of K.
    let fs_b = FakeFs::new();
    fs_b.put(
        &HotName::TombLog {
            origin: NodeId(2),
            first_seq: 1,
        }
        .path(),
        &b_log.encode().unwrap(),
    );
    let disks_b = Disks::hot_only(fs_b);
    let index_b = index_on(tierstore_index::backend::MemBackend::new());
    disks_b.recover(&index_b).unwrap();
    assert_eq!(index_b.get(b"k").unwrap(), Lookup::Deleted(stamp(20)));
    assert_eq!(
        index_a.tombstones().unwrap()[0].position,
        Some((NodeId(2), 1))
    );
}

#[test]
fn a_key_deleted_and_put_again_with_the_same_bytes_is_live_at_the_new_record_after_rebuild() {
    // Row (b): the same generation, a newer stamp.
    let mut node = Node::new(false);
    node.put(b"k", b"same", 10);
    node.seal();
    node.delete(b"k", 20);
    node.put(b"k", b"same", 30);
    let disks = Disks::hot_only(files(&node));
    let index = index_on(tierstore_index::backend::MemBackend::new());
    disks.recover(&index).unwrap();
    let Lookup::Live(e) = index.get(b"k").unwrap() else {
        panic!("k is live");
    };
    assert_eq!(e.stamp, stamp(30));
    assert_eq!(e.generation, gen_of(b"same"));
    assert_eq!(
        e.location,
        EntryLocation::Block {
            block: block(2),
            offset: 0
        }
    );
    assert_eq!(
        view(&index, &[b"k"], &blocks()),
        view(&node.reference(node.acked()), &[b"k"], &blocks())
    );
}

#[test]
fn a_rebuild_does_not_resurrect_the_keys_of_a_dropped_block() {
    // Hot-only mode: block 1 was dropped (its drop is in the log); its keys are definite misses.
    let node = workload();
    let disks = Disks::hot_only(files(&node));
    disks
        .tomb
        .append(LogEntry::BlockDrop(BlockDrop {
            block: block(1),
            stamp: stamp(40),
        }))
        .unwrap();
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.skipped_dropped, 1);
    assert_eq!(
        report.idx_files, 2,
        "the trailer of the dropped block is not read"
    );
    assert_eq!(index.get(b"a").unwrap(), Lookup::Absent);
    assert_eq!(index.get(b"c").unwrap(), Lookup::Deleted(stamp(32)));
    assert!(matches!(index.get(b"d").unwrap(), Lookup::Live(_)));
    assert_eq!(
        index.block(block(1)).unwrap().unwrap().state,
        tierstore_index::blocks::BlockState::Dropped
    );
}

fn two_stripes(freed: bool, freed_without_descriptor: bool) -> (Disks, Arc<FakeFs>) {
    let built: Vec<Built> = (1..=4u64)
        .map(|n| {
            let key: &'static [u8] = [b"k1", b"k2", b"k3", b"k4"][(n - 1) as usize];
            build(BlockKind::Segment, &[(key, b"v", n, RecordFlags(0))])
        })
        .collect();
    let hot = FakeFs::new();
    let cold = Arc::new(FakeFs::new());
    let s1 = stripe(1, &[(block(1), 100), (block(2), 100)], 0);
    let s2 = stripe(2, &[(block(3), 100), (block(4), 100)], 0);
    for (s, first) in [(&s1, 0usize), (&s2, 2)] {
        hot.put(&HotName::Descriptor(s.stripe).path(), &s.encode().unwrap());
        for i in 0..2u8 {
            let (path, bytes) = shard_file(s, i, &built[first + i as usize].bytes);
            cold.put(&path, &bytes);
        }
    }
    if freed {
        let free = StripeFree {
            stripe: s1.stripe,
            revision: 0,
            owner: NODE,
            stamp: stamp(50),
        };
        hot.put(&HotName::Free(s1.stripe).path(), &free.encode());
    }
    if freed_without_descriptor {
        let free = StripeFree {
            stripe: StripeId::new(NODE, 9).unwrap(),
            revision: 0,
            owner: NODE,
            stamp: stamp(50),
        };
        hot.put(&HotName::Free(free.stripe).path(), &free.encode());
    }
    let disks = Disks::new(Arc::new(hot), vec![cold.clone()]);
    (disks, cold)
}

#[test]
fn a_rebuild_skips_the_blocks_of_a_freed_stripe() {
    let (disks, cold) = two_stripes(true, true);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.skipped_freed, 2);
    assert_eq!(report.shard_trailers, 2);
    assert_eq!(
        cold.calls(FsOp::Read),
        4,
        "a footer and a trailer for each shard of stripe 2"
    );
    assert_eq!(index.get(b"k1").unwrap(), Lookup::Absent);
    assert_eq!(index.get(b"k2").unwrap(), Lookup::Absent);
    assert!(matches!(index.get(b"k3").unwrap(), Lookup::Live(_)));
    assert!(matches!(index.get(b"k4").unwrap(), Lookup::Live(_)));
}

#[test]
fn a_rebuild_without_frees_reads_every_stripe() {
    let (disks, _) = two_stripes(false, false);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!((report.skipped_freed, report.shard_trailers), (0, 4));
}

#[test]
fn a_shard_file_of_a_stripe_without_descriptor_is_named_by_its_footer() {
    // A converted data shard keeps its copy footer: its name is a shard's, its footer a copy's.
    let built = build(BlockKind::Segment, &[(b"k", b"v", 1, RecordFlags(0))]);
    let desc = stripe(7, &[(block(1), 100)], 0);
    let cold = Arc::new(FakeFs::new());
    let (path, _) = shard_file(&desc, 0, b"");
    let (_, bytes) = copy_file(block(1), &built.bytes);
    cold.put(&path, &bytes);
    // The data shard of a block read already, found by its footer, is not read again.
    let read_first = build(BlockKind::Segment, &[(b"j", b"v", 2, RecordFlags(0))]);
    let desc2 = stripe(8, &[(block(2), 100)], 0);
    let (path2, bytes2) = shard_file(&desc2, 0, &read_first.bytes);
    cold.put(&path2, &bytes2);
    let hot = FakeFs::new();
    hot.put(&seg_path(block(2)), &read_first.bytes);
    let disks = Disks::new(Arc::new(hot), vec![cold.clone()]);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!((report.trailers, report.shard_trailers), (1, 1));
    assert!(matches!(index.get(b"k").unwrap(), Lookup::Live(_)));
    assert!(matches!(index.get(b"j").unwrap(), Lookup::Live(_)));
    // The file of block 2 cost a footer read and no trailer read.
    assert_eq!(cold.calls(FsOp::Read), 3);
}

#[test]
fn a_parity_footer_and_a_dropped_block_of_an_unknown_stripe_are_not_indexed() {
    let built = build(BlockKind::Segment, &[(b"k", b"v", 1, RecordFlags(0))]);
    let desc = stripe(7, &[(block(1), 100)], 0);
    let cold = Arc::new(FakeFs::new());
    let (path, bytes) = shard_file(&desc, 2, &[1u8; 32]);
    cold.put(&path, &bytes);
    let (path, bytes) = shard_file(&desc, 0, &built.bytes);
    cold.put(&path, &bytes);
    let hot = FakeFs::new();
    let disks = Disks::new(Arc::new(hot), vec![cold]);
    disks
        .tomb
        .append(LogEntry::BlockDrop(BlockDrop {
            block: block(1),
            stamp: stamp(9),
        }))
        .unwrap();
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.shard_trailers, 0);
    assert_eq!(report.skipped_dropped, 1);
    assert_eq!(index.get(b"k").unwrap(), Lookup::Absent);
}

#[test]
fn names_that_are_not_shard_or_copy_files_are_left_alone_on_a_cold_drive() {
    let cold = Arc::new(FakeFs::new());
    let desc = stripe(7, &[(block(1), 100)], 0);
    let up = tierstore_format::names::ColdName::ShardUpload {
        stripe: desc.stripe,
        index: 0,
        nonce: 5,
    }
    .path();
    cold.put(&up, b"partial");
    cold.put("s/aa/not-a-cold-name", b"x");
    cold.put("DRIVE", b"identity");
    let disks = Disks::new(Arc::new(FakeFs::new()), vec![cold.clone()]);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.shard_trailers, 0);
    assert_eq!(cold.calls(FsOp::Read), 0);
}

fn slice_trailer(key: &[u8], at: u64, len: u64) -> Vec<u8> {
    use tierstore_format::trailer::{Trailer, TrailerEntry};
    Trailer {
        kind: BlockKind::ChainSlice,
        entries: vec![TrailerEntry {
            key: key.to_vec(),
            stamp: stamp(at),
            offset: 0,
            length: len,
            generation: gen_of(b"chain"),
            crc32c: 0,
            flags: RecordFlags::CHAIN_SLICE,
        }],
        cell_crcs: vec![0],
        data_len: 4096,
        blake3: [0; 32],
    }
    .encode()
    .unwrap()
}

/// The bytes of a block whose records are 4096 unread bytes, with `trailer`.
fn slice_block(trailer: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 4096];
    bytes.extend_from_slice(trailer);
    bytes
}

fn chain_lengths() -> (u64, u64) {
    let max = geometry().max_object();
    (max, 2 * max + 1000)
}

#[test]
fn the_slices_of_a_chain_in_three_kinds_of_files_make_one_record_at_the_first_block() {
    let (max, total) = chain_lengths();
    let hot = FakeFs::new();
    hot.put(&blk_path(block(5)), &slice_trailer(b"big", 40, max));
    hot.put(
        &seg_path(block(6)),
        &slice_block(&slice_trailer(b"big", 40, max)),
    );
    let cold = Arc::new(FakeFs::new());
    let (path, bytes) = copy_file(block(7), &slice_block(&slice_trailer(b"big", 40, 1000)));
    cold.put(&path, &bytes);
    let disks = Disks::new(Arc::new(hot), vec![cold]);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.incomplete_chains, 0);
    let Lookup::Live(e) = index.get(b"big").unwrap() else {
        panic!("the chain is live");
    };
    assert_eq!(e.location, EntryLocation::Chain { first: block(5) });
    assert_eq!(e.length, total);
    assert_eq!(e.stamp, stamp(40));
    assert_eq!(e.generation, gen_of(b"chain"));
    assert_eq!(e.flags, RecordFlags(0));
    let live = |n| index.live(block(n)).unwrap();
    assert_eq!((live(5).entries, live(5).bytes), (1, max));
    assert_eq!((live(6).entries, live(6).bytes), (1, max));
    assert_eq!((live(7).entries, live(7).bytes), (1, 1000));
}

#[test]
fn a_chain_with_a_missing_wrong_or_scattered_slice_is_not_indexed() {
    let (max, _) = chain_lengths();
    let cases: [(&str, Vec<(u64, u64)>); 3] = [
        ("a missing slice", vec![(5, max), (7, 1000)]),
        (
            "a last slice of the wrong length",
            vec![(5, max), (6, max), (7, 999_999)],
        ),
        ("a chain of one slice", vec![(5, 1000)]),
    ];
    for (what, slices) in cases {
        let hot = FakeFs::new();
        for (n, len) in &slices {
            hot.put(&blk_path(block(*n)), &slice_trailer(b"big", 40, *len));
        }
        let disks = Disks::hot_only(hot);
        let index = index_on(tierstore_index::backend::MemBackend::new());
        let report = disks.recover(&index).unwrap();
        assert_eq!(report.incomplete_chains, 1, "{what}");
        assert_eq!(index.get(b"big").unwrap(), Lookup::Absent, "{what}");
    }
}

#[test]
fn a_chain_with_a_dropped_slice_block_is_not_indexed() {
    let (max, _) = chain_lengths();
    let hot = FakeFs::new();
    hot.put(&blk_path(block(5)), &slice_trailer(b"big", 40, max));
    hot.put(&blk_path(block(6)), &slice_trailer(b"big", 40, max));
    hot.put(&blk_path(block(7)), &slice_trailer(b"big", 40, 1000));
    let disks = Disks::hot_only(hot);
    disks
        .tomb
        .append(LogEntry::BlockDrop(BlockDrop {
            block: block(6),
            stamp: stamp(60),
        }))
        .unwrap();
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!((report.incomplete_chains, report.skipped_dropped), (1, 1));
    assert_eq!(index.get(b"big").unwrap(), Lookup::Absent);
}

#[test]
fn two_chains_of_one_key_are_both_assembled_and_the_newer_wins() {
    let (max, total) = chain_lengths();
    let hot = FakeFs::new();
    for (first, at) in [(5u64, 40u64), (10, 50)] {
        hot.put(&blk_path(block(first)), &slice_trailer(b"big", at, max));
        hot.put(&blk_path(block(first + 1)), &slice_trailer(b"big", at, max));
        hot.put(
            &blk_path(block(first + 2)),
            &slice_trailer(b"big", at, 1000),
        );
    }
    let disks = Disks::hot_only(hot);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.incomplete_chains, 0);
    let Lookup::Live(e) = index.get(b"big").unwrap() else {
        panic!("the chain is live");
    };
    assert_eq!(
        (e.stamp, e.location, e.length),
        (stamp(50), EntryLocation::Chain { first: block(10) }, total)
    );
}

/// A large file of `data` under `key`: its path and bytes (a record header, the key, the data).
fn large_file(key: &[u8], data: &[u8], at: u64, flags: RecordFlags) -> (String, Vec<u8>) {
    use tierstore_format::ids::KeyHash;
    use tierstore_format::record::{RECORD_HEADER_LEN, RecordHeader};
    use tierstore_format::tomb::LargeFile;
    let header = RecordHeader {
        flags,
        key_len: key.len() as u16,
        data_len: data.len() as u64,
        generation: gen_of(data),
        stamp: stamp(at),
        object_offset: 0,
        data_crc: tierstore_format::crc32c(data),
        key_crc: tierstore_format::crc32c(key),
    };
    let mut bytes = header.encode().to_vec();
    assert_eq!(bytes.len(), RECORD_HEADER_LEN);
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(data);
    let path = HotName::Large(LargeFile {
        key_hash: KeyHash::of(key),
        generation: gen_of(data),
        size: data.len() as u64,
    })
    .path();
    (path, bytes)
}

#[test]
fn a_pack_block_relocate_wins_over_the_large_file_record_at_the_same_stamp() {
    let data = b"large object bytes";
    let hot = FakeFs::new();
    let (path, bytes) = large_file(b"obj", data, 70, RecordFlags(0));
    hot.put(&path, &bytes);
    // The pack block holds the same record (same generation, same stamp) with the relocate flag.
    let pack = build(
        BlockKind::Pack,
        &[(b"obj", data, 70, RecordFlags::RELOCATE)],
    );
    hot.put(&blk_path(block(4)), &pack.trailer);
    let disks = Disks::hot_only(hot);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.large_files, 1);
    let Lookup::Live(e) = index.get(b"obj").unwrap() else {
        panic!("obj is live");
    };
    assert_eq!(
        e.location,
        EntryLocation::Block {
            block: block(4),
            offset: pack.offsets[0]
        }
    );
    assert!(e.flags.contains(RecordFlags::RELOCATE));
    assert_eq!((e.stamp, e.length), (stamp(70), data.len() as u64));
}

#[test]
fn without_the_pack_block_the_large_file_is_the_entry() {
    let data = b"large object bytes";
    let hot = FakeFs::new();
    let (path, bytes) = large_file(b"obj", data, 70, RecordFlags(0));
    hot.put(&path, &bytes);
    let disks = Disks::hot_only(hot);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    disks.recover(&index).unwrap();
    let Lookup::Live(e) = index.get(b"obj").unwrap() else {
        panic!("obj is live");
    };
    assert!(matches!(e.location, EntryLocation::Large(f) if f.size == data.len() as u64));
}

#[test]
fn a_name_in_blk_that_is_not_an_index_file_is_ignored() {
    let built = build(BlockKind::Segment, &[(b"k", b"v", 1, RecordFlags(0))]);
    let hot = FakeFs::new();
    hot.put("blk/readme", b"not an index file");
    hot.put(&blk_path(block(5)), &built.trailer);
    let disks = Disks::hot_only(hot);
    let index = index_on(tierstore_index::backend::MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.idx_files, 1);
    assert!(matches!(index.get(b"k").unwrap(), Lookup::Live(_)));
}
