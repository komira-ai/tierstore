//! The cells of one entry: claim CAS and the rest of the transitions of section 2, the evictor's
//! per-cell protocol and its journaled punch (5.1, 8), the batch commit (8), counts and touch (7),
//! `read_cell` (8), holds (7) and the reader counters.
//!
//! Section 14 rows: "N readers on spans of one pack: one `PackFill`; each cell claimed once" (the
//! claim half); "Hedge and original both deliver a cell: one publishes"; "Crash after a batched
//! PUNCH fsync": one sync per batch, records before holes; "Journal batching" (one `fdatasync` per
//! commit).
//!
//! Catches: a claim that wins a cell that is not absent, wins outside the pack, or wins one cell
//! twice across threads; a publish that writes without a claim, writes bytes of the wrong length or
//! leaves the cell claimed after a failure; bytes that do not reach the data file or the mount's
//! read-only descriptor; an evictor step from the wrong state or one that drops `durable`; a punch
//! of a cell that is not evicting, one that journals more than once per call, punches before the
//! record is written, or leaves blocks allocated; a commit that marks a cell durable after it was
//! evicted, syncs more than once, or repeats work; counts, last use or epochs that are wrong or go
//! back; a read of a cell that is not readable; holds that are not per holder.

use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryStore, EvictStep, PunchReport,
    ReaderCounters, Settle, StateView,
};
use tierstore_fetch_api::ids::HolderId;
use tierstore_fetch_api::page::epoch_of;

use super::{MIB, byte, bytes, open, publish, run, scratch, state};
use crate::journal::{Op, RECORD_LEN, record};

/// The bytes of the journal.
fn journal(e: &crate::FileEntry) -> Vec<u8> {
    std::fs::read(e.journal_path()).unwrap()
}

/// The 512-byte blocks the data file has allocated.
fn blocks(e: &crate::FileEntry) -> u64 {
    std::fs::metadata(e.data_path()).unwrap().blocks()
}

/// `len` bytes of the pack at `offset`, read through the descriptor a mount is passed.
fn mount_read(e: &crate::FileEntry, offset: u64, len: u64) -> Vec<u8> {
    let fd = e.fds().unwrap().data.try_clone_to_owned().unwrap();
    let mut buf = vec![0u8; len as usize];
    std::fs::File::from(fd)
        .read_exact_at(&mut buf, offset)
        .unwrap();
    buf
}

#[test]
fn a_new_entry_is_a_sparse_file_of_the_pack_size_with_every_cell_absent_and_an_empty_journal() {
    let dir = scratch("entry-new");
    let (_store, e) = open(&dir, "p", 5 * MIB + 10);
    assert_eq!(e.size(), 5 * MIB + 10);
    assert_eq!(e.cell_count(), 6);
    assert_eq!(e.group_count(), 1);
    assert_eq!(
        std::fs::metadata(e.data_path()).unwrap().len(),
        5 * MIB + 10
    );
    assert_eq!(blocks(&e), 0);
    assert!(journal(&e).is_empty());
    assert!((0..6).all(|c| state(&e, c) == CellState::Absent));
    assert_eq!(e.load(6), None);
    assert_eq!(e.last_use(), 0);
    assert_eq!(e.holders(), 0);
}

#[test]
fn claim_wins_absent_cells_in_ascending_order_and_skips_every_other_state() {
    let dir = scratch("entry-claim");
    let (_store, e) = open(&dir, "p", 8 * MIB);
    publish(&e, 1, 1);
    let held = e.claim(run(3, 1));
    let won: Vec<u32> = e.claim(run(0, 10)).iter().map(ClaimedCell::cell).collect();
    // 1 is present, 3 claimed, 8 and 9 outside the pack.
    assert_eq!(won, vec![0, 2, 4, 5, 6, 7]);
    assert_eq!(state(&e, 0), CellState::Claimed);
    assert!(e.claim(run(0, 8)).is_empty());
    assert_eq!(held, vec![ClaimedCell::won(3)]);
}

#[test]
fn threads_claiming_one_run_win_each_cell_exactly_once() {
    let dir = scratch("entry-claim-threads");
    let (_store, e) = open(&dir, "p", 64 * MIB);
    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let won = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (e, barrier, won) = (e.clone(), barrier.clone(), won.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let cells = e.claim(run(0, 64));
                won.fetch_add(cells.len(), Ordering::SeqCst);
                cells
            })
        })
        .collect();
    let mut all: Vec<u32> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .map(|c| c.cell())
        .collect();
    all.sort_unstable();
    assert_eq!(all, (0..64).collect::<Vec<u32>>());
    assert_eq!(won.load(Ordering::SeqCst), 64);
}

#[test]
fn abandon_returns_a_claimed_cell_to_absent_and_changes_nothing_else() {
    let dir = scratch("entry-abandon");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let mut c = e.claim(run(0, 1));
    e.abandon(c.remove(0));
    assert_eq!(state(&e, 0), CellState::Absent);
    assert_eq!(e.claim(run(0, 1)).len(), 1);
    publish(&e, 1, 9);
    // A forged token of a present cell abandons nothing.
    e.abandon(ClaimedCell::won(1));
    assert_eq!(state(&e, 1), CellState::Present);
}

#[test]
fn publish_writes_the_bytes_then_makes_the_cell_servable_through_the_mounts_descriptor() {
    let dir = scratch("entry-publish");
    let (_store, e) = open(&dir, "p", 3 * MIB);
    publish(&e, 1, 0xab);
    assert_eq!(state(&e, 1), CellState::Present);
    assert!(e.state().ready(run(1, 1)));
    assert!(!e.state().ready(run(0, 2)));
    assert_eq!(mount_read(&e, MIB, MIB), bytes(0xab, MIB));
    assert_eq!(mount_read(&e, 0, 16), vec![0; 16]);
    assert_eq!(e.read_cell(1).unwrap(), bytes(0xab, MIB));
    assert!(blocks(&e) > 0);
}

#[test]
fn publish_of_the_wrong_length_fails_and_returns_the_cell_to_absent_with_nothing_written() {
    let dir = scratch("entry-publish-len");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let mut c = e.claim(run(0, 1));
    let err = e.publish(c.remove(0), &bytes(1, MIB - 1)).unwrap_err();
    assert!(
        matches!(err, EntryError::CellLength { cell: 0, expected, got, .. } if expected == MIB && got == MIB - 1),
        "{err}"
    );
    assert_eq!(state(&e, 0), CellState::Absent);
    assert_eq!(blocks(&e), 0);
}

#[test]
fn publish_of_the_short_last_cell_takes_exactly_its_bytes() {
    let dir = scratch("entry-publish-last");
    let (_store, e) = open(&dir, "p", MIB + 100);
    let mut c = e.claim(run(1, 1));
    e.publish(c.remove(0), &bytes(5, 100)).unwrap();
    assert_eq!(e.read_cell(1).unwrap(), bytes(5, 100));
    assert_eq!(mount_read(&e, MIB, 100), bytes(5, 100));
}

#[test]
fn publish_without_a_claim_is_not_claimed_and_leaves_the_state() {
    let dir = scratch("entry-publish-unclaimed");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let err = e.publish(ClaimedCell::won(0), &bytes(1, MIB)).unwrap_err();
    assert!(
        matches!(err, EntryError::NotClaimed { cell: 0, .. }),
        "{err}"
    );
    assert_eq!(state(&e, 0), CellState::Absent);
    // A hedge and its original share one token, so a cell is published once: a second publish
    // of a present cell is refused and keeps the first bytes.
    publish(&e, 1, 1);
    assert!(e.publish(ClaimedCell::won(1), &bytes(2, MIB)).is_err());
    assert_eq!(e.read_cell(1).unwrap(), bytes(1, MIB));
    // Outside the pack.
    let err = e.publish(ClaimedCell::won(5), &bytes(1, MIB)).unwrap_err();
    assert!(
        matches!(err, EntryError::NotClaimed { cell: 5, .. }),
        "{err}"
    );
}

#[test]
fn mark_bad_makes_a_claimed_cell_bad_and_it_expires_to_absent() {
    let dir = scratch("entry-bad");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let mut c = e.claim(run(0, 1));
    e.mark_bad(c.remove(0));
    assert_eq!(state(&e, 0), CellState::Bad);
    assert!(!e.state().ready(run(0, 1)));
    assert!(e.claim(run(0, 1)).is_empty());
    assert!(!e.settle(0, Settle::RecheckOk));
    assert!(e.settle(0, Settle::ExpireBad));
    assert_eq!(state(&e, 0), CellState::Absent);
    assert!(!e.settle(0, Settle::ExpireBad));
    assert!(!e.settle(9, Settle::ExpireBad));
}

#[test]
fn a_recheck_cell_is_read_for_its_rehash_and_settles_either_way() {
    let dir = scratch("entry-recheck");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    publish(&e, 0, 3);
    publish(&e, 1, 4);
    e.force(0, byte(CellState::Recheck));
    e.force(1, byte(CellState::Recheck));
    assert!(!e.state().ready(run(0, 1)));
    assert_eq!(e.read_cell(0).unwrap(), bytes(3, MIB));
    assert!(e.settle(0, Settle::RecheckOk));
    assert_eq!(state(&e, 0), CellState::Present);
    assert!(e.settle(1, Settle::RecheckFailed));
    assert_eq!(state(&e, 1), CellState::Absent);
    // The read raised and lowered the in-process counter of the group.
    assert_eq!(e.group_readers(0), 0);
}

#[test]
fn read_cell_of_any_other_state_or_outside_the_pack_is_refused() {
    let dir = scratch("entry-read-refused");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let err = e.read_cell(0).unwrap_err();
    assert!(
        matches!(err, EntryError::NotReadable { cell: 0, state, .. } if state == byte(CellState::Absent)),
        "{err}"
    );
    let err = e.read_cell(2).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::CellOutOfRange {
                cell: 2,
                cells: 2,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(e.group_readers(0), 0);
}

#[test]
fn eviction_begins_only_from_present_and_an_abort_keeps_durable_and_the_bytes() {
    let dir = scratch("entry-evict");
    let (_store, e) = open(&dir, "p", 3 * MIB);
    assert!(!e.evict_cell_cas(0, EvictStep::Begin));
    let _claimed = e.claim(run(0, 1));
    assert!(!e.evict_cell_cas(0, EvictStep::Begin));
    assert!(!e.evict_cell_cas(7, EvictStep::Begin));
    publish(&e, 1, 6);
    e.commit_batch().unwrap();
    assert_eq!(e.load(1), Some(CellByte(0x82)));
    assert!(e.evict_cell_cas(1, EvictStep::Begin));
    assert_eq!(e.load(1), Some(CellByte(0x84)));
    assert!(!e.state().ready(run(1, 1)));
    assert!(!e.evict_cell_cas(1, EvictStep::Begin));
    assert!(e.evict_cell_cas(1, EvictStep::Abort));
    assert_eq!(e.load(1), Some(CellByte(0x82)));
    assert_eq!(e.read_cell(1).unwrap(), bytes(6, MIB));
    assert!(!e.evict_cell_cas(1, EvictStep::Finish));
}

#[test]
fn a_batch_punch_journals_every_run_with_one_sync_then_releases_the_blocks() {
    let dir = scratch("entry-punch");
    let (_store, e) = open(&dir, "p", 6 * MIB + 100);
    for c in [0, 1, 3, 6] {
        publish_len(&e, c);
    }
    publish(&e, 4, 8);
    for c in [0, 1, 3, 6] {
        assert!(e.evict_cell_cas(c, EvictStep::Begin));
    }
    let before = blocks(&e);
    let report = e
        .punch(&[run(0, 2), run(2, 0), run(3, 1), run(6, 1)])
        .unwrap();
    assert_eq!(
        report,
        PunchReport {
            journal_syncs: 1,
            records: 3,
            bytes_punched: 3 * MIB + 100,
        }
    );
    let mut expected = Vec::new();
    for r in [run(0, 2), run(3, 1), run(6, 1)] {
        expected.extend_from_slice(&record(Op::Punch, r));
    }
    assert_eq!(journal(&e), expected);
    // The punch released the blocks of the four cells, kept the size and left cell 4.
    assert!(blocks(&e) < before);
    assert_eq!(
        std::fs::metadata(e.data_path()).unwrap().len(),
        6 * MIB + 100
    );
    assert_eq!(mount_read(&e, 0, 2 * MIB), bytes(0, 2 * MIB));
    assert_eq!(mount_read(&e, 3 * MIB, MIB), bytes(0, MIB));
    assert_eq!(mount_read(&e, 6 * MIB, 100), bytes(0, 100));
    assert_eq!(e.read_cell(4).unwrap(), bytes(8, MIB));
    // The punch did not change a state: the evictor finishes each cell.
    assert_eq!(state(&e, 0), CellState::Evicting);
    for c in [0, 1, 3, 6] {
        assert!(e.evict_cell_cas(c, EvictStep::Finish));
        assert_eq!(state(&e, c), CellState::Absent);
    }
    // A later publish of a punched cell survives (the hole was made while it was evicting).
    publish(&e, 0, 0x11);
    assert_eq!(mount_read(&e, 0, MIB), bytes(0x11, MIB));
}

/// Claims and publishes `cell` with bytes of its own length (the last cell may be short).
fn publish_len(e: &crate::FileEntry, cell: u32) {
    let mut c = e.claim(run(cell, 1));
    let len = (e.size() - u64::from(cell) * MIB).min(MIB);
    e.publish(c.remove(0), &bytes(0x5a, len)).unwrap();
}

#[test]
fn a_punch_with_no_cell_is_zeros_and_journals_nothing() {
    let dir = scratch("entry-punch-none");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    assert_eq!(e.punch(&[]).unwrap(), PunchReport::default());
    assert_eq!(e.punch(&[run(1, 0)]).unwrap(), PunchReport::default());
    assert!(journal(&e).is_empty());
}

#[test]
fn a_punch_of_a_cell_that_is_not_evicting_or_outside_the_pack_has_no_effect() {
    let dir = scratch("entry-punch-refused");
    let (_store, e) = open(&dir, "p", 3 * MIB);
    publish(&e, 0, 1);
    publish(&e, 1, 2);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    let err = e.punch(&[run(0, 2)]).unwrap_err();
    assert!(
        matches!(err, EntryError::NotEvicting { cell: 1, .. }),
        "{err}"
    );
    let err = e.punch(&[run(0, 1), run(3, 1)]).unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::CellOutOfRange {
                cell: 3,
                cells: 3,
                ..
            }
        ),
        "{err}"
    );
    assert!(journal(&e).is_empty());
    assert_eq!(mount_read(&e, 0, MIB), bytes(1, MIB));
    assert_eq!(state(&e, 0), CellState::Evicting);
}

#[test]
fn a_commit_syncs_once_marks_the_published_cells_durable_and_journals_one_add_per_run() {
    let dir = scratch("entry-commit");
    let (_store, e) = open(&dir, "p", 8 * MIB);
    assert_eq!(e.commit_batch().unwrap(), CommitReport::default());
    for c in [0, 4, 5, 6] {
        publish(&e, c, 1);
    }
    let report = e.commit_batch().unwrap();
    assert_eq!(
        report,
        CommitReport {
            data_syncs: 1,
            cells_durable: 4,
            records: 2,
        }
    );
    assert!([0, 4, 5, 6].iter().all(|c| e.load(*c).unwrap().durable()));
    assert!(!e.load(1).unwrap().durable());
    let mut expected = record(Op::Add, run(0, 1)).to_vec();
    expected.extend_from_slice(&record(Op::Add, run(4, 3)));
    assert_eq!(journal(&e), expected);
    // Nothing new: nothing to sync.
    assert_eq!(e.commit_batch().unwrap(), CommitReport::default());
    assert_eq!(journal(&e).len(), 2 * RECORD_LEN);
}

#[test]
fn a_cell_evicted_before_the_commit_is_not_marked_durable_or_journaled() {
    let dir = scratch("entry-commit-evicted");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    publish(&e, 0, 1);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert_eq!(
        e.commit_batch().unwrap(),
        CommitReport {
            data_syncs: 1,
            cells_durable: 0,
            records: 0,
        }
    );
    assert_eq!(e.load(0), Some(byte(CellState::Evicting)));
    assert!(journal(&e).is_empty());
}

#[test]
fn a_cell_evicting_at_a_commit_and_then_aborted_is_made_durable_by_the_next_commit() {
    let dir = scratch("entry-commit-abort");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    publish(&e, 0, 1);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert_eq!(e.commit_batch().unwrap().cells_durable, 0);
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(
        e.commit_batch().unwrap(),
        CommitReport {
            data_syncs: 1,
            cells_durable: 1,
            records: 1,
        }
    );
    assert_eq!(e.load(0), Some(CellByte(0x82)));
    assert_eq!(journal(&e), record(Op::Add, run(0, 1)).to_vec());
}

#[test]
fn a_cell_evicted_to_absent_is_dropped_from_the_commits() {
    let dir = scratch("entry-commit-absent");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    publish(&e, 0, 1);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert_eq!(e.commit_batch().unwrap().cells_durable, 0);
    e.punch(&[run(0, 1)]).unwrap();
    assert!(e.evict_cell_cas(0, EvictStep::Finish));
    // Still pending at this commit (it was evicting at the last), then gone for good.
    assert_eq!(e.commit_batch().unwrap().data_syncs, 1);
    assert_eq!(e.commit_batch().unwrap(), CommitReport::default());
    // Published again, it is committed as a new cell.
    publish(&e, 0, 2);
    assert_eq!(e.commit_batch().unwrap().cells_durable, 1);
}

#[test]
fn the_data_descriptor_a_mount_is_passed_is_read_only() {
    let dir = scratch("entry-data-ro");
    let (_store, e) = open(&dir, "p", 2 * MIB);
    let fd = e.fds().unwrap().data.try_clone_to_owned().unwrap();
    let file = std::fs::File::from(fd);
    let err = file.write_all_at(&[1], 0).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    let file = Arc::new(file);
    let err = crate::map::map_shared(&file, 4096, libc::PROT_READ | libc::PROT_WRITE).unwrap_err();
    assert_eq!(
        err.to_string(),
        std::io::Error::from_raw_os_error(libc::EACCES).to_string()
    );
    assert_eq!(blocks(&e), 0);
}

#[test]
fn journal_records_have_their_golden_bytes() {
    assert_eq!(
        record(Op::Punch, run(3, 2)),
        [2, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0, 213, 185, 160, 45]
    );
    assert_eq!(
        record(Op::Add, run(0, 1)),
        [1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 213, 203, 84, 199]
    );
}

#[test]
fn counts_say_how_many_cells_are_in_each_state() {
    let dir = scratch("entry-counts");
    let (_store, e) = open(&dir, "p", 10 * MIB);
    publish(&e, 0, 1);
    publish(&e, 1, 1);
    publish(&e, 2, 1);
    assert!(e.evict_cell_cas(2, EvictStep::Begin));
    let _claimed = e.claim(run(3, 2));
    let mut bad = e.claim(run(5, 1));
    e.mark_bad(bad.remove(0));
    e.force(6, byte(CellState::Recheck));
    let counts = e.counts();
    assert_eq!(
        counts,
        CellCounts {
            absent: 3,
            claimed: 2,
            present: 2,
            recheck: 1,
            evicting: 1,
            bad: 1,
        }
    );
    assert_eq!(counts.used(), 5);
}

#[test]
fn a_touch_raises_the_last_use_and_sets_the_epoch_of_its_group_and_never_goes_back() {
    let dir = scratch("entry-touch");
    let (_store, e) = open(&dir, "p", 40 * MIB);
    assert_eq!(e.touch_epoch(1), Some(0));
    e.touch(7_300, 1);
    assert_eq!(e.last_use(), 7_300);
    assert_eq!(e.touch_epoch(1), Some(7));
    assert_eq!(e.touch_epoch(0), Some(0));
    // An older clock lowers neither the last use nor the epoch it writes.
    e.touch(2_000, 0);
    assert_eq!(e.last_use(), 7_300);
    assert_eq!(e.touch_epoch(0), Some(7));
    // A group outside the pack raises the last use only.
    e.touch(9_000, 3);
    assert_eq!(e.last_use(), 9_000);
    assert_eq!(e.touch_epoch(3), None);
    // The epoch is the seconds, truncated to a u32.
    let far = (u64::from(u32::MAX) + 5) * 1000;
    e.touch(far, 2);
    assert_eq!(e.touch_epoch(2), Some(epoch_of(far)));
    assert_eq!(e.touch_epoch(2), Some(4));
}

#[test]
fn holds_are_refcounts_per_holder() {
    let dir = scratch("entry-holds");
    let (_store, e) = open(&dir, "p", MIB);
    e.hold(HolderId(1));
    e.hold(HolderId(1));
    e.hold(HolderId(2));
    assert_eq!(e.holders(), 2);
    assert!(e.release(HolderId(1)));
    assert_eq!(e.holders(), 2);
    assert!(e.release(HolderId(1)));
    assert_eq!(e.holders(), 1);
    assert!(!e.release(HolderId(1)));
    assert!(!e.release(HolderId(3)));
    assert!(e.release(HolderId(2)));
    assert_eq!(e.holders(), 0);
}

#[test]
fn group_readers_sums_in_process_readers_and_every_attached_holder() {
    let dir = scratch("entry-readers-sum");
    let (store, e) = open(&dir, "p", 20 * MIB);
    let mount = super::MountCounters::new(4096);
    let counters = Arc::new(crate::MemfdCounters::new(mount.fd()).unwrap());
    store.register_holder(HolderId(1), counters).unwrap();
    store
        .attach(
            &super::pack("p"),
            HolderId(1),
            tierstore_fetch_api::page::CounterRegion {
                offset: 0,
                groups: 2,
            },
        )
        .unwrap();
    e.enter(1);
    mount.enter(2);
    mount.enter(2);
    assert_eq!(e.group_readers(1), 3);
    e.leave(1);
    assert_eq!(e.group_readers(1), 2);
    mount.leave(2);
    assert_eq!(e.group_readers(1), 1);
    assert_eq!(e.group_readers(0), 0);
}

#[test]
fn in_process_reader_counters_are_summed_and_ignore_groups_outside_the_pack() {
    let dir = scratch("entry-readers");
    let (_store, e) = open(&dir, "p", 20 * MIB);
    e.enter(1);
    e.enter(1);
    e.enter(5);
    assert_eq!(e.group_readers(1), 2);
    assert_eq!(e.group_readers(5), 0);
    e.leave(1);
    e.leave(5);
    assert_eq!(e.group_readers(1), 1);
    assert_eq!(e.group_readers(0), 0);
}
