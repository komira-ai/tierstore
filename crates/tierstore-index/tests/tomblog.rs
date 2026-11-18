//! The tomb log files: appends, tolerant open, store, pulls, compaction and I/O failures.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::sync::Arc;

use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{
    BlockDrop, LogEntry, LogRecord, Named, TombDrop, Tombstone, scan_log,
};
use tierstore_index::fs::fake::{FakeFs, FsOp, Torn};
use tierstore_index::fs::{DirFs, Fs};
use tierstore_index::tomblog::{LogError, TombLog};
use tierstore_store_api::proto::{TombPage, TombPull};
use tierstore_testkit::dir::TestDir;

const MAX: u64 = u64::MAX;

fn entry(n: u64) -> LogEntry {
    LogEntry::BlockDrop(BlockDrop {
        block: BlockId::from_raw(n),
        stamp: Stamp::from_raw(n),
    })
}

fn rec(origin: u8, seq: u64) -> LogRecord {
    LogRecord {
        origin: NodeId(origin),
        seq,
        entry: entry(seq),
    }
}

fn enc(r: &LogRecord) -> Vec<u8> {
    r.encode().unwrap()
}

/// The length of one framed `BlockDrop` entry.
fn frame() -> u64 {
    enc(&rec(1, 1)).len() as u64
}

fn frames(origin: u8, seqs: std::ops::RangeInclusive<u64>) -> Vec<u8> {
    seqs.flat_map(|s| enc(&rec(origin, s))).collect()
}

fn fake() -> Arc<FakeFs> {
    Arc::new(FakeFs::new())
}

fn open(fs: &Arc<FakeFs>, node: u8, max: u64) -> Result<TombLog, LogError> {
    TombLog::open(fs.clone(), NodeId(node), max)
}

fn seqs(records: &[LogRecord]) -> Vec<u64> {
    records.iter().map(|r| r.seq).collect()
}

fn io_of(e: &LogError) -> (&'static str, &str) {
    match e {
        LogError::Io { op, path, .. } => (op, path),
        other => panic!("not an Io error: {other}"),
    }
}

#[test]
fn append_numbers_from_one_and_continues_across_reopen() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.last(NodeId(1)), 0);
    for n in 1..=3 {
        let r = log.append(entry(n)).unwrap();
        assert_eq!((r.origin, r.seq, r.entry), (NodeId(1), n, entry(n)));
    }
    assert_eq!(log.last(NodeId(1)), 3);
    assert_eq!(log.origins(), [NodeId(1)]);
    drop(log);
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.last(NodeId(1)), 3);
    assert_eq!(log.append(entry(4)).unwrap().seq, 4);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=4));
    assert_eq!(fs.calls(FsOp::Read), 1, "open reads each file once");
}

#[test]
fn an_entry_whose_append_returned_survives_a_crash_that_loses_unsynced_bytes() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    log.append(entry(1)).unwrap();
    log.append(entry(2)).unwrap();
    let crashed = Arc::new(fs.crash(Torn::Lost));
    let log = open(&crashed, 1, MAX).unwrap();
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), [1, 2]);
}

#[test]
fn a_torn_tail_of_every_length_is_cut_at_open_and_the_next_append_continues() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    for n in 1..=3 {
        log.append(entry(n)).unwrap();
    }
    let fourth = enc(&rec(1, 4));
    fs.append("tomb/1/1.log", &fourth).unwrap();
    for k in 0..fourth.len() {
        let crashed = Arc::new(fs.crash(Torn::Prefix(k as u64)));
        let log = open(&crashed, 1, MAX).unwrap();
        assert_eq!(log.last(NodeId(1)), 3, "cut {k}");
        assert_eq!(crashed.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=3));
        assert_eq!(crashed.calls(FsOp::Truncate), u64::from(k > 0), "cut {k}");
        assert_eq!(log.append(entry(4)).unwrap().seq, 4);
        let bytes = crashed.bytes("tomb/1/1.log").unwrap();
        assert_eq!(seqs(&scan_log(&bytes, NodeId(1)).unwrap()), [1, 2, 3, 4]);
        let again = Arc::new(crashed.crash(Torn::Lost));
        assert_eq!(open(&again, 1, MAX).unwrap().last(NodeId(1)), 4);
    }
    let whole = open(&Arc::new(fs.crash(Torn::Kept)), 1, MAX).unwrap();
    assert_eq!(whole.last(NodeId(1)), 4);
}

#[test]
fn a_torn_first_entry_leaves_an_empty_file_that_the_next_append_fills() {
    let fs = fake();
    fs.put("tomb/1/1.log", &enc(&rec(1, 1))[..7]);
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.last(NodeId(1)), 0);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), b"");
    assert_eq!(log.append(entry(1)).unwrap().seq, 1);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=1));
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log"]);
}

#[test]
fn a_damaged_entry_in_the_tail_of_the_last_file_is_cut_with_what_follows() {
    let fs = fake();
    let mut bytes = frames(1, 1..=3);
    bytes[(frame() * 2 + 30) as usize] ^= 0xff;
    fs.put("tomb/1/1.log", &bytes);
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.last(NodeId(1)), 2);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=2));
}

#[test]
fn a_damaged_file_that_is_not_the_last_of_its_origin_is_a_format_error() {
    let fs = fake();
    let mut bytes = frames(1, 1..=2);
    bytes[frame() as usize + 30] ^= 0xff;
    fs.put("tomb/1/1.log", &bytes);
    fs.put("tomb/1/3.log", &frames(1, 3..=3));
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(&err, LogError::Format { path, .. } if path == "tomb/1/1.log"),
        "{err}"
    );
    assert_eq!(
        fs.calls(FsOp::Truncate),
        0,
        "nothing is cut outside the tail"
    );
}

#[test]
fn files_that_do_not_continue_each_other_are_a_gap() {
    let fs = fake();
    fs.put("tomb/1/1.log", &frames(1, 1..=2));
    fs.put("tomb/1/4.log", &frames(1, 4..=4));
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(
            err,
            LogError::Gap {
                origin: NodeId(1),
                last: 2,
                got: 4
            }
        ),
        "{err}"
    );
}

#[test]
fn an_empty_file_before_the_last_leaves_a_gap() {
    let fs = fake();
    fs.put("tomb/1/1.log", b"");
    fs.put("tomb/1/2.log", &frames(1, 2..=2));
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(
            err,
            LogError::Gap {
                origin: NodeId(1),
                last: 0,
                got: 2
            }
        ),
        "{err}"
    );
}

#[test]
fn an_entry_of_another_origin_in_a_file_is_refused() {
    let fs = fake();
    fs.put("tomb/1/1.log", &frames(2, 1..=1));
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(&err, LogError::WrongOrigin { path, origin: NodeId(1), found: NodeId(2) }
            if path == "tomb/1/1.log"),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "tomb log: tomb/1/1.log of origin 1 holds an entry of origin 2"
    );
}

#[test]
fn entries_that_do_not_count_up_from_the_file_name_are_corrupt() {
    let fs = fake();
    let mut bytes = frames(1, 1..=1);
    bytes.extend(enc(&rec(1, 3)));
    fs.put("tomb/1/1.log", &bytes);
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(&err, LogError::Corrupt { path, offset } if path == "tomb/1/1.log" && *offset == frame()),
        "{err}"
    );
    let fs = fake();
    fs.put("tomb/1/5.log", &frames(1, 6..=6));
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(&err, LogError::Corrupt { path, offset: 0 } if path == "tomb/1/5.log"),
        "{err}"
    );
    let fs = fake();
    fs.put("tomb/1/0.log", b"");
    let err = open(&fs, 1, MAX).err().unwrap();
    assert!(
        matches!(&err, LogError::Corrupt { path, offset: 0 } if path == "tomb/1/0.log"),
        "{err}"
    );
}

#[test]
fn a_log_whose_head_was_compacted_opens_at_its_first_file() {
    let fs = fake();
    fs.put("tomb/1/5.log", &frames(1, 5..=6));
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.last(NodeId(1)), 6);
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), [5, 6]);
    assert_eq!(log.append(entry(7)).unwrap().seq, 7);
}

#[test]
fn names_that_are_not_log_files_are_left_alone_at_open() {
    let fs = fake();
    fs.put("tomb/readme", b"x");
    fs.put("tomb/x/1.log", b"x");
    fs.put("tomb/01/1.log", b"x");
    fs.put("tomb/300/1.log", b"x");
    fs.put("tomb/2/notes.txt", b"x");
    fs.put("tomb/2/01.log", b"x");
    fs.put("tomb/3/7.log", &frames(3, 7..=7));
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.origins(), [NodeId(3)]);
    assert_eq!(log.last(NodeId(2)), 0);
    assert_eq!(log.last(NodeId(3)), 7);
}

#[test]
fn files_roll_over_at_the_limit_into_first_seq_names_and_numbering_continues_after_reopen() {
    let fs = fake();
    let log = open(&fs, 1, 2 * frame()).unwrap();
    for n in 1..=5 {
        log.append(entry(n)).unwrap();
    }
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log", "3.log", "5.log"]);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=2));
    assert_eq!(fs.bytes("tomb/1/3.log").unwrap(), frames(1, 3..=4));
    assert_eq!(fs.bytes("tomb/1/5.log").unwrap(), frames(1, 5..=5));
    drop(log);
    let log = open(&fs, 1, 2 * frame()).unwrap();
    assert_eq!(log.append(entry(6)).unwrap().seq, 6);
    assert_eq!(log.append(entry(7)).unwrap().seq, 7);
    assert_eq!(fs.bytes("tomb/1/5.log").unwrap(), frames(1, 5..=6));
    assert_eq!(fs.bytes("tomb/1/7.log").unwrap(), frames(1, 7..=7));
    assert_eq!(
        seqs(&log.read_all(NodeId(1)).unwrap()),
        [1, 2, 3, 4, 5, 6, 7]
    );
}

#[test]
fn an_entry_larger_than_the_limit_still_gets_a_file_of_its_own() {
    let fs = fake();
    let log = open(&fs, 1, 1).unwrap();
    for n in 1..=3 {
        log.append(entry(n)).unwrap();
    }
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log", "2.log", "3.log"]);
}

#[test]
fn a_new_file_starts_after_a_torn_cut_left_an_empty_last_file() {
    let fs = fake();
    fs.put("tomb/1/1.log", &frames(1, 1..=1));
    fs.put("tomb/1/2.log", &enc(&rec(1, 2))[..9]);
    let log = open(&fs, 1, 0).unwrap();
    assert_eq!(log.last(NodeId(1)), 1);
    assert_eq!(log.append(entry(2)).unwrap().seq, 2);
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log", "2.log"]);
    assert_eq!(fs.bytes("tomb/1/2.log").unwrap(), frames(1, 2..=2));
}

#[test]
fn store_continues_each_origin_skips_what_it_has_and_returns_the_count() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(
        log.store(&[rec(2, 1), rec(3, 1), rec(2, 2), rec(2, 3)])
            .unwrap(),
        4
    );
    assert_eq!(log.origins(), [NodeId(2), NodeId(3)]);
    assert_eq!((log.last(NodeId(2)), log.last(NodeId(3))), (3, 1));
    let batch = [rec(2, 2), rec(2, 3), rec(2, 4), rec(2, 5)];
    assert_eq!(log.store(&batch).unwrap(), 2);
    assert_eq!(log.store(&batch).unwrap(), 0);
    assert_eq!(log.store(&[]).unwrap(), 0);
    assert_eq!(seqs(&log.read_all(NodeId(2)).unwrap()), [1, 2, 3, 4, 5]);
    assert_eq!(fs.bytes("tomb/2/1.log").unwrap(), frames(2, 1..=5));
}

#[test]
fn stored_entries_are_synced_before_store_returns() {
    let fs = fake();
    let log = open(&fs, 1, 2 * frame()).unwrap();
    let batch: Vec<_> = (1..=5).map(|s| rec(2, s)).collect();
    log.store(&batch).unwrap();
    assert_eq!(fs.calls(FsOp::Sync), 3, "one sync per file touched");
    log.store(&batch).unwrap();
    assert_eq!(fs.calls(FsOp::Sync), 3, "nothing stored, nothing synced");
    let crashed = Arc::new(fs.crash(Torn::Lost));
    let log = open(&crashed, 1, MAX).unwrap();
    assert_eq!(log.last(NodeId(2)), 5);
}

#[test]
fn store_refuses_a_gap_and_keeps_nothing_of_the_batch() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    log.store(&[rec(2, 1), rec(2, 2), rec(2, 3)]).unwrap();
    let err = log.store(&[rec(2, 4), rec(2, 5), rec(2, 7)]).err().unwrap();
    assert!(
        matches!(
            err,
            LogError::Gap {
                origin: NodeId(2),
                last: 5,
                got: 7
            }
        ),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "tomb log of origin 2: the last sequence is 5, got 7"
    );
    assert_eq!(log.last(NodeId(2)), 3);
    assert_eq!(fs.bytes("tomb/2/1.log").unwrap(), frames(2, 1..=3));
    assert_eq!(log.store(&[rec(2, 4)]).unwrap(), 1);
}

#[test]
fn store_of_a_new_origin_must_start_at_one() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    let err = log.store(&[rec(2, 5)]).err().unwrap();
    assert!(
        matches!(
            err,
            LogError::Gap {
                origin: NodeId(2),
                last: 0,
                got: 5
            }
        ),
        "{err}"
    );
    assert_eq!(log.origins(), Vec::<NodeId>::new());
}

#[test]
fn store_refuses_an_entry_of_this_nodes_own_origin_and_keeps_nothing_of_the_batch() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    let err = log.store(&[rec(2, 1), rec(1, 1)]).err().unwrap();
    assert!(
        matches!(err, LogError::OwnOrigin { origin: NodeId(1) }),
        "{err}"
    );
    assert_eq!(log.last(NodeId(2)), 0);
    assert_eq!(log.last(NodeId(1)), 0);
    assert_eq!(log.store(&[rec(2, 1)]).unwrap(), 1);
    assert_eq!(fs.bytes("tomb/2/1.log").unwrap(), frames(2, 1..=1));
}

#[test]
fn an_entry_that_cannot_be_encoded_is_a_format_error_naming_the_file() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    let bad = LogRecord {
        origin: NodeId(2),
        seq: 1,
        entry: LogEntry::Unknown {
            kind: 1,
            body: vec![],
        },
    };
    let err = log.store(&[bad]).err().unwrap();
    assert!(
        matches!(&err, LogError::Format { path, .. } if path == "tomb/2/1.log"),
        "{err}"
    );
    assert_eq!(log.last(NodeId(2)), 0);
}

#[test]
fn read_serves_the_entries_after_a_cursor_with_a_limit_and_whether_more_follow() {
    let fs = fake();
    let log = open(&fs, 1, 2 * frame()).unwrap();
    for n in 1..=7 {
        log.append(entry(n)).unwrap();
    }
    let read = |after, limit| {
        let (records, more) = log.read(NodeId(1), after, limit).unwrap();
        (seqs(&records), more)
    };
    assert_eq!(read(0, 3), (vec![1, 2, 3], true));
    assert_eq!(read(3, 2), (vec![4, 5], true));
    assert_eq!(read(5, 10), (vec![6, 7], false));
    assert_eq!(read(0, 7), ((1..=7).collect::<Vec<_>>(), false));
    assert_eq!(read(6, 1), (vec![7], false));
    assert_eq!(read(7, 10), (vec![], false));
    assert_eq!(read(100, 10), (vec![], false));
    assert_eq!(read(0, 0), (vec![], true));
    assert_eq!(read(7, 0), (vec![], false));
    assert_eq!(log.read(NodeId(9), 0, 10).unwrap(), (vec![], false));
}

#[test]
fn a_read_page_round_trips_through_tomb_page() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    for n in 1..=5 {
        log.append(entry(n)).unwrap();
    }
    let (records, more) = log.read(NodeId(1), 2, 2).unwrap();
    let page = TombPage::new(2, &records, more).unwrap();
    assert_eq!((page.last, page.more), (4, true));
    let pull = TombPull {
        origin: NodeId(1),
        after: 2,
        limit: 2,
    };
    assert_eq!(page.decode(&pull).unwrap(), records);
    assert_eq!(seqs(&records), [3, 4]);
}

#[test]
fn every_entry_kind_including_an_unknown_one_is_stored_and_served_unchanged() {
    let entries = [
        LogEntry::Tombstone(Tombstone {
            key: b"bucket/key".to_vec(),
            generation: Generation([7; 16]),
            stamp: Stamp::from_raw(99),
            names: vec![
                Named::Block(BlockId::from_raw(3)),
                Named::Unknown {
                    kind: 42,
                    bytes: vec![1, 2],
                },
            ],
        }),
        LogEntry::BlockDrop(BlockDrop {
            block: BlockId::from_raw(8),
            stamp: Stamp::from_raw(5),
        }),
        LogEntry::TombDrop(TombDrop {
            target_origin: NodeId(4),
            target_seq: 11,
        }),
        LogEntry::Unknown {
            kind: 9,
            body: vec![1, 2, 3],
        },
        LogEntry::Unknown {
            kind: 200,
            body: vec![],
        },
    ];
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    let appended: Vec<LogRecord> = entries
        .iter()
        .map(|e| log.append(e.clone()).unwrap())
        .collect();
    let other = open(&fake(), 2, MAX).unwrap();
    let copies: Vec<LogRecord> = appended
        .iter()
        .map(|r| LogRecord {
            origin: NodeId(5),
            ..r.clone()
        })
        .collect();
    other.store(&copies).unwrap();
    drop(log);
    let log = open(&fs, 1, MAX).unwrap();
    assert_eq!(log.read_all(NodeId(1)).unwrap(), appended);
    assert_eq!(other.read_all(NodeId(5)).unwrap(), copies);
}

fn compactable() -> (Arc<FakeFs>, TombLog) {
    let fs = fake();
    let log = open(&fs, 1, 2 * frame()).unwrap();
    for n in 1..=5 {
        log.append(entry(n)).unwrap();
    }
    (fs, log)
}

#[test]
fn compaction_removes_whole_droppable_files_and_reopen_continues_numbering() {
    let (fs, log) = compactable();
    assert_eq!(log.compact(NodeId(1), &|r| r.seq <= 4).unwrap(), 2);
    assert_eq!(fs.list("tomb/1").unwrap(), ["5.log"]);
    assert_eq!(log.last(NodeId(1)), 5);
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), [5]);
    assert_eq!(log.compact(NodeId(1), &|_| true).unwrap(), 0);
    drop(log);
    let log = open(&Arc::new(fs.crash(Torn::Lost)), 1, 2 * frame()).unwrap();
    assert_eq!(log.last(NodeId(1)), 5);
    let log = open(&fs, 1, 2 * frame()).unwrap();
    assert_eq!(log.append(entry(6)).unwrap().seq, 6);
    assert_eq!(fs.bytes("tomb/1/5.log").unwrap(), frames(1, 5..=6));
}

#[test]
fn compaction_never_removes_the_last_file_of_an_origin() {
    let (fs, log) = compactable();
    assert_eq!(log.compact(NodeId(1), &|_| true).unwrap(), 2);
    assert_eq!(fs.list("tomb/1").unwrap(), ["5.log"]);
    assert_eq!(log.last(NodeId(1)), 5);
}

#[test]
fn compaction_keeps_a_file_with_one_entry_that_is_not_droppable_and_the_files_after_it() {
    let (fs, log) = compactable();
    assert_eq!(log.compact(NodeId(1), &|r| r.seq != 2).unwrap(), 0);
    assert_eq!(
        log.compact(NodeId(1), &|r| r.seq == 3 || r.seq == 4)
            .unwrap(),
        0
    );
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log", "3.log", "5.log"]);
    assert_eq!(log.compact(NodeId(1), &|r| r.seq <= 2).unwrap(), 1);
    assert_eq!(fs.list("tomb/1").unwrap(), ["3.log", "5.log"]);
    assert_eq!(log.compact(NodeId(9), &|_| true).unwrap(), 0);
}

#[test]
fn a_failed_removal_in_compaction_is_an_io_error_and_leaves_the_file_listed() {
    let (fs, log) = compactable();
    fs.fail(FsOp::Remove, "tomb/1/3.log", 1);
    let err = log.compact(NodeId(1), &|_| true).err().unwrap();
    assert_eq!(io_of(&err), ("remove", "tomb/1/3.log"));
    assert_eq!(fs.list("tomb/1").unwrap(), ["3.log", "5.log"]);
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), [3, 4, 5]);
    assert_eq!(log.compact(NodeId(1), &|_| true).unwrap(), 1);
}

#[test]
fn a_failed_read_in_compaction_or_in_a_pull_is_an_io_error_naming_the_file() {
    let (fs, log) = compactable();
    fs.fail(FsOp::Read, "tomb/1/1.log", 2);
    let err = log.compact(NodeId(1), &|_| true).err().unwrap();
    assert_eq!(io_of(&err), ("read", "tomb/1/1.log"));
    let err = log.read(NodeId(1), 0, 10).err().unwrap();
    assert_eq!(io_of(&err), ("read", "tomb/1/1.log"));
    assert_eq!(log.read(NodeId(1), 0, 10).unwrap().0.len(), 5);
}

#[test]
fn a_file_changed_under_the_log_is_a_format_error_on_read() {
    let (fs, log) = compactable();
    fs.put("tomb/1/1.log", &[0xff; 80]);
    let err = log.read(NodeId(1), 0, 10).err().unwrap();
    assert!(
        matches!(&err, LogError::Format { path, .. } if path == "tomb/1/1.log"),
        "{err}"
    );
}

#[test]
fn failures_at_open_are_io_errors_naming_the_operation_and_path() {
    let setup = || {
        let fs = fake();
        fs.put("tomb/1/1.log", &frames(1, 1..=2));
        fs
    };
    let cases = [
        (FsOp::List, "tomb", "list", "tomb"),
        (FsOp::List, "tomb/", "list", "tomb/1"),
        (FsOp::Len, "tomb/1", "len", "tomb/1/1.log"),
        (FsOp::Read, "tomb/1", "read", "tomb/1/1.log"),
    ];
    for (fail, prefix, op, path) in cases {
        let fs = setup();
        fs.fail(fail, prefix, 1);
        let err = open(&fs, 1, MAX).err().unwrap();
        assert_eq!(io_of(&err), (op, path), "{fail:?}");
        assert_eq!(open(&fs, 1, MAX).unwrap().last(NodeId(1)), 2);
    }
    for fail in [FsOp::Truncate, FsOp::Sync] {
        let fs = setup();
        fs.append("tomb/1/1.log", &enc(&rec(1, 3))[..5]).unwrap();
        fs.fail(fail, "tomb/1", 1);
        let err = open(&fs, 1, MAX).err().unwrap();
        let op = if fail == FsOp::Truncate {
            "truncate"
        } else {
            "sync"
        };
        assert_eq!(io_of(&err), (op, "tomb/1/1.log"));
    }
}

#[test]
fn a_failed_append_is_an_io_error_and_the_next_append_takes_the_same_sequence() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    fs.fail(FsOp::Append, "tomb/1", 1);
    let err = log.append(entry(1)).err().unwrap();
    assert_eq!(io_of(&err), ("append", "tomb/1/1.log"));
    assert_eq!(
        err.to_string(),
        "tomb log: append tomb/1/1.log: injected Append failure on tomb/1/1.log"
    );
    assert_eq!(log.last(NodeId(1)), 0);
    assert_eq!(
        fs.calls(FsOp::Remove),
        1,
        "the file the append started is removed"
    );
    assert_eq!(fs.list("tomb/1").unwrap(), Vec::<String>::new());
    assert_eq!(log.append(entry(1)).unwrap().seq, 1);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=1));
}

#[test]
fn a_failed_sync_of_a_file_the_append_started_leaves_no_file_under_its_sequence() {
    // Catches: an empty `<seq>.log` left behind after a failed rollover; once that sequence was
    // written into an earlier file, the next open refused the log with a gap.
    let fs = fake();
    let log = open(&fs, 1, 2 * frame()).unwrap();
    log.append(entry(1)).unwrap();
    log.append(entry(2)).unwrap();
    fs.fail(FsOp::Sync, "tomb/1/3.log", 1);
    let err = log.append(entry(3)).err().unwrap();
    assert_eq!(io_of(&err), ("sync", "tomb/1/3.log"));
    assert_eq!(fs.list("tomb/1").unwrap(), ["1.log"]);
    assert_eq!(log.last(NodeId(1)), 2);
    drop(log);
    let log = open(&fs, 1, 2 * frame()).unwrap();
    assert_eq!(log.append(entry(3)).unwrap().seq, 3);
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), [1, 2, 3]);
}

#[test]
fn a_failed_sync_leaves_the_file_as_it_was_when_the_cut_succeeds() {
    let fs = fake();
    let log = open(&fs, 1, MAX).unwrap();
    log.append(entry(1)).unwrap();
    fs.fail(FsOp::Sync, "tomb/1", 1);
    let err = log.append(entry(2)).err().unwrap();
    assert_eq!(io_of(&err), ("sync", "tomb/1/1.log"));
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=1));
    assert_eq!(log.last(NodeId(1)), 1);
    assert_eq!(log.append(entry(2)).unwrap().seq, 2);
    assert_eq!(fs.bytes("tomb/1/1.log").unwrap(), frames(1, 1..=2));
}

#[test]
fn a_failed_store_in_the_middle_of_a_batch_cuts_every_file_back() {
    let fs = fake();
    let log = open(&fs, 1, 2 * frame()).unwrap();
    log.store(&[rec(2, 1)]).unwrap();
    fs.fail(FsOp::Append, "tomb/2/3.log", 1);
    let batch: Vec<_> = (2..=5).map(|s| rec(2, s)).collect();
    let err = log.store(&batch).err().unwrap();
    assert_eq!(io_of(&err), ("append", "tomb/2/3.log"));
    assert_eq!(fs.bytes("tomb/2/1.log").unwrap(), frames(2, 1..=1));
    assert_eq!(log.last(NodeId(2)), 1);
    assert_eq!(log.store(&batch).unwrap(), 4);
    assert_eq!(seqs(&log.read_all(NodeId(2)).unwrap()), [1, 2, 3, 4, 5]);
}

#[test]
fn concurrent_appends_get_distinct_consecutive_sequences() {
    let fs = fake();
    let log = open(&fs, 1, 10 * frame()).unwrap();
    let mut got: Vec<u64> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    (0..25)
                        .map(|n| log.append(entry(n)).unwrap().seq)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    got.sort_unstable();
    assert_eq!(got, (1..=100).collect::<Vec<_>>());
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), got);
}

#[test]
fn on_a_real_directory_a_torn_tail_is_cut_and_the_log_continues() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tomblog-dir");
    let fs = Arc::new(DirFs::new(dir.path()));
    let log = TombLog::open(fs.clone(), NodeId(1), 2 * frame()).unwrap();
    for n in 1..=3 {
        log.append(entry(n)).unwrap();
    }
    fs.append("tomb/1/3.log", &enc(&rec(1, 4))[..10]).unwrap();
    drop(log);
    let log = TombLog::open(fs.clone(), NodeId(1), 2 * frame()).unwrap();
    assert_eq!(log.last(NodeId(1)), 3);
    assert_eq!(fs.len("tomb/1/3.log").unwrap(), frame());
    assert_eq!(log.append(entry(4)).unwrap().seq, 4);
    assert_eq!(log.compact(NodeId(1), &|_| true).unwrap(), 1);
    assert_eq!(fs.list("tomb/1").unwrap(), ["3.log"]);
    assert_eq!(seqs(&log.read_all(NodeId(1)).unwrap()), [3, 4]);
}
