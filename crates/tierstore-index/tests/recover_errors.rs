//! Recovery when files and the store fail or are damaged: every call of a rich recovery failed in
//! turn, damaged formats, large files that do not match their names, and the bounds of a page.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod recover_support;

use std::sync::Arc;

use recover_support::*;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, KeyHash, NodeId, StripeId};
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordFlags, RecordHeader};
use tierstore_format::tomb::{BlockDrop, LargeFile, LogEntry, LogRecord};
use tierstore_format::trailer::{BlockKind, TRAILER_FOOTER_LEN, TRAILER_MAGIC};
use tierstore_index::backend::MemBackend;
use tierstore_index::backend::mem::Call;
use tierstore_index::blocks::{BlockInfo, BlockState};
use tierstore_index::fs::Fs;
use tierstore_index::fs::fake::{FakeFs, FsOp, Torn};
use tierstore_index::kv::KvIndex;
use tierstore_index::recover::{Mode, RecoverError, Recovery, RecoveryReport};
use tierstore_index::tomblog::TombLog;
use tierstore_store_api::index::{EntryLocation, Index, Lookup, ReplayMark};

fn take(fs: &FakeFs, b: BlockId) -> (Vec<u8>, Vec<u8>) {
    let out = (
        fs.bytes(&seg_path(b)).unwrap(),
        fs.bytes(&idx_path(b)).unwrap(),
    );
    fs.remove(&seg_path(b)).unwrap();
    fs.remove(&idx_path(b)).unwrap();
    out
}

/// The bytes of a large file and its path, with the name made from `name_of` instead of the header
/// when given.
fn large(key: &[u8], data: &[u8], at: u64, name: Option<LargeFile>) -> (String, Vec<u8>) {
    let header = RecordHeader {
        flags: RecordFlags(0),
        key_len: key.len() as u16,
        data_len: data.len() as u64,
        generation: gen_of(data),
        stamp: stamp(at),
        object_offset: 0,
        data_crc: tierstore_format::crc32c(data),
        key_crc: tierstore_format::crc32c(key),
    };
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(data);
    let file = name.unwrap_or(LargeFile {
        key_hash: KeyHash::of(key),
        generation: gen_of(data),
        size: data.len() as u64,
    });
    (HotName::Large(file).path(), bytes)
}

/// Disks with every kind of file recovery reads, and a damaged tail of an open segment.
fn rich_files() -> (FakeFs, FakeFs) {
    let mut node = Node::new(false);
    for (key, data, at) in [
        (b"a", b"A1", 1),
        (b"b", b"B1", 2),
        (b"c", b"C1", 3),
        (b"d", b"D1", 4),
    ] {
        node.put(key, data, at);
        node.seal();
    }
    node.put(b"e", b"E1", 5);
    node.delete(b"a", 10);
    let hot = node.fs.crash(Torn::Lost);
    // Segment 1: a sealed segment with its `.idx`. Segment 2: its tail only. Segment 3: a `blk`
    // file. Segment 4: a cold shard of stripe 1 (and its parity shard).
    hot.remove(&idx_path(block(2))).unwrap();
    let (_, t3) = take(&hot, block(3));
    hot.put(&blk_path(block(3)), &t3);
    let (b4, _) = take(&hot, block(4));
    let desc = stripe(1, &[(block(4), b4.len() as u64)], 0);
    hot.put(
        &HotName::Descriptor(desc.stripe).path(),
        &desc.encode().unwrap(),
    );
    let cold = FakeFs::new();
    let (path, bytes) = shard_file(&desc, 0, &b4);
    cold.put(&path, &bytes);
    let (path, bytes) = shard_file(&desc, 2, &[3u8; 16]);
    cold.put(&path, &bytes);
    // Stripe 2 is freed; stripe 9 has a free and no descriptor.
    let freed = stripe(2, &[(block(7), 10)], 0);
    hot.put(
        &HotName::Descriptor(freed.stripe).path(),
        &freed.encode().unwrap(),
    );
    for id in [2, 9] {
        let free = StripeFree {
            stripe: StripeId::new(NODE, id).unwrap(),
            revision: 0,
            owner: NODE,
            stamp: stamp(50),
        };
        hot.put(&HotName::Free(free.stripe).path(), &free.encode());
    }
    let (path, bytes) = shard_file(&freed, 0, &[4u8; 16]);
    cold.put(&path, &bytes);
    // Open segment 5 has a torn tail; open segment 6 is zero-filled after its records.
    let tail = seg_path(block(5));
    hot.append(&tail, b"torn bytes that are not a record")
        .unwrap();
    hot.sync(&tail).unwrap();
    let f = build(BlockKind::Segment, &[(b"f", b"F1", 6, RecordFlags(0))]);
    let mut open = f.bytes[..f.bytes.len() - f.trailer.len()].to_vec();
    open.extend(vec![0u8; 300]);
    hot.put(&seg_path(block(6)), &open);
    // A large file, and one that does not match its name.
    let (path, bytes) = large(b"big", b"large bytes", 7, None);
    hot.put(&path, &bytes);
    let (path, bytes) = large(
        b"odd",
        b"other bytes",
        8,
        Some(LargeFile {
            key_hash: KeyHash::of(b"odd"),
            generation: gen_of(b"not the bytes"),
            size: 11,
        }),
    );
    hot.put(&path, &bytes);
    // The log of another origin.
    let rec = LogRecord {
        origin: NodeId(2),
        seq: 1,
        entry: LogEntry::BlockDrop(BlockDrop {
            block: block(40),
            stamp: stamp(60),
        }),
    };
    hot.put(
        &HotName::TombLog {
            origin: NodeId(2),
            first_seq: 1,
        }
        .path(),
        &rec.encode().unwrap(),
    );
    (hot, cold)
}

/// A recovery over files that fail on demand.
struct Case {
    hot: Arc<Flaky>,
    cold: Arc<Flaky>,
    tomb_fs: Arc<Flaky>,
    tomb: TombLog,
    index: KvIndex<MemBackend>,
}

impl Case {
    fn new(mode: Mode) -> Self {
        let (hot, cold) = rich_files();
        let hot = Arc::new(hot);
        let tomb_fs = Arc::new(Flaky::new(hot.clone()));
        let tomb = TombLog::open(tomb_fs.clone(), NODE, 1 << 20).unwrap();
        let index = index_on(MemBackend::new());
        if mode == Mode::Replay {
            let mark = ReplayMark {
                open_segments: vec![(block(1), 0)],
                tomb_cursors: Vec::new(),
            };
            index.flush_mark(&mark).unwrap();
        }
        Self {
            hot: Arc::new(Flaky::new(hot)),
            cold: Arc::new(Flaky::new(Arc::new(cold))),
            tomb_fs,
            tomb,
            index: index_on(index.backend().crashed(0)),
        }
    }

    fn run(&self) -> Result<RecoveryReport, RecoverError> {
        Recovery {
            hot: &*self.hot,
            cold: vec![&*self.cold],
            tomb: &self.tomb,
            geometry: geometry(),
            window: WINDOW,
        }
        .run(&self.index)
    }
}

#[test]
fn the_rich_recoveries_succeed_and_read_every_kind_of_file() {
    let rebuild = Case::new(Mode::Rebuild).run().unwrap();
    assert_eq!(rebuild.mode, Mode::Rebuild);
    assert_eq!(
        (rebuild.idx_files, rebuild.trailers, rebuild.shard_trailers),
        (2, 1, 1)
    );
    assert_eq!(rebuild.open_segments.len(), 2);
    assert!(rebuild.open_segments.iter().any(|o| o.torn));
    assert_eq!((rebuild.large_files, rebuild.bad_large_files), (1, 1));
    assert_eq!(rebuild.skipped_freed, 1);
    assert_eq!(rebuild.log_entries, 2);
    let replay = Case::new(Mode::Replay).run().unwrap();
    assert_eq!(replay.mode, Mode::Replay);
    assert_eq!((replay.idx_files, replay.trailers), (1, 1));
    assert_eq!(replay.open_segments.len(), 2);
    assert_eq!(replay.log_entries, 2);
}

/// What a failing call of `which` must make of the recovery.
#[derive(Clone, Copy, Debug)]
enum Which {
    Hot,
    Cold,
    Tomb,
    Store(Call),
}

/// Fails each call of one kind in turn, from the first, until a recovery gets through; every
/// failure must be reported as the error of that kind of call. Returns how many calls failed.
fn sweep(mode: Mode, which: Which) -> usize {
    for k in 0..5000 {
        let case = Case::new(mode);
        match which {
            Which::Hot => case.hot.arm(k),
            Which::Cold => case.cold.arm(k),
            Which::Tomb => case.tomb_fs.arm(k),
            Which::Store(call) => case.index.backend().fail_next(call, k, 1),
        }
        match case.run() {
            Ok(_) => return k,
            Err(e) => {
                let right = match which {
                    Which::Hot | Which::Cold => matches!(e, RecoverError::Io { .. }),
                    Which::Tomb => matches!(e, RecoverError::Log(_)),
                    Which::Store(_) => matches!(e, RecoverError::Index(_)),
                };
                assert!(right, "{mode:?} {which:?} call {k}: {e}");
            }
        }
    }
    panic!("{mode:?} {which:?}: recovery never got through");
}

#[test]
fn a_failing_call_of_the_hot_disk_fails_a_rebuild_with_an_io_error_naming_the_file() {
    assert!(sweep(Mode::Rebuild, Which::Hot) > 30);
}

#[test]
fn a_failing_call_of_the_hot_disk_fails_a_replay_with_an_io_error_naming_the_file() {
    assert!(sweep(Mode::Replay, Which::Hot) > 20);
}

#[test]
fn a_failing_call_of_a_cold_drive_fails_a_rebuild_with_an_io_error() {
    assert!(sweep(Mode::Rebuild, Which::Cold) >= 5);
}

#[test]
fn a_failing_read_of_the_tomb_log_fails_a_rebuild_or_a_replay_with_a_log_error() {
    // The log files are read when a pull is served: one read per origin file per page.
    assert!(sweep(Mode::Rebuild, Which::Tomb) >= 2);
    assert!(sweep(Mode::Replay, Which::Tomb) >= 2);
}

#[test]
fn a_failing_call_of_the_index_store_fails_a_rebuild_with_an_index_error() {
    for call in [Call::Get, Call::Scan, Call::Commit] {
        sweep(Mode::Rebuild, Which::Store(call));
    }
}

#[test]
fn a_failing_call_of_the_index_store_fails_a_replay_with_an_index_error() {
    for call in [Call::Get, Call::Scan, Call::Commit] {
        sweep(Mode::Replay, Which::Store(call));
    }
}

#[test]
fn an_io_error_names_the_file_and_what_failed() {
    let case = Case::new(Mode::Rebuild);
    case.hot.arm(0);
    let e = case.run().unwrap_err();
    assert_eq!(e.to_string(), "recovery: stripes: injected failure");
}

fn damaged(f: impl FnOnce(&FakeFs, &FakeFs)) -> RecoverError {
    let hot = FakeFs::new();
    let cold = FakeFs::new();
    f(&hot, &cold);
    let disks = Disks::new(Arc::new(hot), vec![Arc::new(cold)]);
    let index = index_on(MemBackend::new());
    disks.recover(&index).unwrap_err()
}

fn format_path(e: &RecoverError) -> &str {
    match e {
        RecoverError::Format { path, .. } => path,
        other => panic!("not a format error: {other}"),
    }
}

#[test]
fn a_stripe_descriptor_or_free_that_does_not_decode_is_a_format_error_naming_the_file() {
    let desc = HotName::Descriptor(StripeId::new(NODE, 1).unwrap()).path();
    let e = damaged(|hot, _| hot.put(&desc, b"junk"));
    assert_eq!(format_path(&e), desc);
    let free = HotName::Free(StripeId::new(NODE, 1).unwrap()).path();
    let e = damaged(|hot, _| hot.put(&free, b"junk"));
    assert_eq!(format_path(&e), free);
}

/// A copy file of block 1 whose data region is `data`.
fn copy_of(data: &[u8]) -> (String, Vec<u8>) {
    copy_file(block(1), data)
}

#[test]
fn a_cold_file_without_a_footer_or_with_a_damaged_one_is_a_format_error() {
    let (path, _) = copy_of(b"");
    let e = damaged(|_, cold| cold.put(&path, &[0u8; 8192]));
    assert_eq!(format_path(&e), path);
    assert!(e.to_string().contains("no footer"), "{e}");
    // A footer whose bytes were flipped: its crc fails.
    let (path, mut bytes) = copy_of(&[0u8; 100]);
    let at = bytes.len() - 100;
    bytes[at] ^= 0xff;
    let e = damaged(|_, cold| cold.put(&path, &bytes));
    assert_eq!(format_path(&e), path);
    assert!(e.to_string().contains("crc32c"), "{e}");
}

#[test]
fn a_cold_file_whose_data_region_does_not_end_in_a_trailer_is_a_format_error() {
    let good = build(BlockKind::Segment, &[(b"k", b"v", 1, RecordFlags(0))]);
    // No trailer: the data region is zeros.
    let (path, bytes) = copy_of(&[0u8; 200]);
    let e = damaged(|_, cold| cold.put(&path, &bytes));
    assert_eq!(format_path(&e), path);
    assert!(e.to_string().contains("no trailer"), "{e}");
    // Shorter than a trailer footer.
    let (path, bytes) = copy_of(&[0u8; 50]);
    let e = damaged(|_, cold| cold.put(&path, &bytes));
    assert!(e.to_string().contains("no trailer"), "{e}");
    // A footer that says a trailer longer than the data region.
    let mut data = vec![0u8; 200];
    data[200 - TRAILER_FOOTER_LEN..200 - TRAILER_FOOTER_LEN + 4]
        .copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
    data[200 - TRAILER_FOOTER_LEN + 16..200 - TRAILER_FOOTER_LEN + 24]
        .copy_from_slice(&u64::MAX.to_le_bytes());
    let (path, bytes) = copy_of(&data);
    let e = damaged(|_, cold| cold.put(&path, &bytes));
    assert!(e.to_string().contains("no trailer"), "{e}");
    // A trailer with a flipped entry byte: its crc fails.
    let mut data = good.bytes.clone();
    let at = good.bytes.len() - good.trailer.len() + 5;
    data[at] ^= 0xff;
    let (path, bytes) = copy_of(&data);
    let e = damaged(|_, cold| cold.put(&path, &bytes));
    assert_eq!(format_path(&e), path);
    assert!(e.to_string().contains("crc32c"), "{e}");
    // A trailer of other records: it does not start where its records end.
    let (path, bytes) = copy_of(&good.trailer);
    let e = damaged(|_, cold| cold.put(&path, &bytes));
    assert!(e.to_string().contains("does not start"), "{e}");
}

#[test]
fn a_cold_copy_of_a_dropped_block_is_skipped_by_its_name_without_a_read() {
    // The copy's name says its block: no footer is read for a block that is dropped.
    let cold = Arc::new(FakeFs::new());
    let (path, bytes) = copy_of(&[0u8; 100]);
    cold.put(&path, &bytes);
    let disks = Disks::new(Arc::new(FakeFs::new()), vec![cold.clone()]);
    disks
        .tomb
        .append(LogEntry::BlockDrop(BlockDrop {
            block: block(1),
            stamp: stamp(1),
        }))
        .unwrap();
    let index = index_on(MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.skipped_dropped, 1);
    assert_eq!(cold.calls(FsOp::Read), 0);
}

#[test]
fn large_files_that_do_not_match_their_names_are_reported_and_never_indexed() {
    let data = b"the object bytes";
    let key = b"obj";
    let exact = LargeFile {
        key_hash: KeyHash::of(key),
        generation: gen_of(data),
        size: data.len() as u64,
    };
    // The file's head is that of `key` and `data`; its name says another generation, size or key.
    let names = [
        LargeFile {
            generation: gen_of(b"other"),
            ..exact
        },
        LargeFile { size: 99, ..exact },
        LargeFile {
            key_hash: KeyHash::of(b"other key"),
            ..exact
        },
    ];
    let hot = FakeFs::new();
    for name in names {
        let (path, bytes) = large(key, data, 5, Some(name));
        hot.put(&path, &bytes);
    }
    // A flipped key byte: the key crc fails.
    let (path, mut bytes) = large(key, data, 5, None);
    bytes[RECORD_HEADER_LEN] ^= 0xff;
    hot.put(&path, &bytes);
    // A header that is not a header, and a file shorter than one, under names of their own.
    let name = |n: u8| LargeFile {
        key_hash: KeyHash([n; 16]),
        generation: gen_of(data),
        size: 1,
    };
    hot.put(&HotName::Large(name(1)).path(), &[0u8; 100]);
    hot.put(&HotName::Large(name(2)).path(), &[7u8; 10]);
    // A header whose key is longer than the file.
    let (_, mut short_key) = large(key, data, 5, None);
    short_key.truncate(RECORD_HEADER_LEN + 1);
    hot.put(&HotName::Large(name(3)).path(), &short_key);
    // Things that are not large files: a directory name that is not two characters, a name that
    // does not parse, an upload in progress.
    hot.put("big/abc/x.obj", b"x");
    hot.put("big/ab/not-a-large-file", b"x");
    hot.put(
        &HotName::LargeUpload {
            key_hash: KeyHash::of(key),
            nonce: 3,
        }
        .path(),
        b"partial",
    );
    // Two good ones.
    let (path, bytes) = large(b"good", b"good bytes", 6, None);
    hot.put(&path, &bytes);
    let (path, bytes) = large(b"another", b"more bytes", 9, None);
    hot.put(&path, &bytes);
    let disks = Disks::hot_only(hot);
    let index = index_on(MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.large_files, 2);
    assert_eq!(report.bad_large_files, 3 + 1 + 3);
    assert_eq!(index.list(b"", None, 10).unwrap().len(), 2);
    assert_eq!(index.get(b"obj").unwrap(), Lookup::Absent);
    let Lookup::Live(e) = index.get(b"good").unwrap() else {
        panic!("the good large file is indexed");
    };
    assert_eq!(e.stamp, stamp(6));
    assert_eq!(e.length, 10);
    assert!(matches!(e.location, EntryLocation::Large(f) if f.size == 10));
}

#[test]
fn a_replay_reads_the_large_files_too() {
    let hot = FakeFs::new();
    let (path, bytes) = large(b"big", b"large bytes", 7, None);
    hot.put(&path, &bytes);
    let disks = Disks::hot_only(hot);
    let index = index_on(MemBackend::new());
    index
        .flush_mark(&ReplayMark {
            open_segments: Vec::new(),
            tomb_cursors: Vec::new(),
        })
        .unwrap();
    let report = disks.recover(&index).unwrap();
    assert_eq!((report.mode, report.large_files), (Mode::Replay, 1));
    assert!(matches!(index.get(b"big").unwrap(), Lookup::Live(_)));
}

#[test]
fn a_block_table_longer_than_a_page_is_read_to_its_end() {
    let hot = FakeFs::new();
    let sealed = build(BlockKind::Segment, &[(b"late", b"v", 1, RecordFlags(0))]);
    hot.put(&seg_path(block(5000)), &sealed.bytes);
    hot.put(&idx_path(block(5000)), &sealed.trailer);
    let disks = Disks::hot_only(hot);
    let index = index_on(MemBackend::new());
    // 1100 rows: the table is read in two pages. The last row is the one that matters.
    for n in 1..=1100u64 {
        index
            .put_block(block(n), &BlockInfo::new(BlockState::Cold))
            .unwrap();
    }
    index
        .put_block(block(5000), &BlockInfo::new(BlockState::Freed))
        .unwrap();
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.skipped_freed, 1);
    assert_eq!(index.get(b"late").unwrap(), Lookup::Absent);
}

#[test]
fn a_record_longer_than_the_window_is_read_whole_and_a_bad_stripe_name_is_ignored() {
    let big = vec![9u8; 1000];
    let built = build(
        BlockKind::Segment,
        &[
            (b"s1", b"small", 1, RecordFlags(0)),
            (b"large", &big, 2, RecordFlags(0)),
            (b"s2", b"small", 3, RecordFlags(0)),
        ],
    );
    let hot = FakeFs::new();
    hot.put(
        &seg_path(block(1)),
        &built.bytes[..built.bytes.len() - built.trailer.len()],
    );
    hot.put("stripes/not-a-stripe", b"x");
    let disks = Disks::hot_only(hot);
    let index = index_on(MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.facts, 3);
    assert!(!report.open_segments[0].torn);
    let Lookup::Live(e) = index.get(b"large").unwrap() else {
        panic!("large is live");
    };
    assert_eq!(e.length, 1000);
}

#[test]
fn a_shard_with_a_footer_of_a_stripe_names_its_block_by_the_footer_when_the_stripe_is_unknown() {
    // The footer's descriptor has the block at the shard's index: that block's trailer is read.
    let built = build(BlockKind::Segment, &[(b"k", b"v", 1, RecordFlags(0))]);
    let desc = stripe(5, &[(block(3), 10)], 0);
    let cold = FakeFs::new();
    let (path, bytes) = shard_file(&desc, 0, &built.bytes);
    cold.put(&path, &bytes);
    let disks = Disks::new(Arc::new(FakeFs::new()), vec![Arc::new(cold)]);
    let index = index_on(MemBackend::new());
    let report = disks.recover(&index).unwrap();
    assert_eq!(report.shard_trailers, 1);
    let Lookup::Live(e) = index.get(b"k").unwrap() else {
        panic!("k is live");
    };
    assert_eq!(
        e.location,
        EntryLocation::Block {
            block: block(3),
            offset: 0
        }
    );
}
