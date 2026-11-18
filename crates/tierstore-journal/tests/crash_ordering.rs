//! Crash ordering (section 8; section 14 rows "Crash between `pwrite` and journal append: cell
//! absent after restart; no journal record visible without its data" and "Crash after a batched
//! PUNCH fsync and before/after the punch: replay never marks a punched cell `present`").
//!
//! A scripted workload (publish, commit, publish, evict with a batched punch, commit, publish
//! again into a punched cell, commit, close) runs on [`MemFs`] and is killed at **every**
//! mutating file operation in turn. The file system each kill leaves is restarted after a process
//! crash (same boot) and after a power loss (another boot) that keeps none, all, or a seeded
//! choice of the writes not yet synced, torn writes included. After every restart:
//!
//! - **no zeros or stale bytes are ever servable**: every cell restored `present` holds the
//!   bytes of its last residency, and they are durable;
//! - **no journal record without its data**: every cell the journal restores (present or
//!   recheck) whose commit was acknowledged reads its committed bytes;
//! - **a power loss restores nothing servable**: every restored cell is `recheck`;
//! - **acknowledged commits survive**: a cell whose `commit_batch` returned, and whose eviction
//!   never started, is restored (`present` in the same boot, `recheck` in another);
//! - **acknowledged punches stay punched**: a cell whose `punch` returned is not restored until
//!   it is published again;
//! - no `durable` bit was set before the bytes were synced, and a second restart restores the
//!   same cells as the first.
//!
//! Catches: a record appended before the data's `fdatasync`; a `durable` bit set before it; a
//! punch done before its `PUNCH` record is synced; a replay that ignores `PUNCH` or applies
//! records out of order; a fold that resets the journal before its snapshot's rename is durable;
//! a stale journal tail replayed under a new epoch; a power-loss restart that trusts a cell.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use support::*;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::Entry;
use tierstore_fetch_api::ids::CellRun;
use tierstore_journal::FileKind;
use tierstore_journal::fake::{Crash, FsEntry, Keep, MemFs};

/// What the workload knows was acknowledged when it was killed.
#[derive(Debug, Default)]
struct Acked {
    /// Cells whose commit returned, with the residency committed; removed when an eviction of
    /// the cell starts.
    committed: BTreeMap<u32, u32>,
    /// Cells whose punch returned and that were not published again since (once published
    /// again, a commit may reach the journal before the kill, and the cell may come back with
    /// its new bytes, which the `present` check covers).
    punched: BTreeSet<u32>,
    /// The last residency published into each cell (its bytes are what a read must see).
    published: BTreeMap<u32, u32>,
}

fn run(fs: &MemFs, acked: &mut Acked) -> Result<(), tierstore_fetch_api::entry::EntryError> {
    let (entry, _) = open(fs, Some(BOOT_A))?;
    let step_publish = |entry: &FsEntry, acked: &mut Acked, cells: &[u32], r: u32| {
        for &c in cells {
            acked.published.insert(c, r);
            acked.punched.remove(&c);
        }
        publish(entry, cells, r)
    };
    let step_commit = |entry: &FsEntry, acked: &mut Acked| {
        let pending: Vec<u32> = (0..CELLS)
            .filter(|c| state(entry, *c) == Some(CellState::Present))
            .collect();
        entry.commit_batch()?;
        for c in pending {
            let r = acked.published[&c];
            acked.committed.insert(c, r);
        }
        Ok::<(), tierstore_fetch_api::entry::EntryError>(())
    };
    step_publish(&entry, acked, &[0, 1, 2], 1)?;
    step_commit(&entry, acked)?;
    step_publish(&entry, acked, &[3], 1)?;
    let run12 = CellRun { first: 1, count: 2 };
    for c in run12.cells() {
        acked.committed.remove(&c);
    }
    evict(&entry, &[run12])?;
    acked.punched.extend(run12.cells());
    step_commit(&entry, acked)?;
    step_publish(&entry, acked, &[1], 2)?;
    step_commit(&entry, acked)?;
    entry.close()
}

/// The ways a killed run is restarted, with the boot of the restart.
fn restarts() -> Vec<(Crash, Option<tierstore_journal::BootId>)> {
    let mut r = vec![
        (Crash::Process, Some(BOOT_A)),
        (Crash::Power(Keep::Nothing), Some(BOOT_B)),
        (Crash::Power(Keep::Everything), Some(BOOT_B)),
    ];
    r.extend((0..8).map(|s| (Crash::Power(Keep::Seeded(s)), Some(BOOT_B))));
    r
}

/// The bytes of every (cell, residency) the workload writes, made once.
type Expected = BTreeMap<(u32, u32), Vec<u8>>;

fn expected() -> Expected {
    (0..CELLS)
        .flat_map(|c| [1, 2].map(|r| ((c, r), cell_bytes(c, r))))
        .collect()
}

/// Restarts `crashed` in `boot` and checks the invariants; `trusted` says the restart is in the
/// boot that wrote the committed cells (a process crash of the workload itself).
fn check(
    what: &str,
    trusted: bool,
    crashed: &MemFs,
    boot: Option<tierstore_journal::BootId>,
    acked: &Acked,
    expected: &Expected,
) {
    let (entry, rec) = open(crashed, boot).unwrap_or_else(|e| panic!("{what}: open: {e}"));
    let data = crashed.current(FileKind::Data).expect("data file");
    for c in 0..CELLS {
        let s = state(&entry, c).expect("state");
        let start = (u64::from(c) * MIB) as usize;
        let on_disk = &data[start..start + cell_len(c)];
        if s == CellState::Present {
            let r = acked.published[&c];
            assert_eq!(
                on_disk,
                expected[&(c, r)].as_slice(),
                "{what}: cell {c} present with wrong bytes"
            );
            assert!(
                crashed.is_durable(FileKind::Data, start as u64, cell_len(c) as u64),
                "{what}: cell {c} present but its bytes are not durable"
            );
        }
        if let Some(r) = acked.committed.get(&c) {
            let expect = if trusted {
                CellState::Present
            } else {
                CellState::Recheck
            };
            assert_eq!(s, expect, "{what}: acknowledged commit of cell {c} lost");
            assert_eq!(
                on_disk,
                expected[&(c, *r)].as_slice(),
                "{what}: record of cell {c} without its data"
            );
        }
        if acked.punched.contains(&c) {
            assert_eq!(s, CellState::Absent, "{what}: punched cell {c} restored");
        }
    }
    if !trusted {
        assert_eq!(
            rec.count(CellState::Present),
            0,
            "{what}: a power loss restored a servable cell"
        );
    }
    assert_eq!(entry.early_durable(), 0, "{what}");
    // A second restart, same boot, restores what the first did.
    let again = entry.fs().crash(Crash::Process);
    let (_, rec2) = open(&again, boot).unwrap_or_else(|e| panic!("{what}: second open: {e}"));
    assert_eq!(
        restored(&rec2),
        restored(&rec),
        "{what}: second restart differs"
    );
}

#[test]
fn a_kill_at_every_file_operation_never_restores_unsynced_or_punched_bytes() {
    // The full run's mutations bound the kill points.
    let full = MemFs::new();
    let mut acked = Acked::default();
    run(&full, &mut acked).expect("the workload runs to its end without a kill");
    let total = full.mutations();
    assert!(total > 30, "the workload does {total} mutations");
    assert_eq!(
        full.count(tierstore_journal::fake::Op::PunchData, Some(FileKind::Data)),
        1
    );

    let expected = expected();
    for kill in 0..=total {
        let fs = MemFs::new();
        fs.die_after(kill);
        let mut acked = Acked::default();
        let ended = run(&fs, &mut acked);
        assert_eq!(
            ended.is_err(),
            kill < total,
            "kill {kill}: the run ends early only when killed"
        );
        for (how, boot) in restarts() {
            let crashed = fs.crash(how);
            let what = format!("killed after {kill} mutations, restarted after {how:?}");
            check(
                &what,
                matches!(how, Crash::Process),
                &crashed,
                boot,
                &acked,
                &expected,
            );
        }
    }
}

#[test]
fn a_kill_during_the_restart_after_a_power_loss_loses_no_acknowledged_commit() {
    // The restart in another boot folds (a new snapshot and journal, written in boot B). It is
    // killed at every one of its own mutations, and the file system is crashed again: by a
    // process crash (boot B again) and by a second power loss (boot C). The cells boot A
    // committed must still be restored, and only as `recheck`.
    let expected = expected();
    let full = MemFs::new();
    let mut acked = Acked::default();
    run(&full, &mut acked).unwrap();
    let total = full.mutations();
    let boot_c = tierstore_journal::BootId([0xc3; 16]);
    for kill in (0..=total).step_by(3) {
        let fs = MemFs::new();
        fs.die_after(kill);
        let mut acked = Acked::default();
        let _ = run(&fs, &mut acked);
        let lost = fs.crash(Crash::Power(Keep::Nothing));
        let measured = lost.crash(Crash::Process);
        open(&measured, Some(BOOT_B)).unwrap();
        for k in 0..measured.mutations() {
            let again = lost.crash(Crash::Process);
            again.die_after(k);
            assert!(
                open(&again, Some(BOOT_B)).is_err(),
                "kill {kill}, restart killed at {k}"
            );
            for (how, boot) in [
                (Crash::Process, Some(BOOT_B)),
                (Crash::Power(Keep::Nothing), Some(boot_c)),
                (Crash::Power(Keep::Everything), Some(boot_c)),
            ] {
                let what = format!("killed after {kill}, restart killed after {k}, then {how:?}");
                check(&what, false, &again.crash(how), boot, &acked, &expected);
            }
        }
    }
}

#[test]
fn a_crash_between_pwrite_and_the_journal_append_leaves_the_cell_absent() {
    let fs = MemFs::new();
    let (entry, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&entry, &[0], 1).unwrap();
    assert_eq!(state(&entry, 0), Some(CellState::Present));
    // The pwrite happened; no commit did.
    for (how, boot) in restarts() {
        let (after, rec) = open(&fs.crash(how), boot).unwrap();
        assert_eq!(state(&after, 0), Some(CellState::Absent), "{how:?}");
        assert_eq!(rec.records, 0, "{how:?}");
    }
}

#[test]
fn no_add_record_is_written_before_the_data_file_is_synced() {
    let fs = MemFs::new();
    let (entry, _) = open(&fs, Some(BOOT_A)).unwrap();
    publish(&entry, &[0, 1], 1).unwrap();
    // The data file's sync fails: nothing reaches the journal and no bit is set.
    let journal_before = fs.current(FileKind::Journal).unwrap();
    fs.fail(
        tierstore_journal::fake::Op::SyncData,
        Some(FileKind::Data),
        0,
        std::io::ErrorKind::Other,
    );
    assert!(entry.commit_batch().is_err());
    assert_eq!(fs.current(FileKind::Journal).unwrap(), journal_before);
    for c in [0, 1] {
        assert!(!entry.state().load(c).unwrap().durable(), "cell {c}");
    }
    let (after, _) = open(&fs.crash(Crash::Process), Some(BOOT_A)).unwrap();
    assert_eq!(states(&after)[..2], [CellState::Absent, CellState::Absent]);
}
