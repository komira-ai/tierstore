//! `MemEntry` and `MemEntryStore` follow the CAS rules of section 2, the evictor ordering of 5.1,
//! the commit of section 8 and the store contract of section 7.
//!
//! Catches:
//! - a claim that wins a cell that is not absent, wins one outside the pack, or wins one twice
//!   (single-threaded or across threads);
//! - a publish that makes a cell present with bytes of the wrong length, or without a claim;
//! - an abandon or a bad mark that leaves the wrong state;
//! - an evictor step that wins from the wrong state, loses the durable bit on an abort, or leaves
//!   the bytes of a finished eviction;
//! - a commit that syncs more than once, counts runs wrongly, or repeats work;
//! - reader counters that count an out-of-range group, or holds that are not per holder;
//! - a store that opens a second entry for a pack, accepts another size, allocates for a pack
//!   past `u32::MAX` cells, or removes a held entry.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{
    ClaimedCell, CommitReport, Entry, EntryError, EntrySpec, EntryStore, EvictStep, ReaderCounters,
    Settle,
};
use tierstore_fetch_api::fake::{MemEntry, MemEntryStore};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot};

const MIB: usize = 1 << 20;

fn pack(s: &str) -> PackId {
    PackId {
        key: PackKey(s.to_owned()),
        root: PackRoot::Digest([2; 32]),
    }
}

fn spec(s: &str, size: u64) -> EntrySpec {
    EntrySpec {
        pack: pack(s),
        size,
    }
}

fn entry(cells: usize) -> MemEntry {
    MemEntry::new(&spec("p", (cells * MIB) as u64)).unwrap()
}

fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

fn state(e: &MemEntry, cell: u32) -> CellState {
    e.state().load(cell).unwrap().state().unwrap()
}

fn cell_bytes(fill: u8) -> Vec<u8> {
    vec![fill; MIB]
}

fn claim_publish(e: &MemEntry, cell: u32, fill: u8) {
    let won = e.claim(run(cell, 1));
    assert_eq!(won.len(), 1);
    e.publish(won.into_iter().next().unwrap(), &cell_bytes(fill))
        .unwrap();
}

#[test]
fn a_new_entry_has_every_cell_absent_and_zero_bytes() {
    let e = MemEntry::new(&spec("p", 3 * MIB as u64 + 1)).unwrap();
    assert_eq!(e.pack(), &pack("p"));
    assert_eq!(e.size(), 3 * MIB as u64 + 1);
    assert_eq!(e.state().cell_count(), 4);
    for c in 0..4 {
        assert_eq!(
            e.state().load(c),
            Some(CellByte::new(CellState::Absent, false))
        );
    }
    assert_eq!(e.state().load(4), None);
    assert_eq!(e.bytes(0, 4), vec![0; 4]);
}

#[test]
fn an_empty_pack_has_no_cells() {
    let e = MemEntry::new(&spec("p", 0)).unwrap();
    assert_eq!(e.state().cell_count(), 0);
    assert!(e.claim(run(0, 1)).is_empty());
    assert!(e.state().ready(run(0, 0)));
}

#[test]
fn a_pack_of_more_than_u32_max_cells_is_refused_before_any_allocation() {
    // 2^32 cells of 1 MiB: nothing could allocate it, so a refusal that comes back proves that
    // `new` decides before it allocates.
    let size = (u64::from(u32::MAX) + 1) << 20;
    assert!(MemEntry::new(&spec("big", size)).is_none());
}

#[test]
fn claim_wins_absent_cells_in_ascending_order() {
    let e = entry(4);
    let won = e.claim(run(1, 3));
    assert_eq!(
        won,
        vec![
            ClaimedCell::won(1),
            ClaimedCell::won(2),
            ClaimedCell::won(3)
        ]
    );
    assert_eq!(state(&e, 0), CellState::Absent);
    for c in 1..4 {
        assert_eq!(state(&e, c), CellState::Claimed);
    }
}

#[test]
fn claim_skips_claimed_present_and_out_of_range_cells() {
    let e = entry(4);
    claim_publish(&e, 1, 7);
    let held = e.claim(run(2, 1));
    assert_eq!(held.len(), 1);
    let won = e.claim(run(0, 8));
    assert_eq!(won, vec![ClaimedCell::won(0), ClaimedCell::won(3)]);
    assert!(e.claim(run(10, 3)).is_empty());
    assert!(e.claim(run(0, 0)).is_empty());
    assert_eq!(state(&e, 1), CellState::Present);
    assert_eq!(state(&e, 2), CellState::Claimed);
}

#[test]
fn a_second_claim_of_the_same_run_gets_nothing() {
    let e = entry(3);
    assert_eq!(e.claim(run(0, 3)).len(), 3);
    assert!(e.claim(run(0, 3)).is_empty());
}

#[test]
fn abandon_returns_a_claimed_cell_to_absent_and_it_can_be_claimed_again() {
    let e = entry(2);
    let mut won = e.claim(run(0, 1));
    e.abandon(won.remove(0));
    assert_eq!(state(&e, 0), CellState::Absent);
    assert_eq!(e.claim(run(0, 1)).len(), 1);
}

#[test]
fn abandon_of_a_cell_that_is_not_claimed_changes_nothing() {
    let e = entry(2);
    claim_publish(&e, 0, 3);
    e.abandon(ClaimedCell::won(0));
    assert_eq!(state(&e, 0), CellState::Present);
    e.abandon(ClaimedCell::won(9));
}

#[test]
fn publish_writes_the_bytes_and_makes_the_cell_servable() {
    let e = entry(3);
    claim_publish(&e, 1, 0xab);
    assert_eq!(state(&e, 1), CellState::Present);
    assert!(e.state().ready(run(1, 1)));
    assert!(!e.state().ready(run(0, 2)));
    assert!(!e.state().ready(run(2, 5)));
    assert_eq!(e.bytes(MIB, 2 * MIB), cell_bytes(0xab));
    assert_eq!(e.bytes(0, MIB), cell_bytes(0));
    assert_eq!(e.bytes(2 * MIB, 3 * MIB), cell_bytes(0));
}

#[test]
fn publish_of_the_wrong_length_fails_and_returns_the_cell_to_absent() {
    let e = entry(2);
    let token = e.claim(run(0, 1)).remove(0);
    let short = e.publish(token, &[1; 10]).unwrap_err();
    match short {
        EntryError::CellLength {
            pack: p,
            cell,
            expected,
            got,
        } => {
            assert_eq!(p, pack("p"));
            assert_eq!(cell, 0);
            assert_eq!(expected, MIB as u64);
            assert_eq!(got, 10);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(state(&e, 0), CellState::Absent);
    assert_eq!(e.bytes(0, 4), vec![0; 4]);
    // The cell can be claimed again, and a too-long publish also frees it.
    let token = e.claim(run(0, 1)).remove(0);
    let long = e.publish(token, &vec![1; MIB + 1]).unwrap_err();
    assert!(matches!(long, EntryError::CellLength { got, .. } if got == MIB as u64 + 1));
    assert_eq!(state(&e, 0), CellState::Absent);
    claim_publish(&e, 0, 5);
    assert_eq!(state(&e, 0), CellState::Present);
}

#[test]
fn publish_of_the_last_short_cell_takes_exactly_its_bytes() {
    let e = MemEntry::new(&spec("p", MIB as u64 + 100)).unwrap();
    assert_eq!(e.claim(run(0, 2)).len(), 2);
    e.publish(ClaimedCell::won(1), &[9; 100]).unwrap();
    assert_eq!(state(&e, 1), CellState::Present);
    assert_eq!(e.bytes(MIB, MIB + 100), vec![9; 100]);
    // A fresh claim of the short cell names its own length in a refusal.
    let f = MemEntry::new(&spec("p", MIB as u64 + 100)).unwrap();
    assert_eq!(f.claim(run(1, 1)).len(), 1);
    let err = f.publish(ClaimedCell::won(1), &cell_bytes(1)).unwrap_err();
    assert!(matches!(err, EntryError::CellLength { expected: 100, .. }));
}

#[test]
fn publish_without_a_claim_is_not_claimed() {
    let e = entry(2);
    // Absent cell, a token made by hand.
    let err = e.publish(ClaimedCell::won(0), &cell_bytes(1)).unwrap_err();
    assert!(matches!(err, EntryError::NotClaimed { cell: 0, .. }));
    // A cell outside the pack.
    let err = e.publish(ClaimedCell::won(5), &cell_bytes(1)).unwrap_err();
    assert!(matches!(err, EntryError::NotClaimed { cell: 5, .. }));
    // A cell that was abandoned after its token was copied.
    let mut won = e.claim(run(1, 1));
    e.abandon(ClaimedCell::won(1));
    let err = e.publish(won.remove(0), &cell_bytes(1)).unwrap_err();
    assert!(matches!(err, EntryError::NotClaimed { cell: 1, .. }));
    assert_eq!(state(&e, 1), CellState::Absent);
    assert_eq!(e.bytes(MIB, MIB + 4), vec![0; 4]);
}

#[test]
fn mark_bad_moves_a_claimed_cell_to_bad_and_it_is_not_servable() {
    let e = entry(2);
    let mut won = e.claim(run(0, 1));
    e.mark_bad(won.remove(0));
    assert_eq!(state(&e, 0), CellState::Bad);
    assert!(!e.state().ready(run(0, 1)));
    // Not a claimed cell: nothing changes.
    e.mark_bad(ClaimedCell::won(1));
    assert_eq!(state(&e, 1), CellState::Absent);
}

#[test]
fn an_expired_bad_cell_settles_to_absent() {
    let e = entry(1);
    let mut won = e.claim(run(0, 1));
    e.mark_bad(won.remove(0));
    assert!(!e.settle(0, Settle::RecheckOk));
    assert!(!e.settle(0, Settle::RecheckFailed));
    assert!(e.settle(0, Settle::ExpireBad));
    assert_eq!(state(&e, 0), CellState::Absent);
    assert!(!e.settle(0, Settle::ExpireBad));
}

#[test]
fn settle_of_a_cell_that_is_not_in_recheck_or_outside_the_pack_does_not_win() {
    let e = entry(2);
    claim_publish(&e, 0, 1);
    assert!(!e.settle(0, Settle::RecheckOk));
    assert!(!e.settle(0, Settle::RecheckFailed));
    assert!(!e.settle(1, Settle::RecheckOk));
    assert!(!e.settle(7, Settle::ExpireBad));
    assert_eq!(state(&e, 0), CellState::Present);
}

#[test]
fn eviction_begins_only_from_present() {
    let e = entry(3);
    claim_publish(&e, 0, 1);
    let _held = e.claim(run(1, 1));
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert_eq!(state(&e, 0), CellState::Evicting);
    assert!(!e.state().ready(run(0, 1)));
    assert!(!e.evict_cell_cas(0, EvictStep::Begin));
    assert!(!e.evict_cell_cas(1, EvictStep::Begin));
    assert!(!e.evict_cell_cas(2, EvictStep::Begin));
    assert!(!e.evict_cell_cas(9, EvictStep::Begin));
}

#[test]
fn abort_returns_to_present_and_keeps_the_durable_bit_and_the_bytes() {
    let e = entry(2);
    claim_publish(&e, 0, 4);
    e.commit_batch().unwrap();
    assert!(e.state().load(0).unwrap().durable());
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(
        e.state().load(0),
        Some(CellByte::new(CellState::Present, true))
    );
    assert_eq!(e.bytes(0, MIB), cell_bytes(4));
    // Not evicting any more: neither abort nor finish wins.
    assert!(!e.evict_cell_cas(0, EvictStep::Abort));
    assert!(!e.evict_cell_cas(0, EvictStep::Finish));
}

#[test]
fn finish_makes_the_cell_absent_and_zeroes_its_bytes_only() {
    let e = MemEntry::new(&spec("p", 2 * MIB as u64 + 10)).unwrap();
    claim_publish(&e, 0, 1);
    claim_publish(&e, 1, 2);
    assert_eq!(e.claim(run(2, 1)).len(), 1);
    e.publish(ClaimedCell::won(2), &[3; 10]).unwrap();
    assert!(e.evict_cell_cas(2, EvictStep::Begin));
    assert!(e.evict_cell_cas(2, EvictStep::Finish));
    assert_eq!(state(&e, 2), CellState::Absent);
    assert_eq!(e.bytes(2 * MIB, 2 * MIB + 10), vec![0; 10]);
    assert_eq!(e.bytes(0, MIB), cell_bytes(1));
    assert_eq!(e.bytes(MIB, 2 * MIB), cell_bytes(2));
    // An evicted cell can be fetched again.
    assert_eq!(e.claim(run(2, 1)).len(), 1);
}

#[test]
fn finish_of_a_full_cell_zeroes_the_whole_cell() {
    let e = entry(2);
    claim_publish(&e, 0, 9);
    claim_publish(&e, 1, 8);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert!(e.evict_cell_cas(0, EvictStep::Finish));
    assert_eq!(e.bytes(0, MIB), cell_bytes(0));
    assert_eq!(e.bytes(MIB, 2 * MIB), cell_bytes(8));
}

#[test]
fn group_readers_count_enter_minus_leave_per_group() {
    let e = entry(40);
    assert_eq!(e.group_readers(0), 0);
    e.enter(0);
    e.enter(0);
    e.enter(2);
    assert_eq!(e.group_readers(0), 2);
    assert_eq!(e.group_readers(1), 0);
    assert_eq!(e.group_readers(2), 1);
    e.leave(0);
    assert_eq!(e.group_readers(0), 1);
    e.leave(0);
    e.leave(2);
    assert_eq!(e.group_readers(0), 0);
    assert_eq!(e.group_readers(2), 0);
}

#[test]
fn a_group_outside_the_pack_is_ignored_by_the_counters() {
    let e = entry(40);
    // 40 cells are 3 groups.
    e.enter(3);
    e.leave(3);
    e.enter(u32::MAX);
    assert_eq!(e.group_readers(3), 0);
    assert_eq!(e.group_readers(u32::MAX), 0);
    assert_eq!(e.group_readers(2), 0);
}

#[test]
fn holds_are_per_holder_and_one_release_drops_a_holder() {
    let e = entry(1);
    assert_eq!(e.holders(), 0);
    e.hold(HolderId(1));
    e.hold(HolderId(1));
    e.hold(HolderId(2));
    assert_eq!(e.holders(), 2);
    // Holder 1 holds twice: the first release keeps it a holder, the second removes it.
    assert!(e.release(HolderId(1)));
    assert_eq!(e.holders(), 2);
    assert!(e.release(HolderId(1)));
    assert_eq!(e.holders(), 1);
    assert!(!e.release(HolderId(1)));
    assert!(!e.release(HolderId(99)));
    assert!(e.release(HolderId(2)));
    assert_eq!(e.holders(), 0);
}

#[test]
fn commit_batch_syncs_once_and_writes_one_record_per_contiguous_run() {
    let e = entry(6);
    for c in [0, 1, 2, 5] {
        claim_publish(&e, c, 1);
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
    for c in [0, 1, 2, 5] {
        assert_eq!(
            e.state().load(c),
            Some(CellByte::new(CellState::Present, true))
        );
    }
    assert_eq!(
        e.state().load(3),
        Some(CellByte::new(CellState::Absent, false))
    );
}

#[test]
fn commit_batch_of_one_cell_is_one_run() {
    let e = entry(2);
    claim_publish(&e, 1, 1);
    assert_eq!(
        e.commit_batch().unwrap(),
        CommitReport {
            data_syncs: 1,
            cells_durable: 1,
            records: 1,
        }
    );
}

#[test]
fn a_commit_with_nothing_new_does_nothing() {
    let e = entry(3);
    assert_eq!(e.commit_batch().unwrap(), CommitReport::default());
    claim_publish(&e, 0, 1);
    e.commit_batch().unwrap();
    assert_eq!(e.commit_batch().unwrap(), CommitReport::default());
    claim_publish(&e, 1, 1);
    // Only the new cell is in the next commit.
    assert_eq!(
        e.commit_batch().unwrap(),
        CommitReport {
            data_syncs: 1,
            cells_durable: 1,
            records: 1,
        }
    );
}

#[test]
fn a_cell_evicted_before_the_commit_is_not_counted_durable() {
    let e = entry(2);
    claim_publish(&e, 0, 1);
    claim_publish(&e, 1, 1);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    let report = e.commit_batch().unwrap();
    assert_eq!(report.cells_durable, 1);
    assert_eq!(report.records, 1);
    assert_eq!(state(&e, 0), CellState::Evicting);
}

#[test]
fn bytes_outside_the_pack_or_reversed_are_empty() {
    let e = entry(1);
    assert!(e.bytes(0, MIB + 1).is_empty());
    assert!(e.bytes(MIB + 1, MIB + 2).is_empty());
    assert!(e.bytes(5, 3).is_empty());
    assert_eq!(e.bytes(3, 3), Vec::<u8>::new());
    assert_eq!(e.bytes(0, MIB).len(), MIB);
}

#[test]
fn threads_claiming_the_same_run_win_each_cell_exactly_once() {
    const THREADS: usize = 8;
    const CELLS: u32 = 64;
    let e = Arc::new(entry(CELLS as usize));
    let barrier = Arc::new(Barrier::new(THREADS));
    let total = Arc::new(AtomicUsize::new(0));
    let per_cell: Arc<Vec<AtomicUsize>> =
        Arc::new((0..CELLS).map(|_| AtomicUsize::new(0)).collect());
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (e, barrier, total, per_cell) = (
                Arc::clone(&e),
                Arc::clone(&barrier),
                Arc::clone(&total),
                Arc::clone(&per_cell),
            );
            std::thread::spawn(move || {
                barrier.wait();
                for won in e.claim(run(0, CELLS)) {
                    total.fetch_add(1, Ordering::SeqCst);
                    per_cell[won.cell() as usize].fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(total.load(Ordering::SeqCst), CELLS as usize);
    for c in per_cell.iter() {
        assert_eq!(c.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn the_store_opens_an_entry_and_reopening_the_same_size_returns_it() {
    let s = MemEntryStore::new();
    assert!(s.packs().is_empty());
    assert!(s.get(&pack("p")).is_none());
    assert!(s.mem_entry(&pack("p")).is_none());
    let a = s.open(&spec("p", 2 * MIB as u64)).unwrap();
    let b = s.open(&spec("p", 2 * MIB as u64)).unwrap();
    assert_eq!(a.claim(run(0, 1)).len(), 1);
    assert_eq!(b.state().load(0).unwrap().state(), Some(CellState::Claimed));
    assert_eq!(s.packs(), vec![pack("p")]);
    let got = s.get(&pack("p")).unwrap();
    assert_eq!(got.size(), 2 * MIB as u64);
    let concrete = s.mem_entry(&pack("p")).unwrap();
    assert_eq!(concrete.pack(), &pack("p"));
    assert_eq!(
        concrete.state().load(0).unwrap().state(),
        Some(CellState::Claimed)
    );
}

#[test]
fn reopening_with_another_size_is_a_size_mismatch() {
    let s = MemEntryStore::new();
    s.open(&spec("p", 100)).unwrap();
    let err = s.open(&spec("p", 101)).err().unwrap();
    match err {
        EntryError::SizeMismatch {
            pack: p,
            size,
            existing,
        } => {
            assert_eq!(p, pack("p"));
            assert_eq!(size, 101);
            assert_eq!(existing, 100);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_pack_too_large_is_refused_and_not_listed() {
    let s = MemEntryStore::new();
    let size = (u64::from(u32::MAX) + 1) << 20;
    let err = s.open(&spec("big", size)).err().unwrap();
    assert!(matches!(err, EntryError::TooLarge { size: n, .. } if n == size));
    assert!(s.packs().is_empty());
}

#[test]
fn packs_lists_every_open_entry_in_order() {
    let s = MemEntryStore::new();
    s.open(&spec("b", 1)).unwrap();
    s.open(&spec("a", 1)).unwrap();
    assert_eq!(s.packs(), vec![pack("a"), pack("b")]);
}

#[test]
fn remove_drops_an_unheld_entry_and_keeps_a_held_one() {
    let s = MemEntryStore::new();
    let held = s.open(&spec("held", 1)).unwrap();
    s.open(&spec("free", 1)).unwrap();
    held.hold(HolderId(1));
    assert!(!s.remove(&pack("held")).unwrap());
    assert!(s.remove(&pack("free")).unwrap());
    assert!(!s.remove(&pack("free")).unwrap());
    assert!(!s.remove(&pack("never")).unwrap());
    assert_eq!(s.packs(), vec![pack("held")]);
    assert!(held.release(HolderId(1)));
    assert!(s.remove(&pack("held")).unwrap());
    assert!(s.get(&pack("held")).is_none());
}

#[test]
fn finish_zeroes_the_bytes_before_the_cell_is_claimable_so_a_later_publish_survives() {
    let e = entry(1);
    claim_publish(&e, 0, 7);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert!(e.evict_cell_cas(0, EvictStep::Finish));
    // The cell is absent only once its bytes are zero; a claim and publish after that keep theirs.
    assert_eq!(e.bytes(0, MIB), cell_bytes(0));
    claim_publish(&e, 0, 3);
    assert_eq!(e.bytes(0, MIB), cell_bytes(3));
    // A finish of a cell that is no longer evicting changes no bytes.
    assert!(!e.evict_cell_cas(0, EvictStep::Finish));
    assert_eq!(e.bytes(0, MIB), cell_bytes(3));
}
