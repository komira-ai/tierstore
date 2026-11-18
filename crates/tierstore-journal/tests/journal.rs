//! The journal's life: open (new, clean, replayed, discarded), folds, and I/O failures at every
//! step (section 8).
//!
//! Catches: a torn journal tail that later appends would follow (and a replay then stop before
//! them); a stale journal tail replayed after a fold; a snapshot of another pack, a corrupt one,
//! one behind its journal, a record of an unknown kind or out of the pack, trusted instead of
//! discarded; a discard that leaves the data file's blocks allocated or reuses an epoch; a fold
//! that is not every 64 records; a failed I/O that a later call ignores; an open that cannot
//! read its files reported as anything but the error.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;

use support::*;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EntryError, EntrySpec};
use tierstore_fetch_api::ids::{CellRun, PackKey};
use tierstore_journal::fake::{Crash, Keep, MemFs, Op};
use tierstore_journal::format::{
    FormatError, HEADER_LEN, JournalHeader, RECORD_LEN, Record, RecordKind, Snapshot,
};
use tierstore_journal::{DiscardReason, EntryFiles, FOLD_EVERY, FileKind, Trust};

/// An entry with cells 0 and 1 committed in boot A, closed (folded), on its file system.
fn closed() -> MemFs {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&e, &[0, 1], 1).unwrap();
    e.commit_batch().unwrap();
    e.close().unwrap();
    fs.crash(Crash::Process)
}

fn snapshot_of(fs: &MemFs) -> Snapshot {
    Snapshot::decode(&fs.current(FileKind::Snapshot).unwrap()).unwrap()
}

#[test]
fn a_new_entry_starts_empty_with_a_snapshot_and_a_journal_of_epoch_zero() {
    let fs = MemFs::new();
    let (e, rec) = open(&fs, Some(BOOT_A)).unwrap();
    assert!(rec.fresh);
    assert!(rec.folded);
    assert_eq!(rec.discarded, None);
    assert_eq!(rec.count(CellState::Absent), CELLS);
    assert_eq!(e.journal().epoch(), 0);
    let snap = snapshot_of(&fs);
    assert_eq!((snap.epoch, snap.boot, snap.size), (0, Some(BOOT_A), SIZE));
    assert_eq!(snap.pack, pack());
    let jnl = fs.durable(FileKind::Journal).unwrap();
    assert_eq!(
        JournalHeader::decode(&jnl).unwrap(),
        JournalHeader {
            epoch: 0,
            boot: Some(BOOT_A)
        }
    );
    // The journal's and the snapshot's names are durable (the directory was synced after both).
    assert!(fs.durable(FileKind::Snapshot).is_some());
    assert_eq!(fs.durable(FileKind::SnapshotTemp), None);
    assert_eq!(
        fs.count(Op::PunchData, Some(FileKind::Data)),
        0,
        "a new entry punches nothing"
    );
}

#[test]
fn a_reopen_after_records_folds_them_into_the_next_epoch() {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&e, &[0, 1, 3], 1).unwrap();
    e.commit_batch().unwrap();
    let (e2, rec) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(rec.records, 2, "runs 0..2 and 3");
    assert!(rec.folded);
    assert_eq!(e2.journal().epoch(), 1);
    let snap = snapshot_of(e2.fs());
    assert_eq!(snap.present.iter().collect::<Vec<_>>(), [0, 1, 3]);
    assert!(snap.recheck.is_empty());
}

#[test]
fn a_torn_journal_tail_is_dropped_before_any_new_record() {
    let fs = closed();
    let mut jnl = fs.current(FileKind::Journal).unwrap();
    // Half a record: an append the crash tore.
    jnl.extend_from_slice(
        &Record {
            kind: RecordKind::Add,
            run: CellRun { first: 2, count: 1 },
        }
        .encode(1)[..7],
    );
    fs.write_at(FileKind::Journal, 0, &jnl).unwrap();
    let (e, rec) = open(&fs, Some(BOOT_A)).unwrap();
    assert_eq!(rec.tail_bytes, 7);
    assert!(rec.folded);
    // The new journal is a bare header of the next epoch, so a record appended now is read.
    assert_eq!(e.fs().current(FileKind::Journal).unwrap().len(), HEADER_LEN);
    publish(&e, &[2], 1).unwrap();
    e.commit_batch().unwrap();
    let (_, rec) = open(&e.fs().crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(rec.records, 1);
    assert_eq!(restored(&rec)[..3], [CellState::Present; 3]);
}

#[test]
fn a_journal_left_from_an_earlier_epoch_is_not_replayed() {
    let fs = closed();
    // The journal of epoch 0 (with a PUNCH of cell 0) is put back after the fold to epoch 1
    // reached the disk: its records are already in the snapshot and must not apply again.
    let epoch = snapshot_of(&fs).epoch;
    assert_eq!(epoch, 1, "the open folded to 0, the close to 1");
    let stale = [
        JournalHeader {
            epoch: epoch - 1,
            boot: Some(BOOT_A),
        }
        .encode()
        .to_vec(),
        Record {
            kind: RecordKind::Punch,
            run: CellRun { first: 0, count: 1 },
        }
        .encode(epoch - 1)
        .to_vec(),
    ]
    .concat();
    fs.set_len(FileKind::Journal, 0).unwrap();
    fs.write_at(FileKind::Journal, 0, &stale).unwrap();
    let (_, rec) = open(&fs, Some(BOOT_A)).unwrap();
    assert_eq!(rec.records, 0);
    assert_eq!(
        restored(&rec)[..2],
        [CellState::Present, CellState::Present]
    );
}

#[test]
fn records_of_an_earlier_epoch_after_a_new_header_end_the_replay() {
    let fs = closed();
    let epoch = snapshot_of(&fs).epoch;
    // A new header whose truncate did not reach the disk: the old epoch's records follow it.
    let bytes = [
        JournalHeader {
            epoch,
            boot: Some(BOOT_A),
        }
        .encode()
        .to_vec(),
        Record {
            kind: RecordKind::Add,
            run: CellRun { first: 2, count: 2 },
        }
        .encode(epoch - 1)
        .to_vec(),
    ]
    .concat();
    fs.write_at(FileKind::Journal, 0, &bytes).unwrap();
    let (_, rec) = open(&fs, Some(BOOT_A)).unwrap();
    assert_eq!(rec.records, 0);
    assert_eq!(rec.tail_bytes, RECORD_LEN as u64);
    assert_eq!(restored(&rec)[2..], [CellState::Absent, CellState::Absent]);
}

#[test]
fn a_missing_or_unreadable_journal_header_replays_nothing_and_trusts_the_snapshot() {
    for damage in [None, Some(b"garbage".to_vec())] {
        let fs = closed();
        match &damage {
            None => fs.set_len(FileKind::Journal, 0).unwrap(),
            Some(bytes) => fs.write_at(FileKind::Journal, 0, bytes).unwrap(),
        }
        let (_, rec) = open(&fs, Some(BOOT_A)).unwrap();
        assert_eq!(rec.trust, Trust::SameBoot);
        assert!(rec.folded);
        assert_eq!(
            restored(&rec)[..2],
            [CellState::Present, CellState::Present],
            "{damage:?}"
        );
    }
}

/// Opens `fs` and returns why it discarded the entry, after checking the discard's effects.
fn discarded(fs: &MemFs, spec: &EntrySpec) -> DiscardReason {
    let epochs_before = [
        fs.current(FileKind::Snapshot)
            .and_then(|b| Snapshot::decode(&b).ok())
            .map(|s| s.epoch),
        fs.current(FileKind::Journal)
            .and_then(|b| JournalHeader::decode(&b).ok())
            .map(|h| h.epoch),
    ];
    let (e, rec) = tierstore_journal::fake::FsEntry::open(fs.clone(), spec, Some(BOOT_A)).unwrap();
    assert!(!rec.fresh);
    assert_eq!(
        rec.count(CellState::Absent),
        CELLS,
        "a discarded entry starts empty"
    );
    assert_eq!(
        fs.count(Op::PunchData, Some(FileKind::Data)),
        1,
        "the data file is punched whole"
    );
    assert!(fs.current(FileKind::Data).unwrap().iter().all(|b| *b == 0));
    let new_epoch = e.journal().epoch();
    for before in epochs_before.into_iter().flatten() {
        assert!(new_epoch > before, "epoch {new_epoch} reuses {before}");
    }
    rec.discarded.expect("discarded")
}

#[test]
fn a_corrupt_snapshot_is_discarded() {
    let fs = closed();
    let mut snap = fs.current(FileKind::Snapshot).unwrap();
    snap[50] ^= 1;
    fs.write_at(FileKind::Snapshot, 0, &snap).unwrap();
    assert_eq!(
        discarded(&fs, &spec()),
        DiscardReason::Snapshot(FormatError::Crc { what: "snapshot" })
    );
}

#[test]
fn a_snapshot_of_another_pack_or_size_is_discarded() {
    let mut other = spec();
    other.pack.key = PackKey("packs/other".to_owned());
    assert_eq!(discarded(&closed(), &other), DiscardReason::Identity);
    let mut bigger = spec();
    bigger.size += 1;
    assert_eq!(discarded(&closed(), &bigger), DiscardReason::Identity);
}

#[test]
fn a_journal_ahead_of_its_snapshot_is_discarded() {
    let fs = closed();
    let epoch = snapshot_of(&fs).epoch;
    let header = JournalHeader {
        epoch: epoch + 1,
        boot: Some(BOOT_A),
    }
    .encode();
    fs.write_at(FileKind::Journal, 0, &header).unwrap();
    assert_eq!(
        discarded(&fs, &spec()),
        DiscardReason::JournalAhead {
            journal: epoch + 1,
            snapshot: epoch
        }
    );
}

#[test]
fn a_record_of_an_unknown_kind_or_outside_the_pack_is_discarded() {
    let epoch = snapshot_of(&closed()).epoch;
    let header = JournalHeader {
        epoch,
        boot: Some(BOOT_A),
    }
    .encode()
    .to_vec();
    let mut unknown = Record {
        kind: RecordKind::Add,
        run: CellRun { first: 0, count: 1 },
    }
    .encode(epoch);
    unknown[0] = 9;
    let crc = crc32c::crc32c_append(crc32c::crc32c(&epoch.to_le_bytes()), &unknown[..12]);
    unknown[12..].copy_from_slice(&crc.to_le_bytes());
    let outside = Record {
        kind: RecordKind::Add,
        run: CellRun { first: 3, count: 2 },
    }
    .encode(epoch);
    for (record, reason) in [
        (unknown.to_vec(), DiscardReason::UnknownRecord(9)),
        (
            outside.to_vec(),
            DiscardReason::RunOutsidePack(CellRun { first: 3, count: 2 }),
        ),
    ] {
        let fs = closed();
        fs.write_at(FileKind::Journal, 0, &[header.clone(), record].concat())
            .unwrap();
        assert_eq!(discarded(&fs, &spec()), reason);
    }
}

#[test]
fn the_journal_is_folded_every_sixty_four_records() {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    // Each commit of one cell, punched in between, appends one record: ADD, PUNCH, ADD, ...
    let cell = CellRun { first: 0, count: 1 };
    let mut appended = 0;
    let mut residency = 1;
    while appended < FOLD_EVERY - 2 {
        publish(&e, &[0], residency).unwrap();
        e.commit_batch().unwrap();
        evict(&e, &[cell]).unwrap();
        appended += 2;
        residency += 1;
    }
    assert_eq!(e.journal().epoch(), 0);
    publish(&e, &[0], residency).unwrap();
    e.commit_batch().unwrap();
    assert_eq!(e.journal().epoch(), 0, "63 records");
    evict(&e, &[cell]).unwrap();
    assert_eq!(e.journal().epoch(), 1, "the 64th record folds");
    assert_eq!(e.fs().current(FileKind::Journal).unwrap().len(), HEADER_LEN);
    // A commit of 64 runs at once folds after it.
    let fs = MemFs::new();
    let big = EntrySpec {
        pack: pack(),
        size: 128 * MIB,
    };
    let (e, _) = tierstore_journal::fake::FsEntry::open(fs, &big, Some(BOOT_A)).unwrap();
    for c in (0..128).step_by(2) {
        let t = e.claim(CellRun { first: c, count: 1 }).pop().unwrap();
        e.publish(t, &vec![1; MIB as usize]).unwrap();
    }
    assert_eq!(e.commit_batch().unwrap().records, FOLD_EVERY);
    assert_eq!(e.journal().epoch(), 1);
}

#[test]
fn a_failure_at_any_write_of_a_fold_fails_the_journal_until_it_is_reopened() {
    // The fold of a close: snapshot temp truncate, write, sync, rename, directory sync, journal
    // truncate, header write, sync.
    let steps = [
        (
            Op::SetLen,
            Some(FileKind::SnapshotTemp),
            "truncate the snapshot temp",
        ),
        (
            Op::WriteAt,
            Some(FileKind::SnapshotTemp),
            "write the snapshot temp",
        ),
        (
            Op::SyncData,
            Some(FileKind::SnapshotTemp),
            "fdatasync the snapshot temp",
        ),
        (Op::ReplaceSnapshot, None, "rename the snapshot"),
        (Op::SyncDir, None, "fsync the entry directory"),
        (Op::SetLen, Some(FileKind::Journal), "truncate the journal"),
        (
            Op::WriteAt,
            Some(FileKind::Journal),
            "write the journal header",
        ),
        (
            Op::SyncData,
            Some(FileKind::Journal),
            "fdatasync the journal",
        ),
    ];
    for (op, file, name) in steps {
        let fs = MemFs::new();
        let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
        publish(&e, &[0], 1).unwrap();
        e.commit_batch().unwrap();
        fs.fail(op, file, 0, ErrorKind::Other);
        let err = e.close().unwrap_err();
        assert!(
            matches!(err, EntryError::Io { op, .. } if op == name),
            "{name}: {err}"
        );
        assert!(e.journal().failed());
        assert!(e.commit_batch().is_err());
        assert!(e.close().is_err());
        // What reached the disk still restores the committed cell.
        let (_, rec) = open(&fs.crash(Crash::Power(Keep::Nothing)), Some(BOOT_B)).unwrap();
        assert_eq!(restored(&rec)[0], CellState::Recheck, "{name}");
    }
}

#[test]
fn the_first_fold_names_the_journal_durably_and_a_failure_there_fails_the_open() {
    // Mutations of a new entry's open: data set_len, directory sync (the store's), then the
    // fold's eight and its second directory sync, for the journal's new name.
    let fs = MemFs::new();
    fs.fail(Op::SyncDir, None, 2, ErrorKind::Other);
    let err = open(&fs, Some(BOOT_A)).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "fsync the entry directory",
                ..
            }
        ),
        "{err}"
    );
    let fs = MemFs::new();
    open(&fs, Some(BOOT_A)).unwrap();
    assert_eq!(fs.count(Op::SyncDir, None), 3);
    assert!(fs.durable(FileKind::Journal).is_some());
}

#[test]
fn an_open_that_cannot_read_its_files_or_punch_a_discard_fails() {
    for (op, file, name) in [
        (Op::Read, Some(FileKind::Snapshot), "read the snapshot"),
        (Op::Read, Some(FileKind::Journal), "read the journal"),
    ] {
        let fs = closed();
        fs.fail(op, file, 0, ErrorKind::PermissionDenied);
        let err = open(&fs, Some(BOOT_A)).unwrap_err();
        assert!(
            matches!(&err, EntryError::Io { op, .. } if *op == name),
            "{err}"
        );
    }
    let fs = closed();
    fs.corrupt(FileKind::Snapshot, 0, 0xff);
    fs.fail(Op::PunchData, None, 0, ErrorKind::Other);
    let err = open(&fs, Some(BOOT_A)).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "punch the discarded data file",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn an_open_of_a_pack_too_large_for_u32_cells_is_refused() {
    let spec = EntrySpec {
        pack: pack(),
        size: (u64::from(u32::MAX) + 1) << 20,
    };
    let err = tierstore_journal::EntryJournal::open(MemFs::new(), &spec, Some(BOOT_A)).unwrap_err();
    assert!(matches!(err, EntryError::TooLarge { .. }), "{err}");
}

#[test]
fn a_failed_journal_append_of_a_commit_fails_the_journal() {
    for op in [Op::WriteAt, Op::SyncData] {
        let fs = MemFs::new();
        let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
        publish(&e, &[0], 1).unwrap();
        fs.fail(op, Some(FileKind::Journal), 0, ErrorKind::Other);
        assert!(e.commit_batch().is_err(), "{op:?}");
        assert!(e.journal().failed());
        // The durable bit was set (the data was synced) but no restart trusts the cell
        // unless the record reached the disk.
        let (_, rec) = open(&fs.crash(Crash::Power(Keep::Nothing)), Some(BOOT_B)).unwrap();
        assert_eq!(restored(&rec)[0], CellState::Absent, "{op:?}");
    }
}

#[test]
fn a_fold_drops_the_cells_whose_recheck_failed_and_keeps_pending_ones_pending() {
    let fs = closed();
    let lost = fs.crash(Crash::Power(Keep::Nothing));
    let (e, _) = open(&lost, Some(BOOT_B)).unwrap();
    assert!(e.settle(0, tierstore_fetch_api::entry::Settle::RecheckFailed));
    publish(&e, &[2], 1).unwrap();
    e.journal().fold(77).unwrap();
    assert_eq!(e.journal().pending(), 1);
    let snap = snapshot_of(e.fs());
    assert_eq!(snap.recheck.iter().collect::<Vec<_>>(), [1]);
    assert_eq!(snap.last_use, 77);
    assert!(snap.present.is_empty());
}

#[test]
fn a_journal_header_of_another_boot_than_its_snapshot_is_folded_into_this_boot() {
    let fs = closed();
    let epoch = snapshot_of(&fs).epoch;
    fs.write_at(
        FileKind::Journal,
        0,
        &JournalHeader {
            epoch,
            boot: Some(BOOT_B),
        }
        .encode(),
    )
    .unwrap();
    let (e, rec) = open(&fs, Some(BOOT_A)).unwrap();
    assert!(rec.folded);
    assert_eq!(
        rec.trust,
        Trust::SameBoot,
        "no record of the other boot was replayed"
    );
    assert_eq!(
        restored(&rec)[..2],
        [CellState::Present, CellState::Present]
    );
    let header = JournalHeader::decode(&e.fs().current(FileKind::Journal).unwrap()).unwrap();
    assert_eq!(
        header,
        JournalHeader {
            epoch: epoch + 1,
            boot: Some(BOOT_A)
        }
    );
}

#[test]
fn a_cell_already_durable_is_not_committed_again() {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&e, &[0], 1).unwrap();
    e.commit_batch().unwrap();
    // A second notice for the same residency (a caller that tells the journal twice).
    e.journal().published(0);
    assert_eq!(
        e.commit_batch().unwrap(),
        tierstore_fetch_api::entry::CommitReport::default()
    );
    assert_eq!(fs.count(Op::SyncData, Some(FileKind::Data)), 1);
}

/// An entry whose journal has 63 records since its last fold (cell 0 committed and punched in
/// turn), with cell 0 committed and `evicting`.
fn one_record_before_a_fold() -> (MemFs, tierstore_journal::fake::FsEntry) {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    let cell = CellRun { first: 0, count: 1 };
    let mut residency = 1;
    for _ in 0..(FOLD_EVERY - 1) / 2 {
        publish(&e, &[0], residency).unwrap();
        e.commit_batch().unwrap();
        evict(&e, &[cell]).unwrap();
        residency += 1;
    }
    publish(&e, &[0], residency).unwrap();
    e.commit_batch().unwrap();
    assert_eq!(e.journal().epoch(), 0);
    (fs, e)
}

#[test]
fn a_fold_that_fails_after_a_punch_or_a_commit_is_done_fails_the_journal_not_the_call() {
    let (fs, e) = one_record_before_a_fold();
    assert!(e.evict_cell_cas(0, tierstore_fetch_api::entry::EvictStep::Begin));
    fs.fail(
        Op::SetLen,
        Some(FileKind::SnapshotTemp),
        0,
        ErrorKind::Other,
    );
    let report = e.punch(&[CellRun { first: 0, count: 1 }]).unwrap();
    assert_eq!(report.records, 1);
    assert!(
        e.journal().failed(),
        "the next call reports the fold's failure"
    );
    assert!(e.commit_batch().is_err());
    // The punch is durable: the cell is not restored.
    let (_, rec) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(restored(&rec)[0], CellState::Absent);

    // The 64th record comes from a commit instead.
    let (fs, e) = one_record_before_a_fold();
    publish(&e, &[2], 1).unwrap();
    fs.fail(
        Op::SetLen,
        Some(FileKind::SnapshotTemp),
        0,
        ErrorKind::Other,
    );
    let report = e.commit_batch().unwrap();
    assert_eq!((report.cells_durable, report.records), (1, 1));
    assert!(e.journal().failed());
    assert!(e.close().is_err());
    let (_, rec) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(
        restored(&rec)[2],
        CellState::Present,
        "the commit is durable"
    );
}

#[test]
fn a_journal_left_by_a_kill_before_its_name_was_synced_is_named_before_records_follow() {
    // A new entry's open: data set_len and directory sync, then the fold's 8 steps and the
    // directory sync of the journal's new name. Kill before that last sync.
    let fs = MemFs::new();
    fs.die_after(10);
    assert!(open(&fs, Some(BOOT_A)).is_err());
    // The same boot: the journal is visible and clean, so the open does not fold, but it syncs
    // the directory before a record goes into the journal.
    let restarted = fs.crash(Crash::Process);
    let (e, rec) = open(&restarted, Some(BOOT_A)).unwrap();
    assert!(!rec.folded);
    publish(&e, &[1], 1).unwrap();
    e.commit_batch().unwrap();
    // A power loss now keeps the journal's name and its record.
    let (_, rec) = open(&restarted.crash(Crash::Power(Keep::Nothing)), Some(BOOT_B)).unwrap();
    assert_eq!(restored(&rec)[1], CellState::Recheck);
}

#[test]
fn a_clean_open_that_cannot_sync_its_directory_fails() {
    let fs = closed();
    fs.fail(Op::SyncDir, None, 0, ErrorKind::Other);
    let err = open(&fs, Some(BOOT_A)).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "fsync the entry directory",
                ..
            }
        ),
        "{err}"
    );
}
