//! [`FsEntry`] follows the frozen `Entry` contract (section 2's CAS rules, holds, touch, readers,
//! `read_cell`), so the evictor's tests (T8) can run on it with a crashable file system.
//!
//! Catches: a publish that writes bytes for a cell it does not own, or leaves a cell `claimed`
//! after a failed write; a claim that wins a cell twice; evictor steps that win from the wrong
//! state; readers, holds and touches that do not count; a read of a cell that is not on disk.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;

use support::*;
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{CellCounts, Entry, EntryError, EvictStep, ReaderCounters};
use tierstore_fetch_api::ids::{CellRun, HolderId};
use tierstore_journal::FileKind;
use tierstore_journal::fake::{MemFs, Op};

fn fresh() -> (MemFs, tierstore_journal::fake::FsEntry) {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    (fs, e)
}

#[test]
fn claims_publish_once_and_a_failed_publish_returns_the_cell_to_absent() {
    let (fs, e) = fresh();
    assert_eq!(
        (e.pack(), e.size(), e.state().cell_count()),
        (&pack(), SIZE, CELLS)
    );
    let won = e.claim(CellRun { first: 0, count: 3 });
    assert_eq!(won.iter().map(|t| t.cell()).collect::<Vec<_>>(), [0, 1, 2]);
    assert!(
        e.claim(CellRun { first: 0, count: 3 }).is_empty(),
        "claimed cells are skipped"
    );
    let mut won = won.into_iter();
    // Wrong length: back to absent.
    let err = e.publish(won.next().unwrap(), &[1; 3]).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::CellLength {
                cell: 0,
                got: 3,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(state(&e, 0), Some(CellState::Absent));
    // A failed write: back to absent.
    fs.fail(Op::WriteAt, Some(FileKind::Data), 0, ErrorKind::StorageFull);
    let err = e
        .publish(won.next().unwrap(), &cell_bytes(1, 1))
        .unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::Io {
                op: "pwrite the cell",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(state(&e, 1), Some(CellState::Absent));
    // Abandon.
    e.abandon(won.next().unwrap());
    assert_eq!(state(&e, 2), Some(CellState::Absent));
    // A token whose cell left `claimed` (a defect of the caller) is refused.
    let t = e.claim(CellRun { first: 3, count: 1 }).pop().unwrap();
    let t2 = e.claim(CellRun { first: 2, count: 1 }).pop().unwrap();
    e.abandon(t2);
    let stray = tierstore_fetch_api::entry::ClaimedCell::won(2);
    assert!(matches!(
        e.publish(stray, &cell_bytes(2, 1)),
        Err(EntryError::NotClaimed { cell: 2, .. })
    ));
    e.publish(t, &cell_bytes(3, 1)).unwrap();
    assert_eq!(e.read_cell(3).unwrap(), cell_bytes(3, 1));
    assert_eq!(e.journal().pending(), 1);
}

#[test]
fn evictor_steps_win_only_from_their_state_and_keep_the_durable_bit() {
    let (_fs, e) = fresh();
    publish(&e, &[0], 1).unwrap();
    e.commit_batch().unwrap();
    assert!(!e.evict_cell_cas(0, EvictStep::Abort));
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert_eq!(e.state().load(0), Some(CellByte(0x84)));
    assert!(!e.evict_cell_cas(0, EvictStep::Begin));
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(e.state().load(0), Some(CellByte(0x82)));
    assert!(!e.evict_cell_cas(9, EvictStep::Begin), "outside the pack");
    let t = e.claim(CellRun { first: 1, count: 1 }).pop().unwrap();
    e.mark_bad(t);
    assert_eq!(
        e.counts(),
        CellCounts {
            absent: 2,
            present: 1,
            bad: 1,
            ..CellCounts::default()
        }
    );
    let err = e.read_cell(1).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::NotReadable {
                cell: 1,
                state: CellByte(5),
                ..
            }
        ),
        "{err}"
    );
    assert!(matches!(
        e.read_cell(4),
        Err(EntryError::CellOutOfRange {
            cell: 4,
            cells: 4,
            ..
        })
    ));
}

#[test]
fn a_failed_read_of_a_cell_is_an_io_error() {
    let (fs, e) = fresh();
    publish(&e, &[0], 1).unwrap();
    fs.fail(Op::Read, Some(FileKind::Data), 0, ErrorKind::Other);
    assert!(matches!(
        e.read_cell(0),
        Err(EntryError::Io {
            op: "pread the cell",
            ..
        })
    ));
}

#[test]
fn readers_holds_and_touches_are_counted_per_group() {
    let (_fs, e) = fresh();
    e.enter(0);
    e.enter(0);
    e.enter(7); // outside the pack's one group: ignored
    assert_eq!(e.group_readers(0), 2);
    assert_eq!(e.group_readers(7), 0);
    e.leave(0);
    e.leave(7);
    assert_eq!(e.group_readers(0), 1);
    assert_eq!(e.last_use(), 0);
    e.touch(5_500, 0);
    e.touch(2_000, 0);
    e.touch(9_000, 3);
    assert_eq!(e.last_use(), 9_000);
    assert_eq!(e.touch_epoch(0), Some(5));
    assert_eq!(e.touch_epoch(3), None);
    let (h1, h2) = (HolderId(1), HolderId(2));
    e.hold(h1);
    e.hold(h1);
    e.hold(h2);
    assert_eq!(e.holders(), 2);
    assert!(e.release(h1));
    assert_eq!(e.holders(), 2);
    assert!(e.release(h1));
    assert!(!e.release(h1));
    assert_eq!(e.holders(), 1);
    assert!(e.fds().is_none());
    assert_eq!(e.early_durable(), 0);
}

#[test]
fn the_last_use_is_kept_by_a_close_and_restored() {
    let (fs, e) = fresh();
    e.touch(42_000, 0);
    e.close().unwrap();
    let (after, rec) = open(
        &fs.crash(tierstore_journal::fake::Crash::Process),
        Some(BOOT_A),
    )
    .unwrap();
    assert_eq!(rec.last_use, 42_000);
    assert_eq!(after.last_use(), 42_000);
}

#[test]
fn an_open_that_cannot_set_the_data_file_or_sync_its_directory_fails() {
    let fs = MemFs::new();
    fs.fail(Op::SetLen, Some(FileKind::Data), 0, ErrorKind::StorageFull);
    assert!(matches!(
        open(&fs, Some(BOOT_A)),
        Err(EntryError::Io {
            op: "set the data file's length",
            ..
        })
    ));
    let fs = MemFs::new();
    fs.fail(Op::SyncDir, None, 0, ErrorKind::Other);
    assert!(matches!(
        open(&fs, Some(BOOT_A)),
        Err(EntryError::Io {
            op: "fsync the entry directory",
            ..
        })
    ));
}

#[test]
fn the_order_check_counts_a_durable_bit_set_before_the_bytes_are_synced() {
    // The check the crash tests rely on (`early_durable` stays 0) sees the defect it is for: a
    // `durable` bit set on a cell whose bytes were written but not synced.
    let (_fs, e) = fresh();
    publish(&e, &[2], 1).unwrap();
    assert!(tierstore_journal::CellStates::mark_durable(&e, 2));
    assert_eq!(e.early_durable(), 1);
    // After the sync it counts nothing.
    publish(&e, &[1], 1).unwrap();
    e.commit_batch().unwrap();
    assert_eq!(e.early_durable(), 1);
}
