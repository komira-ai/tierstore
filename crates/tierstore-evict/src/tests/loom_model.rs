//! The reader/evictor protocol of 5.1 under every interleaving loom explores (section 14 row:
//! "Loom: reader/evictor protocol with per-cell CAS, neighbouring cells going
//! `absent -> claimed -> present` during the scan, a reader straddling groups, post-read
//! recheck: no reader returns bytes of a cell whose punch has begun; the evictor never changes a
//! `claimed` cell").
//!
//! The evictor is this crate's own [`protocol::run`], unchanged, over [`LoomEntry`]: an `Entry`
//! whose cell bytes, reader counters and data are loom atomics. A cell's "data" is 1 while its
//! bytes are on disk and 0 once punched, so a reader that commits a reply with a 0 has served
//! zeros. The reader is the mount's protocol (5.1 reader steps 1 to 5) written out here.
//!
//! Three threads: the evictor on cells 14 to 17, a reader of cells 15 and 16 (groups 0 and 1,
//! so it straddles), and a fetch that claims and publishes cell 17 while the evictor runs. The
//! evictor's pause yields to loom and moves the model clock 1 s, so `evict_wait` (2 s) gives up
//! after two polls: the timeout path is explored too.
//!
//! **SeqCst in loom.** Loom models a `SeqCst` load, store or read-modify-write as only acquire or
//! release (its documented limitation), which lets both sides of the Dekker pattern read stale
//! values: something `SeqCst` on all four accesses forbids. A `SeqCst` fence loom does model, so
//! the model's entry and reader put one right after each side's store (the evictor's CAS, the
//! reader's `fetch_add`): that restores the order the real code has, and nothing more.
//!
//! Two further models check that each guard holds alone: the reader counters with no post-read
//! recheck, and the post-read recheck with counters the evictor cannot see. A last model removes
//! both and must fail: the model detects the defect it exists for.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use loom::sync::atomic::{AtomicU8, AtomicU16, Ordering::SeqCst, fence};
use tierstore_fetch_api::cell::{CellByte, CellState, Transition};
use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EvictStep, PunchReport,
    Settle, StateView,
};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::GROUP_CELLS;

use crate::config::CELL_BYTES;
use crate::pause::Pause;
use crate::protocol::{self, Batch};

/// Two groups.
const CELLS: u32 = 32;

/// An entry of loom atomics.
struct LoomEntry {
    pack: PackId,
    cells: Vec<AtomicU8>,
    /// 1 while the cell's bytes are on disk, 0 once punched.
    data: Vec<AtomicU8>,
    readers: Vec<AtomicU16>,
    /// Whether `group_readers` reports the counters (false: an evictor that ignores readers).
    counters_seen: bool,
}

impl LoomEntry {
    fn new(present: &[u32], counters_seen: bool) -> Self {
        let byte = |c: u32| {
            if present.contains(&c) {
                CellByte::new(CellState::Present, true).0
            } else {
                CellByte::new(CellState::Absent, false).0
            }
        };
        Self {
            pack: PackId {
                key: PackKey("loom".to_owned()),
                root: PackRoot::Digest([0; 32]),
            },
            cells: (0..CELLS).map(|c| AtomicU8::new(byte(c))).collect(),
            data: (0..CELLS)
                .map(|c| AtomicU8::new(u8::from(present.contains(&c))))
                .collect(),
            readers: (0..CELLS / GROUP_CELLS)
                .map(|_| AtomicU16::new(0))
                .collect(),
            counters_seen,
        }
    }

    fn cas(&self, cell: u32, t: Transition) -> bool {
        let slot = &self.cells[cell as usize];
        let cur = slot.load(SeqCst);
        match t.apply(CellByte(cur)) {
            Some(next) => {
                let won = slot.compare_exchange(cur, next.0, SeqCst, SeqCst).is_ok();
                fence(SeqCst);
                won
            }
            None => false,
        }
    }

    fn state_of(&self, cell: u32) -> CellState {
        CellByte(self.cells[cell as usize].load(SeqCst))
            .state()
            .unwrap_or(CellState::Bad)
    }
}

impl StateView for LoomEntry {
    fn cell_count(&self) -> u32 {
        CELLS
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        self.cells
            .get(cell as usize)
            .map(|c| CellByte(c.load(SeqCst)))
    }
}

impl Entry for LoomEntry {
    fn pack(&self) -> &PackId {
        &self.pack
    }

    fn size(&self) -> u64 {
        u64::from(CELLS) * CELL_BYTES
    }

    fn state(&self) -> &dyn StateView {
        self
    }

    fn claim(&self, run: CellRun) -> Vec<ClaimedCell> {
        run.cells()
            .filter(|&c| self.cas(c, Transition::Claim))
            .map(ClaimedCell::won)
            .collect()
    }

    fn publish(&self, cell: ClaimedCell, _bytes: &[u8]) -> Result<(), EntryError> {
        let c = cell.cell();
        self.data[c as usize].store(1, SeqCst);
        if self.cas(c, Transition::Publish) {
            Ok(())
        } else {
            Err(EntryError::NotClaimed {
                pack: self.pack.clone(),
                cell: c,
            })
        }
    }

    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool {
        self.cas(cell, step.transition())
    }

    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        let cells: Vec<u32> = runs.iter().flat_map(|r| r.cells()).collect();
        if let Some(&cell) = cells
            .iter()
            .find(|&&c| self.state_of(c) != CellState::Evicting)
        {
            return Err(EntryError::NotEvicting {
                pack: self.pack.clone(),
                cell,
            });
        }
        for &c in &cells {
            self.data[c as usize].store(0, SeqCst);
        }
        Ok(PunchReport {
            journal_syncs: 1,
            records: runs.len() as u32,
            bytes_punched: cells.len() as u64 * CELL_BYTES,
        })
    }

    fn group_readers(&self, group: u32) -> u64 {
        if self.counters_seen {
            u64::from(self.readers[group as usize].load(SeqCst))
        } else {
            0
        }
    }

    fn abandon(&self, _cell: ClaimedCell) {
        unreachable!("not in the model")
    }

    fn mark_bad(&self, _cell: ClaimedCell) {
        unreachable!("not in the model")
    }

    fn settle(&self, _cell: u32, _how: Settle) -> bool {
        unreachable!("not in the model")
    }

    fn counts(&self) -> CellCounts {
        unreachable!("not in the model")
    }

    fn last_use(&self) -> u64 {
        unreachable!("not in the model")
    }

    fn touch(&self, _now_ms: u64, _group: u32) {
        unreachable!("not in the model")
    }

    fn touch_epoch(&self, _group: u32) -> Option<u32> {
        unreachable!("not in the model")
    }

    fn read_cell(&self, _cell: u32) -> Result<Vec<u8>, EntryError> {
        unreachable!("not in the model")
    }

    fn fds(&self) -> Option<EntryFds<'_>> {
        None
    }

    fn hold(&self, _holder: HolderId) {
        unreachable!("not in the model")
    }

    fn release(&self, _holder: HolderId) -> bool {
        unreachable!("not in the model")
    }

    fn holders(&self) -> u32 {
        unreachable!("not in the model")
    }

    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        unreachable!("not in the model")
    }
}

/// The model's clock: only the evictor's pause moves it, so a plain atomic (outside loom's
/// model) is enough.
#[derive(Default)]
struct ModelClock(std::sync::atomic::AtomicU64);

impl Clock for ModelClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Yields to the other threads and moves the clock 1 s.
struct YieldPause(Arc<ModelClock>);

impl Pause for YieldPause {
    fn pause(&self, _attempt: u32) {
        self.0
            .0
            .fetch_add(1000, std::sync::atomic::Ordering::SeqCst);
        loom::thread::yield_now();
    }
}

/// The mount's read of `cells` (5.1 reader steps 1 to 5): whether it committed a reply, and
/// panics if that reply held a punched cell's zeros. `recheck` is step 4.
fn read(e: &LoomEntry, cells: &[u32], recheck: bool) -> bool {
    let mut groups: Vec<u32> = cells.iter().map(|c| c / GROUP_CELLS).collect();
    groups.dedup();
    for &g in &groups {
        e.readers[g as usize].fetch_add(1, SeqCst);
    }
    fence(SeqCst);
    let leave = || {
        for &g in &groups {
            e.readers[g as usize].fetch_sub(1, SeqCst);
        }
    };
    let ready = |e: &LoomEntry| cells.iter().all(|&c| e.state_of(c) == CellState::Present);
    if !ready(e) {
        leave();
        return false;
    }
    let bytes: Vec<u8> = cells
        .iter()
        .map(|&c| e.data[c as usize].load(SeqCst))
        .collect();
    if recheck && !ready(e) {
        leave();
        return false;
    }
    assert!(
        bytes.iter().all(|&b| b == 1),
        "a reader committed zeros of a punched cell: {bytes:?}"
    );
    leave();
    true
}

/// One model: the evictor on cells 14 to 17, a reader of 15 and 16, a fetch of 17.
fn model(counters_seen: bool, recheck: bool) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(move || {
        let e = Arc::new(LoomEntry::new(&[14, 15, 16], counters_seen));
        let clock = Arc::new(ModelClock::default());

        let evictor = {
            let e = e.clone();
            let clock = clock.clone();
            loom::thread::spawn(move || {
                let batch = Batch {
                    entry: e as Arc<dyn Entry>,
                    cells: vec![14, 15, 16, 17],
                };
                let pause = YieldPause(clock.clone());
                protocol::run(&[batch], clock.as_ref(), &pause, 2_000)
            })
        };
        let reader = {
            let e = e.clone();
            loom::thread::spawn(move || read(&e, &[15, 16], recheck))
        };
        // The fetch of cell 17, absent when the evictor began.
        let claimed = e.claim(CellRun {
            first: 17,
            count: 1,
        });
        assert_eq!(claimed.len(), 1);
        for token in claimed {
            e.publish(token, &[])
                .expect("the evictor never changes a claimed cell");
        }

        let out = evictor.join().unwrap();
        reader.join().unwrap();
        // Every cell the evictor won ended `absent` (punched) or back `present` (timed out).
        assert_eq!(out.won, out.evicted + out.restored);
        assert!(out.punch_failures.is_empty());
        // A present cell always has its bytes; a punched one is absent.
        for c in 14..=17 {
            let s = e.state_of(c);
            assert!(
                matches!(s, CellState::Present | CellState::Absent),
                "{c}: {s:?}"
            );
            if s == CellState::Present {
                assert_eq!(
                    e.data[c as usize].load(SeqCst),
                    1,
                    "cell {c} present without bytes"
                );
            }
        }
    });
}

#[test]
fn no_reader_commits_zeros_and_no_claimed_cell_is_touched() {
    model(true, true);
}

#[test]
fn the_reader_counters_alone_keep_zeros_from_a_reader() {
    model(true, false);
}

#[test]
fn the_post_read_recheck_alone_keeps_zeros_from_a_reader() {
    model(false, true);
}

#[test]
fn without_either_guard_the_model_finds_a_reader_committing_zeros() {
    let found = catch_unwind(AssertUnwindSafe(|| model(false, false)));
    assert!(found.is_err(), "the model must catch a reader served zeros");
}
