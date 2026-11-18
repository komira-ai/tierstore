//! Boot id and `recheck` (section 8; section 14 row "Process crash, same boot: restored cells
//! `present`, no recheck; different boot or no boot_id: every restored cell `recheck`, not served
//! by the mount, corrupted cell refetched").
//!
//! Catches: a same-boot restart that re-hashes (or a restored cell without its `durable` bit);
//! a restart in another boot, or with no boot id on either side, that trusts a cell; a `recheck`
//! cell the mount's read path would serve; a corrupted cell served after its re-hash, or not
//! fetchable again; a fold in the new boot that turns unverified cells into trusted ones; a
//! re-hashed cell that is rechecked again at every restart.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::*;
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{Entry, Settle};
use tierstore_fetch_api::ids::CellRun;
use tierstore_journal::fake::{Crash, FsEntry, Keep, MemFs};
use tierstore_journal::{BootId, FileKind, Trust};

/// An entry with cells 0..3 committed and cell 3 published but not committed, in boot A.
fn written() -> (MemFs, FsEntry) {
    let fs = MemFs::new();
    let (entry, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&entry, &[0, 1, 2], 1).unwrap();
    entry.commit_batch().unwrap();
    publish(&entry, &[3], 1).unwrap();
    (fs, entry)
}

/// What the server's recheck does for one cell (the hash pool at low priority, 4.7): read the
/// bytes on disk, re-hash, settle. Whether they matched.
fn recheck(entry: &FsEntry, cell: u32, residency: u32) -> bool {
    let bytes = entry.read_cell(cell).unwrap();
    let ok = digest(&bytes) == digest(&cell_bytes(cell, residency));
    let how = if ok {
        Settle::RecheckOk
    } else {
        Settle::RecheckFailed
    };
    assert!(entry.settle(cell, how));
    ok
}

#[test]
fn a_process_crash_in_the_same_boot_restores_committed_cells_present_and_durable() {
    let (fs, _entry) = written();
    let (after, rec) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(rec.trust, Trust::SameBoot);
    assert_eq!(rec.count(CellState::Recheck), 0);
    for c in 0..3 {
        assert_eq!(after.state().load(c), Some(CellByte(0x82)), "cell {c}");
        assert!(after.state().ready(CellRun { first: c, count: 1 }));
    }
    // Published but never committed: not restored.
    assert_eq!(state(&after, 3), Some(CellState::Absent));
}

#[test]
fn a_restart_in_another_boot_or_with_no_boot_id_restores_every_cell_recheck_unservable() {
    let (fs, _entry) = written();
    let cases: [(Option<BootId>, Option<BootId>); 3] = [
        (Some(BOOT_A), Some(BOOT_B)), // another boot
        (Some(BOOT_A), None),         // the restart cannot read its boot id
        (None, Some(BOOT_A)),         // the files were written with no boot id
    ];
    for (written_in, now) in cases {
        let fs = if written_in.is_some() {
            fs.crash(Crash::Process)
        } else {
            let fs = MemFs::new();
            let (e, _) = open(&fs, None).unwrap();
            publish(&e, &[0, 1, 2], 1).unwrap();
            e.commit_batch().unwrap();
            fs.crash(Crash::Process)
        };
        let (after, rec) = open(&fs, now).unwrap();
        assert_eq!(rec.trust, Trust::Recheck, "{written_in:?} -> {now:?}");
        assert_eq!(
            rec.count(CellState::Recheck),
            3,
            "{written_in:?} -> {now:?}"
        );
        assert_eq!(
            rec.count(CellState::Present),
            0,
            "{written_in:?} -> {now:?}"
        );
        for c in 0..3 {
            let byte = after.state().load(c).unwrap();
            assert_eq!(byte, CellByte(0x03));
            assert!(!byte.servable(), "the mount does not serve a recheck cell");
            assert!(!after.state().ready(CellRun { first: c, count: 1 }));
            // A recheck cell is not claimable: its bytes are on disk.
            assert!(after.claim(CellRun { first: c, count: 1 }).is_empty());
        }
    }
}

#[test]
fn a_corrupted_cell_fails_its_recheck_is_fetched_again_and_the_good_ones_become_present() {
    let (fs, _entry) = written();
    let lost = fs.crash(Crash::Power(Keep::Nothing));
    // The device lost a write it had acknowledged: one byte of cell 1 is wrong.
    lost.corrupt(FileKind::Data, MIB + 17, 0x40);
    let (after, _) = open(&lost, Some(BOOT_B)).unwrap();
    assert!(recheck(&after, 0, 1));
    assert!(!recheck(&after, 1, 1));
    assert_eq!(state(&after, 0), Some(CellState::Present));
    assert_eq!(state(&after, 1), Some(CellState::Absent));
    // The corrupted cell is claimable and is fetched again.
    publish(&after, &[1], 1).unwrap();
    let report = after.commit_batch().unwrap();
    // Cell 0 (re-hashed) and cell 1 (refetched) are committed in this boot.
    assert_eq!(report.cells_durable, 2);
    assert_eq!(report.records, 1);
    // Cell 2 was never re-hashed: after a process crash in boot B it is still recheck, and
    // cells 0 and 1 are trusted.
    let (again, rec) = open(&lost.crash(Crash::Process), Some(BOOT_B)).unwrap();
    assert_eq!(
        restored(&rec),
        [
            CellState::Present,
            CellState::Present,
            CellState::Recheck,
            CellState::Absent
        ]
    );
    assert_eq!(again.read_cell(1).unwrap(), cell_bytes(1, 1));
}

#[test]
fn a_fold_in_the_new_boot_keeps_unverified_cells_recheck() {
    let (fs, _entry) = written();
    let (after, rec) = open(&fs.crash(Crash::Power(Keep::Nothing)), Some(BOOT_B)).unwrap();
    assert!(
        rec.folded,
        "the open in another boot writes a snapshot of boot B"
    );
    after.close().unwrap();
    // Same boot B again: the snapshot is boot B's, but its cells were never re-hashed.
    let (_, rec) = open(&after.fs().crash(Crash::Process), Some(BOOT_B)).unwrap();
    assert_eq!(rec.count(CellState::Recheck), 3);
    assert_eq!(rec.count(CellState::Present), 0);
}

#[test]
fn a_failed_recheck_is_not_rechecked_again_after_a_restart() {
    let (fs, _entry) = written();
    let lost = fs.crash(Crash::Power(Keep::Nothing));
    lost.corrupt(FileKind::Data, 5, 0x01);
    let (after, _) = open(&lost, Some(BOOT_B)).unwrap();
    assert!(!recheck(&after, 0, 1));
    after.close().unwrap();
    let (_, rec) = open(&lost.crash(Crash::Process), Some(BOOT_B)).unwrap();
    assert_eq!(
        restored(&rec),
        [
            CellState::Absent,
            CellState::Recheck,
            CellState::Recheck,
            CellState::Absent
        ]
    );
}

#[test]
fn a_failed_recheck_is_dropped_by_the_next_commit_too() {
    let (fs, _entry) = written();
    let lost = fs.crash(Crash::Power(Keep::Nothing));
    lost.corrupt(FileKind::Data, 5, 0x01);
    let (after, _) = open(&lost, Some(BOOT_B)).unwrap();
    assert!(!recheck(&after, 0, 1));
    assert!(after.journal().to_recheck().contains(0));
    // Nothing to make durable, yet the commit forgets the failed cell.
    assert_eq!(after.commit_batch().unwrap().data_syncs, 0);
    assert!(!after.journal().to_recheck().contains(0));
}

#[test]
fn expiring_a_bad_cell_changes_nothing_the_journal_holds() {
    let fs = MemFs::new();
    let (entry, _) = open(&fs, Some(BOOT_A)).unwrap();
    let token = entry.claim(CellRun { first: 0, count: 1 }).pop().unwrap();
    entry.mark_bad(token);
    assert!(entry.settle(0, Settle::ExpireBad));
    assert_eq!(entry.journal().pending(), 0);
    assert!(entry.journal().to_recheck().is_empty());
    // A settle that does not win tells the journal nothing.
    assert!(!entry.settle(0, Settle::RecheckOk));
    assert_eq!(entry.journal().pending(), 0);
}

#[test]
fn a_same_boot_restart_of_an_entry_closed_cleanly_writes_nothing_but_a_directory_sync() {
    let (fs, entry) = written();
    entry.commit_batch().unwrap();
    entry.close().unwrap();
    let crashed = fs.crash(Crash::Process);
    let (_, rec) = open(&crashed, Some(BOOT_A)).unwrap();
    assert!(!rec.folded);
    // It writes no file; it syncs the directory once, so the journal's name is durable before
    // a record is appended to it.
    assert_eq!(crashed.mutations(), 1);
    assert_eq!(crashed.count(tierstore_journal::fake::Op::SyncDir, None), 1);
    assert_eq!(rec.count(CellState::Present), 4);
}
