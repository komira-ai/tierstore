//! [`DirFiles`]: an entry's files in a directory (Linux), under a scratch directory of the test.
//!
//! Catches: files named other than `<stem>.pack`, `.jnl`, `.meta`, `.meta.tmp`; a missing file
//! read as an error instead of `None` (or another error read as `None`); a punch that changes the
//! size or leaves the bytes; a rename that does not replace the snapshot; a journal that does not
//! survive a reopen of the directory; the data descriptor lent out not being the file's.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;

use support::*;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::ids::CellRun;
use tierstore_journal::{DirFiles, EntryFiles, EntryJournal, FileKind, Trust};
use tierstore_testkit::dir::TestDir;

#[test]
fn the_files_are_named_by_stem_and_kind_and_a_missing_one_reads_as_none() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "dir-files-names");
    let files = DirFiles::open(&dir, "ab12").unwrap();
    assert_eq!(files.path(FileKind::Data), dir.join("ab12.pack"));
    assert_eq!(
        files.path(FileKind::SnapshotTemp),
        dir.join("ab12.meta.tmp")
    );
    assert!(dir.join("ab12.pack").exists(), "open creates the data file");
    assert_eq!(files.read(FileKind::Journal).unwrap(), None);
    files.write_at(FileKind::Journal, 3, b"xy").unwrap();
    files.sync_data(FileKind::Journal).unwrap();
    assert_eq!(files.read(FileKind::Journal).unwrap().unwrap(), b"\0\0\0xy");
    files.set_len(FileKind::Journal, 1).unwrap();
    assert_eq!(std::fs::read(dir.join("ab12.jnl")).unwrap(), b"\0");
    assert_eq!(
        files.sync_data(FileKind::Snapshot).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    // A directory where the snapshot should be: an error that is not "missing".
    std::fs::create_dir(dir.join("ab12.meta")).unwrap();
    assert!(files.read(FileKind::Snapshot).is_err());
}

#[test]
fn a_punch_reads_as_zeros_and_keeps_the_size_and_a_rename_replaces_the_snapshot() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "dir-files-punch");
    let files = DirFiles::open(&dir, "e").unwrap();
    files.set_len(FileKind::Data, 3 * MIB).unwrap();
    files
        .write_at(FileKind::Data, 0, &vec![7; 3 * MIB as usize])
        .unwrap();
    files.sync_data(FileKind::Data).unwrap();
    files.punch_data(MIB, MIB).unwrap();
    files.punch_data(0, 0).unwrap();
    let data = std::fs::read(files.path(FileKind::Data)).unwrap();
    assert_eq!(data.len() as u64, 3 * MIB);
    assert!(data[..MIB as usize].iter().all(|b| *b == 7));
    assert!(data[MIB as usize..2 * MIB as usize].iter().all(|b| *b == 0));
    assert_eq!(files.data().metadata().unwrap().len(), 3 * MIB);

    files.write_at(FileKind::Snapshot, 0, b"old").unwrap();
    files.write_at(FileKind::SnapshotTemp, 0, b"new").unwrap();
    files.replace_snapshot().unwrap();
    files.sync_dir().unwrap();
    assert_eq!(files.read(FileKind::Snapshot).unwrap().unwrap(), b"new");
    assert_eq!(files.read(FileKind::SnapshotTemp).unwrap(), None);
    assert_eq!(
        files.replace_snapshot().unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn a_punch_the_file_system_refuses_is_an_error() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "dir-files-bad-punch");
    let files = DirFiles::open(&dir, "e").unwrap();
    // A negative offset as the kernel sees it.
    assert!(files.punch_data(u64::MAX - 1, 1).is_err());
}

#[test]
fn a_missing_directory_fails_open_and_a_removed_one_fails_its_sync_and_writes() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "dir-files-perm");
    assert!(DirFiles::open(&dir.join("absent"), "e").is_err());
    let sub = dir.join("gone");
    std::fs::create_dir(&sub).unwrap();
    let files = DirFiles::open(&sub, "e").unwrap();
    std::fs::remove_file(sub.join("e.pack")).unwrap();
    std::fs::remove_dir(&sub).unwrap();
    assert!(files.sync_dir().is_err());
    assert!(files.write_at(FileKind::Journal, 0, b"x").is_err());
}

#[test]
fn an_entry_journaled_in_a_directory_survives_a_reopen_in_the_same_boot() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "dir-files-journal");
    let s = spec();
    {
        let files = DirFiles::open(&dir, "pack").unwrap();
        files.set_len(FileKind::Data, SIZE).unwrap();
        files.sync_dir().unwrap();
        let (journal, rec) = EntryJournal::open(files, &s, Some(BOOT_A)).unwrap();
        assert!(rec.fresh);
        // Commit cells 0 and 3 through a minimal state table, then punch cell 0.
        let cells = Cells::new();
        let reopened = DirFiles::open(&dir, "pack").unwrap();
        reopened
            .write_at(FileKind::Data, 0, &cell_bytes(0, 1))
            .unwrap();
        reopened
            .write_at(FileKind::Data, 3 * MIB, &cell_bytes(3, 1))
            .unwrap();
        for c in [0, 3] {
            cells.set(c, CellState::Present);
            journal.published(c);
        }
        let report = journal.commit_batch(&cells).unwrap();
        assert_eq!(
            (report.data_syncs, report.cells_durable, report.records),
            (1, 2, 2)
        );
        cells.set(0, CellState::Evicting);
        let report = journal
            .punch(&cells, &[CellRun { first: 0, count: 1 }])
            .unwrap();
        assert_eq!((report.journal_syncs, report.bytes_punched), (1, MIB));
    }
    let files = DirFiles::open(&dir, "pack").unwrap();
    let (_, rec) = EntryJournal::open(files, &s, Some(BOOT_A)).unwrap();
    assert_eq!(rec.trust, Trust::SameBoot);
    assert_eq!(
        restored(&rec),
        [
            CellState::Absent,
            CellState::Absent,
            CellState::Absent,
            CellState::Present
        ]
    );
    let data = std::fs::read(dir.join("pack.pack")).unwrap();
    assert_eq!(&data[3 * MIB as usize..], cell_bytes(3, 1).as_slice());
    assert!(data[..MIB as usize].iter().all(|b| *b == 0));
}

/// A state table of the tests' pack in plain cells.
struct Cells(std::sync::Mutex<Vec<tierstore_fetch_api::cell::CellByte>>);

impl Cells {
    fn new() -> Self {
        Self(std::sync::Mutex::new(vec![
            tierstore_fetch_api::cell::CellByte(0);
            CELLS as usize
        ]))
    }

    fn set(&self, cell: u32, s: CellState) {
        self.0.lock().unwrap()[cell as usize] = tierstore_fetch_api::cell::CellByte::new(s, false);
    }
}

impl tierstore_journal::CellStates for Cells {
    fn load(&self, cell: u32) -> Option<tierstore_fetch_api::cell::CellByte> {
        self.0.lock().unwrap().get(cell as usize).copied()
    }

    fn mark_durable(&self, cell: u32) -> bool {
        let mut b = self.0.lock().unwrap();
        match tierstore_fetch_api::cell::Transition::Durable.apply(b[cell as usize]) {
            Some(next) => {
                b[cell as usize] = next;
                true
            }
            None => false,
        }
    }

    fn last_use(&self) -> u64 {
        0
    }
}
