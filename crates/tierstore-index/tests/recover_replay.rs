//! Replay from the mark: open segments from the marked length, the segments sealed since, the
//! tail of the tomb logs, and what is cut or skipped.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod recover_support;

use recover_support::*;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::tomb::{BlockDrop, LogEntry, LogRecord};
use tierstore_format::trailer::BlockKind;
use tierstore_index::blocks::{BlockInfo, BlockState};
use tierstore_index::fs::Fs;
use tierstore_index::fs::fake::{FakeFs, FsOp};
use tierstore_index::kv::KvIndex;
use tierstore_index::recover::{Mode, OpenSegment};
use tierstore_store_api::index::{Fact, Index, Lookup, ReplayMark};

/// An index whose store holds `mark` and nothing else, as a restart finds it.
fn marked<S: Stores>(s: &S, open: &[(BlockId, u64)]) -> KvIndex<S::B> {
    let index = s.fresh();
    let mark = ReplayMark {
        open_segments: open.to_vec(),
        tomb_cursors: Vec::new(),
    };
    index.flush_mark(&mark).unwrap();
    s.restart(index)
}

fn plain(key: &'static [u8], data: &'static [u8], at: u64) -> Rec<'static> {
    (key, data, at, RecordFlags(0))
}

/// Runs `ops`; at the first `Mark` remembers what the store holds. Returns the node, the index a
/// restart makes (the store as of that mark) and the operations acknowledged by then.
fn node_and_restart<S: Stores>(s: &S, ops: &[Op]) -> (Node, KvIndex<S::B>) {
    let mut node = Node::new(false);
    let mut restarted = None;
    for op in ops {
        node.run(*op);
        if matches!(op, Op::Mark) && restarted.is_none() {
            let index = s.fresh();
            node.apply_to(&index, 0, node.acked());
            index.flush_mark(&node.current_mark()).unwrap();
            restarted = Some(s.restart(index));
        }
    }
    (node, restarted.unwrap())
}

fn replay_gives_the_index_of_a_node_that_never_restarted<S: Stores>(s: &S) {
    let (node, index) = node_and_restart(
        s,
        &[
            Op::Put("a", "A1", 10),
            Op::Put("b", "B1", 11),
            Op::Mark,
            Op::Put("c", "C1", 12),
            Op::Delete("a", 20),
            Op::Seal,
            Op::Put("d", "D1", 21),
            Op::Put("e", "E1", 22),
            Op::Delete("e", 23),
        ],
    );
    let disks = Disks::new(node.fs.clone(), Vec::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.mode, Mode::Replay);
    assert_eq!(
        view(&index, &KEYS, &blocks()),
        view(&node.reference(node.acked()), &KEYS, &blocks())
    );
    // Segment 1 was sealed since the mark: its trailer, from its `.idx`. Segment 2 is open and is
    // not in the mark: scanned from 0.
    assert_eq!(report.idx_files, 1);
    assert_eq!(report.trailers, 0);
    assert_eq!(
        report.open_segments,
        vec![OpenSegment {
            block: block(2),
            valid_len: disks.hot.len(&seg_path(block(2))).unwrap(),
            torn: false
        }]
    );
    assert_eq!(report.log_entries, 2);
    assert_eq!(report.skipped_dropped + report.skipped_freed, 0);
    assert!(report.applied > 0 && report.applied <= report.facts);
}

fn replay_reads_an_open_segment_only_from_its_marked_length<S: Stores>(s: &S) {
    let mut node = Node::new(false);
    let keys: Vec<String> = (0..50).map(|i| format!("k{i:02}")).collect();
    for (i, k) in keys.iter().enumerate().take(40) {
        node.put(k.as_bytes(), format!("data-{i}").as_bytes(), 100 + i as u64);
    }
    let index = s.fresh();
    node.apply_to(&index, 0, 40);
    index.flush_mark(&node.current_mark()).unwrap();
    let index = s.restart(index);
    for (i, k) in keys.iter().enumerate().skip(40) {
        node.put(k.as_bytes(), format!("data-{i}").as_bytes(), 100 + i as u64);
    }
    let disks = Disks::new(node.fs.clone(), Vec::new());
    let report = disks.recover(&index).unwrap();
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes()).collect();
    assert_eq!(
        view(&index, &refs, &blocks()),
        view(&node.reference(50), &refs, &blocks())
    );
    assert_eq!(report.facts, 10, "only the records after the mark are read");
    assert_eq!(report.applied, 10);
}

fn a_deleted_key_stays_deleted_across_restart_and_seal<S: Stores>(s: &S) {
    let (node, index) = node_and_restart(
        s,
        &[
            Op::Put("a", "A1", 10),
            Op::Mark,
            Op::Delete("a", 20),
            Op::Seal,
            Op::Put("b", "B1", 30),
        ],
    );
    let disks = Disks::new(node.fs.clone(), Vec::new());
    disks.recover(&index).unwrap();
    assert_eq!(index.get(b"a").unwrap(), Lookup::Deleted(stamp(20)));
    assert!(matches!(index.get(b"b").unwrap(), Lookup::Live(_)));
    // Flush the recovered index with a new mark (segment 2 open, the log applied) and restart again:
    // the sealed segment holding the old record is not read, and the key is still deleted.
    let mark = ReplayMark {
        open_segments: vec![(block(2), 0)],
        tomb_cursors: index.tomb_cursors().unwrap(),
    };
    index.flush_mark(&mark).unwrap();
    let index = s.restart(index);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.idx_files + report.trailers, 0);
    assert_eq!(report.log_entries, 0);
    assert_eq!(index.get(b"a").unwrap(), Lookup::Deleted(stamp(20)));
    assert_eq!(
        view(&index, &KEYS, &blocks()),
        view(&node.reference(node.acked()), &KEYS, &blocks())
    );
}

fn a_torn_tail_is_cut_at_the_last_valid_record<S: Stores>(s: &S) {
    let recs = [
        plain(b"k1", b"one", 1),
        plain(b"k2", b"two", 2),
        plain(b"k3", b"three", 3),
        plain(
            b"k4",
            b"four four four four four four four four four four four four four four",
            4,
        ),
    ];
    let built = build(BlockKind::Segment, &recs);
    let records_len = (built.bytes.len() - built.trailer.len()) as u64;
    let end3 = built.offsets[3];
    for cut in 0..=(records_len - end3 - 1) {
        let hot = FakeFs::new();
        let path = seg_path(block(1));
        hot.put(&path, &built.bytes[..(end3 + cut) as usize]);
        let disks = Disks::hot_only(hot);
        let index = marked(s, &[(block(1), 0)]);
        let report = disks.recover(&index).unwrap();
        assert_eq!(
            report.open_segments,
            vec![OpenSegment {
                block: block(1),
                valid_len: end3,
                torn: cut > 0
            }],
            "cut {cut}"
        );
        assert_eq!(disks.hot.len(&path).unwrap(), end3, "cut {cut}");
        assert_eq!(report.facts, 3, "cut {cut}");
        for (i, k) in [b"k1", b"k2", b"k3"].iter().enumerate() {
            let Lookup::Live(e) = index.get(*k).unwrap() else {
                panic!("{k:?} not live at cut {cut}");
            };
            assert_eq!(
                e.location,
                entry(block(1), built.offsets[i], b"", 0).location
            );
        }
        assert_eq!(index.get(b"k4").unwrap(), Lookup::Absent, "cut {cut}");
    }
}

fn a_zero_filled_tail_of_a_segment_is_the_end_of_its_records<S: Stores>(s: &S) {
    let built = build(BlockKind::Segment, &[plain(b"k1", b"one", 1)]);
    let records_len = built.bytes.len() - built.trailer.len();
    let mut bytes = built.bytes[..records_len].to_vec();
    bytes.extend(vec![0u8; 500]);
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &bytes);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(
        report.open_segments,
        vec![OpenSegment {
            block: block(1),
            valid_len: records_len as u64,
            torn: false
        }]
    );
    assert_eq!(
        disks.hot.len(&seg_path(block(1))).unwrap(),
        bytes.len() as u64,
        "a preallocated zero tail is not cut"
    );
    assert!(matches!(index.get(b"k1").unwrap(), Lookup::Live(_)));
}

fn a_corrupt_record_in_the_middle_ends_the_valid_records<S: Stores>(s: &S) {
    let built = build(
        BlockKind::Segment,
        &[
            plain(b"k1", b"one", 1),
            plain(b"k2", b"two", 2),
            plain(b"k3", b"three", 3),
        ],
    );
    let mut bytes = built.bytes[..built.bytes.len() - built.trailer.len()].to_vec();
    // A flipped data byte of the second record: its data crc fails.
    let at = built.offsets[1] as usize + 64 + 2;
    bytes[at + 1] ^= 0xff;
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &bytes);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.open_segments[0].valid_len, built.offsets[1]);
    assert!(report.open_segments[0].torn);
    assert!(matches!(index.get(b"k1").unwrap(), Lookup::Live(_)));
    assert_eq!(index.get(b"k2").unwrap(), Lookup::Absent);
    assert_eq!(index.get(b"k3").unwrap(), Lookup::Absent);
}

fn a_mark_past_the_end_of_the_segment_finds_nothing_to_scan<S: Stores>(s: &S) {
    let built = build(BlockKind::Segment, &[plain(b"k1", b"one", 1)]);
    let records_len = (built.bytes.len() - built.trailer.len()) as u64;
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &built.bytes[..records_len as usize]);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), records_len + 1000)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.facts, 0);
    assert_eq!(
        report.open_segments,
        vec![OpenSegment {
            block: block(1),
            valid_len: records_len,
            torn: false
        }]
    );
}

fn replay_reads_the_segments_the_mark_does_not_cover_by_their_age<S: Stores>(s: &S) {
    let sealed = |k: &'static [u8], at: u64| build(BlockKind::Segment, &[plain(k, b"v", at)]);
    let hot = FakeFs::new();
    // Segment 2 was sealed before the mark was flushed: older than the open segment 5, not read.
    let old = sealed(b"old", 1);
    hot.put(&seg_path(block(2)), &old.bytes);
    hot.put(&idx_path(block(2)), &old.trailer);
    // Segment 5 is the open segment of the mark; it is still open.
    let open = build(BlockKind::Segment, &[plain(b"open", b"v", 2)]);
    hot.put(
        &seg_path(block(5)),
        &open.bytes[..open.bytes.len() - open.trailer.len()],
    );
    // Segment 7 is newer and sealed, with a trailer at its tail and no `.idx`.
    let tail = sealed(b"tail", 3);
    hot.put(&seg_path(block(7)), &tail.bytes);
    // Segment 8 is newer and sealed, with an `.idx` file only (its `.seg` is gone).
    let idx_only = sealed(b"idxonly", 4);
    hot.put(&idx_path(block(8)), &idx_only.trailer);
    // Segment 9 is newer and open.
    let fresh = build(BlockKind::Segment, &[plain(b"fresh", b"v", 5)]);
    hot.put(
        &seg_path(block(9)),
        &fresh.bytes[..fresh.bytes.len() - fresh.trailer.len()],
    );
    // A segment of another node, with a higher counter, that the mark knows nothing of.
    let other = BlockId::new(NodeId(2), 1).unwrap();
    let foreign = sealed(b"foreign", 6);
    hot.put(&seg_path(other), &foreign.bytes);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(5), 0)]);
    let report = disks.recover(&index).unwrap();
    let live = |k: &[u8]| matches!(index.get(k).unwrap(), Lookup::Live(_));
    assert!(!live(b"old"), "segment 2 is older than the mark's blocks");
    assert!(live(b"open") && live(b"tail") && live(b"idxonly") && live(b"fresh"));
    assert!(live(b"foreign"));
    assert_eq!(report.idx_files, 1);
    assert_eq!(report.trailers, 2);
    let scanned: Vec<BlockId> = report.open_segments.iter().map(|o| o.block).collect();
    assert_eq!(scanned, vec![block(5), block(9)]);
}

fn a_segment_the_mark_lists_that_is_gone_is_ignored<S: Stores>(s: &S) {
    let disks = Disks::hot_only(FakeFs::new());
    let index = marked(s, &[(block(3), 100)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.facts, 0);
    assert!(report.open_segments.is_empty());
}

fn an_idx_file_that_is_not_a_trailer_falls_back_to_the_tail_of_the_segment<S: Stores>(s: &S) {
    let built = build(BlockKind::Segment, &[plain(b"k1", b"one", 1)]);
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &built.bytes);
    // A crash in the middle of writing the `.idx` leaves a prefix of it.
    hot.put(
        &idx_path(block(1)),
        &built.trailer[..built.trailer.len() / 2],
    );
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!((report.idx_files, report.trailers), (0, 1));
    assert!(matches!(index.get(b"k1").unwrap(), Lookup::Live(_)));
}

fn a_torn_trailer_leaves_an_open_segment_cut_at_its_last_record<S: Stores>(s: &S) {
    let built = build(
        BlockKind::Segment,
        &[plain(b"k1", b"one", 1), plain(b"k2", b"two", 2)],
    );
    let records_len = (built.bytes.len() - built.trailer.len()) as u64;
    // The crash kept the first half of the trailer: the segment is not sealed.
    let kept = records_len as usize + built.trailer.len() / 2;
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &built.bytes[..kept]);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.trailers, 0);
    assert_eq!(
        report.open_segments,
        vec![OpenSegment {
            block: block(1),
            valid_len: records_len,
            torn: true
        }]
    );
    assert_eq!(disks.hot.len(&seg_path(block(1))).unwrap(), records_len);
    assert!(matches!(index.get(b"k2").unwrap(), Lookup::Live(_)));
}

fn a_trailer_whose_records_do_not_end_where_it_starts_is_not_the_segments_trailer<S: Stores>(
    s: &S,
) {
    // The tail of the file is a valid trailer of other records: its data length does not match
    // the file, so the file is scanned as an open segment.
    let a = build(BlockKind::Segment, &[plain(b"k1", b"one", 1)]);
    let b = build(
        BlockKind::Segment,
        &[plain(b"k2", b"two", 2), plain(b"k3", b"333", 3)],
    );
    let mut bytes = a.bytes[..a.bytes.len() - a.trailer.len()].to_vec();
    bytes.extend_from_slice(&b.trailer);
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &bytes);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.trailers, 0);
    assert!(report.open_segments[0].torn);
    assert!(matches!(index.get(b"k1").unwrap(), Lookup::Live(_)));
    assert_eq!(index.get(b"k2").unwrap(), Lookup::Absent);
}

fn a_trailer_larger_than_the_first_read_is_read_in_two<S: Stores>(s: &S) {
    // 3000 records: the trailer passes 64 KiB, so the tail read is repeated for its whole length.
    let keys: Vec<String> = (0..3000).map(|i| format!("key-{i:05}")).collect();
    let recs: Vec<Rec<'_>> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_bytes(), b"v".as_slice(), i as u64 + 1, RecordFlags(0)))
        .collect();
    let built = build(BlockKind::Segment, &recs);
    assert!(built.trailer.len() > 64 * 1024);
    let hot = FakeFs::new();
    hot.put(&seg_path(block(1)), &built.bytes);
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.trailers, 1);
    assert_eq!(report.facts, 3000);
    assert_eq!(disks.hot.calls(FsOp::Read), 2);
}

fn replay_skips_the_blocks_the_table_marks_dropped_or_freed<S: Stores>(s: &S) {
    let sealed = |k: &'static [u8], at: u64| build(BlockKind::Segment, &[plain(k, b"v", at)]);
    let hot = FakeFs::new();
    for (n, k) in [(2, b"dropped".as_slice()), (3, b"freed"), (4, b"kept")] {
        let b = sealed(k, n);
        hot.put(&seg_path(block(n)), &b.bytes);
        hot.put(&idx_path(block(n)), &b.trailer);
    }
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    index
        .put_block(block(2), &BlockInfo::new(BlockState::Dropped))
        .unwrap();
    index
        .put_block(block(3), &BlockInfo::new(BlockState::Freed))
        .unwrap();
    index
        .put_block(block(4), &BlockInfo::new(BlockState::Cold))
        .unwrap();
    let report = disks.recover(&index).unwrap();
    assert_eq!((report.skipped_dropped, report.skipped_freed), (1, 1));
    assert_eq!(report.idx_files, 1);
    assert!(matches!(index.get(b"kept").unwrap(), Lookup::Live(_)));
    assert_eq!(index.get(b"dropped").unwrap(), Lookup::Absent);
    assert_eq!(index.get(b"freed").unwrap(), Lookup::Absent);
}

fn replay_applies_only_the_log_entries_after_the_cursor_in_pages<S: Stores>(s: &S) {
    let disks = Disks::hot_only(FakeFs::new());
    let drop = |n: u64| {
        LogEntry::BlockDrop(BlockDrop {
            block: block(1000 + n),
            stamp: stamp(n),
        })
    };
    // 4200 entries of origin 2: after the first 50 applied, more than a page of the log.
    let records: Vec<LogRecord> = (1..=4200)
        .map(|n| LogRecord {
            origin: NodeId(2),
            seq: n,
            entry: drop(n),
        })
        .collect();
    disks.tomb.store(&records).unwrap();
    let index = marked(s, &[]);
    // The index applied the first 50.
    index
        .replay(&mut records[..50].iter().cloned().map(Fact::Log))
        .unwrap();
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.log_entries, 4150);
    assert_eq!(index.tomb_cursor(NodeId(2)).unwrap(), 4200);
    let dropped = index.blocks(None, usize::MAX).unwrap();
    assert_eq!(dropped.len(), 4200);
}

fn replay_applies_the_log_of_every_origin<S: Stores>(s: &S) {
    let disks = Disks::hot_only(FakeFs::new());
    for origin in [2u8, 3] {
        let rec = LogRecord {
            origin: NodeId(origin),
            seq: 1,
            entry: LogEntry::BlockDrop(BlockDrop {
                block: block(u64::from(origin)),
                stamp: stamp(1),
            }),
        };
        disks.tomb.store(&[rec]).unwrap();
    }
    let index = marked(s, &[]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.log_entries, 2);
    assert_eq!(
        index.tomb_cursors().unwrap(),
        vec![(NodeId(2), 1), (NodeId(3), 1)]
    );
}

fn names_in_seg_that_are_not_segments_are_ignored_and_an_idx_without_a_segment_adds_nothing<
    S: Stores,
>(
    s: &S,
) {
    let hot = FakeFs::new();
    hot.put("seg/stray-file", b"x");
    // The `.idx` of block 2 is damaged and its `.seg` is gone: nothing to read.
    hot.put(&idx_path(block(2)), b"damaged");
    let disks = Disks::hot_only(hot);
    let index = marked(s, &[(block(1), 0)]);
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.facts, 0);
    assert_eq!(report.idx_files + report.trailers, 0);
    assert!(report.open_segments.is_empty());
}

crate::on_stores!(
    mem,
    crate::recover_support::Mem,
    replay_gives_the_index_of_a_node_that_never_restarted,
    replay_reads_an_open_segment_only_from_its_marked_length,
    a_deleted_key_stays_deleted_across_restart_and_seal,
    a_torn_tail_is_cut_at_the_last_valid_record,
    a_zero_filled_tail_of_a_segment_is_the_end_of_its_records,
    a_corrupt_record_in_the_middle_ends_the_valid_records,
    a_mark_past_the_end_of_the_segment_finds_nothing_to_scan,
    replay_reads_the_segments_the_mark_does_not_cover_by_their_age,
    a_segment_the_mark_lists_that_is_gone_is_ignored,
    an_idx_file_that_is_not_a_trailer_falls_back_to_the_tail_of_the_segment,
    a_torn_trailer_leaves_an_open_segment_cut_at_its_last_record,
    a_trailer_whose_records_do_not_end_where_it_starts_is_not_the_segments_trailer,
    a_trailer_larger_than_the_first_read_is_read_in_two,
    replay_skips_the_blocks_the_table_marks_dropped_or_freed,
    replay_applies_only_the_log_entries_after_the_cursor_in_pages,
    replay_applies_the_log_of_every_origin,
    names_in_seg_that_are_not_segments_are_ignored_and_an_idx_without_a_segment_adds_nothing,
);

crate::on_stores!(
    fjall,
    crate::recover_support::fjall_store(),
    replay_gives_the_index_of_a_node_that_never_restarted,
    replay_reads_an_open_segment_only_from_its_marked_length,
    a_deleted_key_stays_deleted_across_restart_and_seal,
    a_torn_tail_is_cut_at_the_last_valid_record,
    a_zero_filled_tail_of_a_segment_is_the_end_of_its_records,
    a_corrupt_record_in_the_middle_ends_the_valid_records,
    a_mark_past_the_end_of_the_segment_finds_nothing_to_scan,
    replay_reads_the_segments_the_mark_does_not_cover_by_their_age,
    a_segment_the_mark_lists_that_is_gone_is_ignored,
    an_idx_file_that_is_not_a_trailer_falls_back_to_the_tail_of_the_segment,
    a_torn_trailer_leaves_an_open_segment_cut_at_its_last_record,
    a_trailer_whose_records_do_not_end_where_it_starts_is_not_the_segments_trailer,
    a_trailer_larger_than_the_first_read_is_read_in_two,
    replay_skips_the_blocks_the_table_marks_dropped_or_freed,
    replay_applies_only_the_log_entries_after_the_cursor_in_pages,
    replay_applies_the_log_of_every_origin,
    names_in_seg_that_are_not_segments_are_ignored_and_an_idx_without_a_segment_adds_nothing,
);

crate::on_stores!(
    redb,
    crate::recover_support::redb_store(),
    replay_gives_the_index_of_a_node_that_never_restarted,
    replay_reads_an_open_segment_only_from_its_marked_length,
    a_deleted_key_stays_deleted_across_restart_and_seal,
    a_torn_tail_is_cut_at_the_last_valid_record,
    a_zero_filled_tail_of_a_segment_is_the_end_of_its_records,
    a_corrupt_record_in_the_middle_ends_the_valid_records,
    a_mark_past_the_end_of_the_segment_finds_nothing_to_scan,
    replay_reads_the_segments_the_mark_does_not_cover_by_their_age,
    a_segment_the_mark_lists_that_is_gone_is_ignored,
    an_idx_file_that_is_not_a_trailer_falls_back_to_the_tail_of_the_segment,
    a_torn_trailer_leaves_an_open_segment_cut_at_its_last_record,
    a_trailer_whose_records_do_not_end_where_it_starts_is_not_the_segments_trailer,
    a_trailer_larger_than_the_first_read_is_read_in_two,
    replay_skips_the_blocks_the_table_marks_dropped_or_freed,
    replay_applies_only_the_log_entries_after_the_cursor_in_pages,
    replay_applies_the_log_of_every_origin,
    names_in_seg_that_are_not_segments_are_ignored_and_an_idx_without_a_segment_adds_nothing,
);
