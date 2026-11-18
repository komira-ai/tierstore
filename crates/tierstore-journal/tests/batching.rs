//! Journal batching (section 8; section 14 row "Journal batching: at most one `fdatasync` per
//! dirty entry per interval; `durable` set only after it").
//!
//! Time is the injected clock's milliseconds passed to the committer; no test waits.
//!
//! Catches: a commit per part or per demand sub-range; a second `fdatasync` of an entry within
//! one interval below the byte threshold; a round that waits out the interval past the
//! threshold; a `durable` bit set before the data's sync (or on a cell published after the
//! round's sync); a round on an entry with nothing new; a failing entry that stops the round for
//! the others.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;
use std::sync::Arc;

use support::*;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::entry::{Entry, EntrySpec};
use tierstore_fetch_api::ids::{PackId, PackKey, PackRoot};
use tierstore_journal::fake::{FsEntry, MemFs, Op};
use tierstore_journal::{CommitPolicy, Committer, FileKind};

fn other_pack() -> PackId {
    PackId {
        key: PackKey("packs/ffff/other.pack".to_owned()),
        root: PackRoot::Digest([9; 32]),
    }
}

fn entry_on(fs: &MemFs, pack: PackId) -> Arc<FsEntry> {
    let spec = EntrySpec { pack, size: SIZE };
    Arc::new(FsEntry::open(fs.clone(), &spec, Some(BOOT_A)).unwrap().0)
}

fn data_syncs(fs: &MemFs) -> u64 {
    fs.count(Op::SyncData, Some(FileKind::Data))
}

#[test]
fn the_default_policy_is_one_second_or_thirty_two_mib() {
    assert_eq!(
        CommitPolicy::default(),
        CommitPolicy {
            interval_ms: 1000,
            pending_bytes: 32 << 20
        }
    );
}

#[test]
fn every_dirty_entry_is_synced_at_most_once_per_interval() {
    let clock = ManualClock::new();
    let committer = Committer::new(CommitPolicy::default());
    let (fs1, fs2) = (MemFs::new(), MemFs::new());
    let e1 = entry_on(&fs1, pack());
    let e2 = entry_on(&fs2, other_pack());
    let (d1, d2): (Arc<dyn Entry>, Arc<dyn Entry>) = (e1.clone(), e2.clone());
    clock.advance(1000);

    // Four parts of entry 1 and two of entry 2 land within one interval, one cell each.
    for (entry, dyn_entry, cell) in [
        (&e1, &d1, 0),
        (&e2, &d2, 0),
        (&e1, &d1, 1),
        (&e1, &d1, 2),
        (&e2, &d2, 1),
        (&e1, &d1, 3),
    ] {
        publish(entry, &[cell], 1).unwrap();
        assert!(!committer.published(dyn_entry, cell_len(cell) as u64));
        // Nothing is durable before its round.
        assert!(!entry.state().load(cell).unwrap().durable());
    }
    assert_eq!(committer.dirty(), 2);
    let round = committer
        .commit_due(clock.now_ms())
        .expect("the first round is due");
    assert_eq!(
        (round.entries, round.data_syncs, round.cells_durable),
        (2, 2, 6)
    );
    assert_eq!(round.records, 2, "one contiguous run per entry");
    assert!(round.failed.is_empty());
    assert_eq!((data_syncs(&fs1), data_syncs(&fs2)), (1, 1));
    assert_eq!(e1.early_durable() + e2.early_durable(), 0);

    // More parts within the same interval: no round until the interval has passed.
    evict(
        &e1,
        &[tierstore_fetch_api::ids::CellRun { first: 0, count: 1 }],
    )
    .unwrap();
    publish(&e1, &[0], 2).unwrap();
    committer.published(&d1, MIB);
    clock.advance(999);
    assert!(!committer.due(clock.now_ms()));
    assert!(committer.commit_due(clock.now_ms()).is_none());
    assert_eq!(data_syncs(&fs1), 1);
    assert!(!e1.state().load(0).unwrap().durable());
    clock.advance(1);
    assert!(committer.due(clock.now_ms()));
    let round = committer.commit_due(clock.now_ms()).unwrap();
    assert_eq!(
        (round.entries, round.data_syncs, round.cells_durable),
        (1, 1, 1)
    );
    assert_eq!(data_syncs(&fs1), 2);
    assert!(e1.state().load(0).unwrap().durable());

    // Nothing dirty: never due, however long.
    clock.advance(60_000);
    assert!(!committer.due(clock.now_ms()));
    assert!(committer.commit_due(clock.now_ms()).is_none());
}

#[test]
fn thirty_two_mib_pending_starts_a_round_before_the_interval() {
    let policy = CommitPolicy {
        interval_ms: 1000,
        pending_bytes: 3 * MIB,
    };
    let committer = Committer::new(policy);
    let fs = MemFs::new();
    let e = entry_on(&fs, pack());
    let d: Arc<dyn Entry> = e.clone();
    // A first round at t = 1000 resets the interval.
    publish(&e, &[0], 1).unwrap();
    committer.published(&d, MIB);
    assert!(committer.commit_due(1000).is_some());
    publish(&e, &[1, 2], 1).unwrap();
    assert!(!committer.published(&d, MIB));
    assert!(!committer.due(1001));
    publish(&e, &[3], 1).unwrap();
    assert!(committer.published(&d, 2 * MIB), "the threshold is reached");
    assert!(committer.due(1001));
    let round = committer.commit_due(1001).unwrap();
    assert_eq!(round.cells_durable, 3);
    assert_eq!(data_syncs(&fs), 2);
}

#[test]
fn a_round_with_nothing_new_on_an_entry_does_not_sync_it() {
    let committer = Committer::new(CommitPolicy::default());
    let fs = MemFs::new();
    let e = entry_on(&fs, pack());
    let d: Arc<dyn Entry> = e.clone();
    // Marked dirty, but the cell was evicted before the round: nothing to make durable.
    publish(&e, &[0], 1).unwrap();
    committer.published(&d, MIB);
    evict(
        &e,
        &[tierstore_fetch_api::ids::CellRun { first: 0, count: 1 }],
    )
    .unwrap();
    let round = committer.commit_due(5000).unwrap();
    assert_eq!(
        (round.entries, round.data_syncs, round.cells_durable),
        (1, 0, 0)
    );
    assert_eq!(data_syncs(&fs), 0);
}

#[test]
fn a_cell_published_during_eviction_waits_for_the_round_after_its_abort() {
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&e, &[0], 1).unwrap();
    // The evictor wins the cell before the commit: it stays pending.
    assert!(e.evict_cell_cas(0, tierstore_fetch_api::entry::EvictStep::Begin));
    assert_eq!(e.commit_batch().unwrap().data_syncs, 0);
    assert_eq!(e.journal().pending(), 1);
    // The eviction is aborted: the next commit makes it durable.
    assert!(e.evict_cell_cas(0, tierstore_fetch_api::entry::EvictStep::Abort));
    let report = e.commit_batch().unwrap();
    assert_eq!(
        (report.data_syncs, report.cells_durable, report.records),
        (1, 1, 1)
    );
    assert_eq!(state(&e, 0), Some(CellState::Present));
    assert!(e.state().load(0).unwrap().durable());
}

/// Cells whose state moves under the commit: a `CellStates` whose `mark_durable` loses to an
/// evictor (the cell goes `evicting` between the commit's load and its CAS), or to a cell that
/// went absent.
#[test]
fn a_cell_that_goes_evicting_between_the_sync_and_its_durable_cas_stays_pending() {
    use std::cell::RefCell;
    use tierstore_fetch_api::cell::{CellByte, CellState};
    use tierstore_journal::CellStates;

    struct Racy {
        bytes: RefCell<Vec<CellByte>>,
    }
    impl CellStates for Racy {
        fn load(&self, cell: u32) -> Option<CellByte> {
            self.bytes.borrow().get(cell as usize).copied()
        }
        fn mark_durable(&self, cell: u32) -> bool {
            // The evictor wins cell 0, and cell 1 was punched, just before the CAS.
            let mut b = self.bytes.borrow_mut();
            b[0] = CellByte::new(CellState::Evicting, false);
            b[1] = CellByte::new(CellState::Absent, false);
            if cell == 2 {
                b[2] = CellByte::new(CellState::Present, true);
                return true;
            }
            // Cell 3's CAS loses to a byte already durable: nothing to requeue.
            b[3] = CellByte::new(CellState::Present, true);
            false
        }
        fn last_use(&self) -> u64 {
            0
        }
    }
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    let racy = Racy {
        bytes: RefCell::new(vec![CellByte::new(CellState::Present, false); 4]),
    };
    for c in 0..4 {
        e.journal().published(c);
    }
    let report = e.journal().commit_batch(&racy).unwrap();
    assert_eq!(
        (report.data_syncs, report.cells_durable, report.records),
        (1, 1, 1)
    );
    // Cell 0 waits for its eviction to end; cells 1 (absent) and 3 (durable) are not requeued.
    assert_eq!(e.journal().pending(), 1);
    let committed: Vec<u32> = e.journal().committed().iter().collect();
    assert_eq!(committed, [2]);
}

#[test]
fn an_entry_whose_commit_fails_is_reported_and_the_others_are_committed() {
    let committer = Committer::new(CommitPolicy::default());
    let (fs1, fs2) = (MemFs::new(), MemFs::new());
    let e1 = entry_on(&fs1, pack());
    let e2 = entry_on(&fs2, other_pack());
    for (e, fs) in [(&e1, &fs1), (&e2, &fs2)] {
        publish(e, &[0], 1).unwrap();
        let d: Arc<dyn Entry> = e.clone();
        committer.published(&d, MIB);
        let _ = fs;
    }
    fs1.fail(Op::SyncData, Some(FileKind::Data), 0, ErrorKind::Other);
    let round = committer.commit_due(1000).unwrap();
    assert_eq!(round.entries, 2);
    assert_eq!(round.failed.len(), 1);
    assert_eq!(round.failed[0].0, pack());
    assert_eq!(round.cells_durable, 1);
    assert!(e2.state().load(0).unwrap().durable());
    assert!(!e1.state().load(0).unwrap().durable());
    let shown = format!("{committer:?}");
    assert!(
        shown.contains("Committer") && shown.contains("last_round_ms: 1000"),
        "{shown}"
    );
}

#[test]
fn a_cell_whose_durable_cas_loses_but_that_is_present_again_stays_pending() {
    use std::cell::Cell;
    use tierstore_fetch_api::cell::CellByte;
    use tierstore_journal::CellStates;

    /// An evictor begins and aborts cell 0 between the commit's load and its CAS: the CAS loses,
    /// and the reload sees the cell `present` and not durable again.
    struct BeginThenAbort {
        lose: Cell<bool>,
    }
    impl CellStates for BeginThenAbort {
        fn load(&self, _cell: u32) -> Option<CellByte> {
            Some(CellByte::new(CellState::Present, false))
        }
        fn mark_durable(&self, _cell: u32) -> bool {
            !self.lose.replace(false)
        }
        fn last_use(&self) -> u64 {
            0
        }
    }
    let fs = MemFs::new();
    let (e, _) = open(&fs, Some(BOOT_A)).unwrap();
    let racy = BeginThenAbort {
        lose: Cell::new(true),
    };
    e.journal().published(0);
    let first = e.journal().commit_batch(&racy).unwrap();
    assert_eq!((first.data_syncs, first.cells_durable), (1, 0));
    assert_eq!(e.journal().pending(), 1, "the cell is not forgotten");
    let second = e.journal().commit_batch(&racy).unwrap();
    assert_eq!((second.cells_durable, second.records), (1, 1));
    assert!(e.journal().committed().contains(0));
}

#[test]
fn the_committer_visits_an_entry_again_after_an_eviction_of_an_undurable_cell_is_aborted() {
    use tierstore_fetch_api::entry::EvictStep;
    let committer = Committer::new(CommitPolicy::default());
    let fs = MemFs::new();
    let e = entry_on(&fs, pack());
    let d: Arc<dyn Entry> = e.clone();
    publish(&e, &[0], 1).unwrap();
    committer.published(&d, MIB);
    // The evictor wins the cell before the round: the round leaves it pending in the journal.
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    let round = committer.commit_due(1000).unwrap();
    assert_eq!(round.cells_durable, 0);
    assert_eq!(committer.dirty(), 0);
    // A reader outlived `evict_wait`: the evictor aborts, and tells the committer (0 bytes).
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(e.journal().pending(), 1);
    committer.published(&d, 0);
    let round = committer.commit_due(2000).unwrap();
    assert_eq!(round.cells_durable, 1);
    assert!(e.state().load(0).unwrap().durable());
    // Aborting the eviction of a durable cell needs no commit.
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(e.journal().pending(), 0);
}
