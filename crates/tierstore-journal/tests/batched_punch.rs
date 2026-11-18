//! Batched PUNCH with one journal fsync (5.1 step 4, section 8; section 14 row "Crash after a
//! batched PUNCH fsync and before/after the punch: replay never marks a punched cell `present`;
//! one fsync per batch").
//!
//! Catches: one journal sync per run (or per cell) instead of one per call; a punch issued
//! before the batch's records are synced; a crash after the sync and before the punch restoring
//! the cells; a punch of a cell that is not `evicting`, or out of the pack, that has any effect;
//! a failed `fallocate` that forgets the records are durable; a last short cell punched past
//! the pack's end.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;

use support::*;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EntryError, EvictStep, PunchReport};
use tierstore_fetch_api::ids::CellRun;
use tierstore_journal::FileKind;
use tierstore_journal::fake::{Crash, FsEntry, Keep, MemFs, Op};

/// An entry with every cell committed in boot A.
fn committed() -> (MemFs, FsEntry) {
    let fs = MemFs::new();
    let (entry, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&entry, &[0, 1, 2, 3], 1).unwrap();
    entry.commit_batch().unwrap();
    (fs, entry)
}

fn begin(entry: &FsEntry, cells: &[u32]) {
    for &c in cells {
        assert!(entry.evict_cell_cas(c, EvictStep::Begin));
    }
}

#[test]
fn a_batch_of_runs_is_journaled_with_one_fsync_before_any_punch() {
    let (fs, entry) = committed();
    let syncs = fs.count(Op::SyncData, Some(FileKind::Journal));
    begin(&entry, &[0, 2, 3]);
    let runs = [
        CellRun { first: 0, count: 1 },
        CellRun { first: 2, count: 2 },
    ];
    let report = entry.punch(&runs).unwrap();
    assert_eq!(
        report,
        PunchReport {
            journal_syncs: 1,
            records: 2,
            // Cells 0 and 2 whole, cell 3 is 1000 bytes: the punch stops at the pack's end.
            bytes_punched: 2 * MIB + 1000,
        }
    );
    assert_eq!(fs.count(Op::SyncData, Some(FileKind::Journal)), syncs + 1);
    assert_eq!(fs.count(Op::PunchData, Some(FileKind::Data)), 2);
    // `punch` does not change the state; the caller finishes each cell.
    assert_eq!(
        states(&entry),
        [
            CellState::Evicting,
            CellState::Present,
            CellState::Evicting,
            CellState::Evicting
        ]
    );
    let data = fs.current(FileKind::Data).unwrap();
    assert_eq!(data.len() as u64, SIZE, "the punch keeps the size");
    assert!(data[..MIB as usize].iter().all(|b| *b == 0));
    assert_eq!(
        &data[MIB as usize..2 * MIB as usize],
        cell_bytes(1, 1).as_slice()
    );
    assert!(data[2 * MIB as usize..].iter().all(|b| *b == 0));
}

#[test]
fn a_crash_after_the_punch_fsync_and_before_or_after_the_punch_never_restores_the_cells() {
    // Kill the punch call after its records are synced (before the first fallocate), after the
    // first fallocate, and after both.
    for punches_done in 0..=2u64 {
        let (fs, entry) = committed();
        begin(&entry, &[0, 2, 3]);
        let runs = [
            CellRun { first: 0, count: 1 },
            CellRun { first: 2, count: 2 },
        ];
        // The punch's mutations: the record append, the journal sync, then each fallocate.
        fs.die_after(2 + punches_done);
        let result = entry.punch(&runs);
        assert_eq!(
            result.is_ok(),
            punches_done == 2,
            "after {punches_done} punches"
        );
        for (how, boot) in [
            (Crash::Process, Some(BOOT_A)),
            (Crash::Power(Keep::Nothing), Some(BOOT_B)),
            (Crash::Power(Keep::Everything), Some(BOOT_B)),
            (Crash::Power(Keep::Seeded(3)), Some(BOOT_B)),
        ] {
            let (after, rec) = open(&fs.crash(how), boot).unwrap();
            assert_eq!(
                state(&after, 0),
                Some(CellState::Absent),
                "{punches_done} {how:?}"
            );
            assert_eq!(
                state(&after, 2),
                Some(CellState::Absent),
                "{punches_done} {how:?}"
            );
            assert_eq!(
                state(&after, 3),
                Some(CellState::Absent),
                "{punches_done} {how:?}"
            );
            assert_ne!(
                state(&after, 1),
                Some(CellState::Absent),
                "{punches_done} {how:?}: cell 1 was kept"
            );
            assert_eq!(rec.records, 3, "one ADD, two PUNCH: {punches_done} {how:?}");
        }
    }
}

#[test]
fn a_crash_before_the_punch_fsync_leaves_every_cell_present_with_its_bytes() {
    let (fs, entry) = committed();
    begin(&entry, &[1]);
    fs.die_after(1); // The record is written, its sync never runs.
    assert!(entry.punch(&[CellRun { first: 1, count: 1 }]).is_err());
    assert_eq!(
        fs.count(Op::PunchData, Some(FileKind::Data)),
        0,
        "no punch before the sync"
    );
    // Power loss with nothing unsynced kept: the record is gone and the bytes are not punched.
    let (after, _) = open(&fs.crash(Crash::Power(Keep::Nothing)), Some(BOOT_B)).unwrap();
    assert_eq!(states(&after), [CellState::Recheck; 4]);
    assert_eq!(after.read_cell(1).unwrap(), cell_bytes(1, 1));
}

#[test]
fn a_punch_refused_for_a_cell_not_evicting_or_out_of_the_pack_has_no_effect() {
    let (fs, entry) = committed();
    begin(&entry, &[0, 3]);
    let journal = fs.current(FileKind::Journal).unwrap();
    let err = entry.punch(&[CellRun { first: 0, count: 2 }]).unwrap_err();
    assert!(
        matches!(err, EntryError::NotEvicting { cell: 1, .. }),
        "{err}"
    );
    let err = entry.punch(&[CellRun { first: 3, count: 2 }]).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::CellOutOfRange {
                cell: 4,
                cells: 4,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(fs.current(FileKind::Journal).unwrap(), journal);
    assert_eq!(fs.count(Op::PunchData, Some(FileKind::Data)), 0);
    assert_eq!(after_restart(&fs)[0], CellState::Present);
}

fn after_restart(fs: &MemFs) -> Vec<CellState> {
    let (after, _) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    states(&after)
}

#[test]
fn no_runs_or_only_empty_runs_are_a_report_of_zeros_with_no_sync() {
    let (fs, entry) = committed();
    let syncs = fs.count(Op::SyncData, Some(FileKind::Journal));
    assert_eq!(entry.punch(&[]).unwrap(), PunchReport::default());
    assert_eq!(
        entry.punch(&[CellRun { first: 2, count: 0 }]).unwrap(),
        PunchReport::default()
    );
    assert_eq!(fs.count(Op::SyncData, Some(FileKind::Journal)), syncs);
}

#[test]
fn a_failed_fallocate_fails_the_call_but_the_durable_records_keep_the_cells_from_coming_back() {
    let (fs, entry) = committed();
    begin(&entry, &[1]);
    fs.fail(Op::PunchData, None, 0, ErrorKind::Unsupported);
    let err = entry.punch(&[CellRun { first: 1, count: 1 }]).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "punch the data file",
                ..
            }
        ),
        "{err}"
    );
    // The journal is still usable: a failed fallocate leaves its state on disk known.
    assert!(!entry.journal().failed());
    assert!(entry.evict_cell_cas(1, EvictStep::Finish));
    assert_eq!(after_restart(&fs)[1], CellState::Absent);
    publish(&entry, &[1], 2).unwrap();
    assert_eq!(entry.commit_batch().unwrap().cells_durable, 1);
    assert_eq!(after_restart(&fs)[1], CellState::Present);
}

#[test]
fn a_failed_journal_append_or_sync_fails_the_punch_and_the_journal_and_punches_nothing() {
    for op in [Op::WriteAt, Op::SyncData] {
        let (fs, entry) = committed();
        begin(&entry, &[1]);
        fs.fail(op, Some(FileKind::Journal), 0, ErrorKind::Other);
        assert!(
            entry.punch(&[CellRun { first: 1, count: 1 }]).is_err(),
            "{op:?}"
        );
        assert_eq!(fs.count(Op::PunchData, Some(FileKind::Data)), 0, "{op:?}");
        assert!(entry.journal().failed(), "{op:?}");
        let err = entry.punch(&[CellRun { first: 1, count: 1 }]).unwrap_err();
        assert!(
            err.to_string().contains("reopen the entry"),
            "{op:?}: {err}"
        );
    }
}

#[test]
fn a_cell_punched_and_published_again_is_restored_only_after_its_new_commit() {
    let (fs, entry) = committed();
    evict(&entry, &[CellRun { first: 2, count: 1 }]).unwrap();
    publish(&entry, &[2], 2).unwrap();
    assert_eq!(
        after_restart(&fs)[2],
        CellState::Absent,
        "published, not committed"
    );
    entry.commit_batch().unwrap();
    let (after, _) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(state(&after, 2), Some(CellState::Present));
    assert_eq!(after.read_cell(2).unwrap(), cell_bytes(2, 2));
}
