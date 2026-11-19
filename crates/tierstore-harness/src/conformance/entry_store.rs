//! The cases of one entry's cell protocol: claim, abandon, publish, bad, settle, the evictor's
//! CAS steps, punch, commit, counts, touch and the state view (sections 2, 5.1, 7 and 8).
//!
//! # Panics
//!
//! Every case panics, with a message that starts `conformance:`, when the implementation breaks
//! the contract the case checks.

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::entry::{
    CellCounts, CommitReport, EntryError, EvictStep, PunchReport, Settle,
};
use tierstore_fetch_api::page::epoch_of;

use super::EntryStoreFixture;
use super::check::{ensure, ensure_eq, expect_err, expect_ok};
use super::support::{
    MIB, at, bytes, cb, cell_len, cells, open, pack, publish, punch_as_able, run, same, state_of,
    unsupported,
};

/// A new entry has every cell absent and not durable, counts them so, was never used, has no
/// holder, and the state view has no cell past the pack; a pack of no bytes has no cell.
pub fn new_entry_is_all_absent(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 3 * MIB + 1);
    ensure_eq(e.pack(), &pack("p"), "the entry names its pack");
    ensure_eq(e.size(), 3 * MIB + 1, "the entry reports its size");
    ensure_eq(e.state().cell_count(), 4, "a pack of 3 MiB + 1 has 4 cells");
    for c in 0..4 {
        ensure_eq(
            at(&*e, c),
            cb(CellState::Absent, false),
            format_args!("cell {c} of a new entry"),
        );
    }
    ensure_eq(at(&*e, 4), None, "the state view has no cell 4");
    ensure_eq(
        e.counts(),
        CellCounts {
            absent: 4,
            ..CellCounts::default()
        },
        "a new entry counts every cell absent",
    );
    ensure_eq(e.last_use(), 0, "a new entry was never used");
    ensure_eq(e.holders(), 0, "a new entry has no holder");

    let empty = open(&*store, "empty", 0);
    ensure_eq(
        empty.state().cell_count(),
        0,
        "a pack of no bytes has no cell",
    );
    ensure(
        empty.claim(run(0, 1)).is_empty(),
        "no cell of an empty pack is claimed",
    );
}

/// `claim` wins each absent cell of the run once, in ascending order; claimed, present, bad and
/// out-of-range cells and an empty run win nothing, and a second claim of the same run wins
/// nothing.
pub fn claim_wins_each_absent_cell_once_in_ascending_order(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 6 * MIB);
    ensure_eq(
        cells(&e.claim(run(1, 3))),
        vec![1, 2, 3],
        "a claim wins the absent cells of its run, ascending",
    );
    for c in 1..4 {
        ensure_eq(
            state_of(&*e, c),
            Some(CellState::Claimed),
            format_args!("won cell {c}"),
        );
    }
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Absent),
        "a cell outside the run stays absent",
    );
    ensure(
        e.claim(run(1, 3)).is_empty(),
        "a second claim of the same run wins nothing",
    );

    publish(&*e, 4, 7);
    let bad = e.claim(run(5, 1));
    for token in bad {
        e.mark_bad(token);
    }
    ensure_eq(
        cells(&e.claim(run(0, 20))),
        vec![0],
        "a claim skips claimed, present, bad and out-of-range cells",
    );
    ensure(
        e.claim(run(10, 3)).is_empty(),
        "a run past the pack wins nothing",
    );
    ensure(e.claim(run(0, 0)).is_empty(), "an empty run wins nothing");
    ensure_eq(
        state_of(&*e, 4),
        Some(CellState::Present),
        "a present cell is not claimed",
    );
    ensure_eq(
        state_of(&*e, 5),
        Some(CellState::Bad),
        "a bad cell is not claimed",
    );
}

/// `abandon` returns a claimed cell to absent, and the cell can be claimed again.
pub fn abandon_returns_the_cell_to_absent(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 2 * MIB);
    for token in e.claim(run(0, 2)) {
        if token.cell() == 0 {
            e.abandon(token);
        }
    }
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Absent),
        "an abandoned cell is absent",
    );
    ensure_eq(
        state_of(&*e, 1),
        Some(CellState::Claimed),
        "another claim is untouched",
    );
    ensure_eq(
        cells(&e.claim(run(0, 2))),
        vec![0],
        "an abandoned cell is claimable again",
    );
}

/// `publish` writes the bytes and makes the cell present (not durable), `read_cell` returns
/// them, and the short last cell of a pack takes and returns its own length.
pub fn publish_makes_the_cell_present_and_read_cell_returns_the_bytes(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 2 * MIB + 5);
    publish(&*e, 0, 7);
    publish(&*e, 2, 9);
    ensure_eq(
        at(&*e, 0),
        cb(CellState::Present, false),
        "a published cell is present, not durable",
    );
    ensure(e.state().ready(run(0, 1)), "a present cell is ready");
    ensure(
        !e.state().ready(run(0, 2)),
        "a run with an absent cell is not ready",
    );
    ensure(
        !e.state().ready(run(2, 2)),
        "a run that leaves the pack is not ready",
    );
    ensure(e.state().ready(run(1, 0)), "an empty run is ready");
    ensure_eq(cell_len(&*e, 2), 5, "the last cell is 5 bytes");
    ensure_eq(
        expect_ok(e.read_cell(0), "read of a present cell"),
        bytes(&*e, 0, 7),
        "read_cell returns the bytes published",
    );
    ensure_eq(
        expect_ok(e.read_cell(2), "read of the short last cell"),
        vec![9; 5],
        "read_cell returns the short last cell with its own length",
    );
    ensure_eq(
        e.counts(),
        CellCounts {
            absent: 1,
            present: 2,
            ..CellCounts::default()
        },
        "two present cells and an absent one",
    );
}

/// A publish of the wrong length is an error that names the lengths, returns the cell to
/// absent (claimable again), and writes nothing a later publish does not replace.
pub fn publish_of_the_wrong_length_returns_the_cell_to_absent(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", MIB + 5);
    for token in e.claim(run(0, 1)) {
        expect_err(
            e.publish(token, &[1; 10]),
            |err| {
                same(
                    err,
                    &EntryError::CellLength {
                        pack: pack("p"),
                        cell: 0,
                        expected: MIB,
                        got: 10,
                    },
                )
            },
            "a short publish of a whole cell",
        );
    }
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Absent),
        "a wrong-length publish leaves absent",
    );
    for token in e.claim(run(1, 1)) {
        expect_err(
            e.publish(token, &bytes(&*e, 0, 1)),
            |err| {
                same(
                    err,
                    &EntryError::CellLength {
                        pack: pack("p"),
                        cell: 1,
                        expected: 5,
                        got: MIB,
                    },
                )
            },
            "a whole cell of bytes for the short last cell",
        );
    }
    ensure_eq(
        state_of(&*e, 1),
        Some(CellState::Absent),
        "the short cell is absent again",
    );
    publish(&*e, 0, 3);
    publish(&*e, 1, 4);
    ensure_eq(
        expect_ok(e.read_cell(0), "read after the retry"),
        bytes(&*e, 0, 3),
        "a retried publish is what is read",
    );
}

/// A publish whose cell is not claimed is `NotClaimed` and leaves the cell and its bytes as
/// they are, whether the cell is absent or present.
pub fn publish_of_a_cell_that_is_not_claimed_changes_nothing(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 2 * MIB);
    publish(&*e, 1, 5);
    let not_claimed = |cell| {
        move |err: &EntryError| {
            same(
                err,
                &EntryError::NotClaimed {
                    pack: pack("p"),
                    cell,
                },
            )
        }
    };
    expect_err(
        e.publish(
            tierstore_fetch_api::entry::ClaimedCell::won(0),
            &bytes(&*e, 0, 1),
        ),
        not_claimed(0),
        "a publish of an absent cell",
    );
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Absent),
        "the absent cell stays absent",
    );
    expect_err(
        e.publish(
            tierstore_fetch_api::entry::ClaimedCell::won(1),
            &bytes(&*e, 1, 6),
        ),
        not_claimed(1),
        "a publish of a present cell",
    );
    ensure_eq(
        expect_ok(e.read_cell(1), "read of the present cell"),
        bytes(&*e, 1, 5),
        "the present cell keeps its bytes",
    );
}

/// `mark_bad` takes a claimed cell to bad: not claimable, not readable (`NotReadable` with the
/// byte seen), counted, and `ExpireBad` returns it to absent exactly once.
pub fn mark_bad_then_expire_returns_the_cell_to_absent(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", MIB);
    for token in e.claim(run(0, 1)) {
        e.mark_bad(token);
    }
    ensure_eq(at(&*e, 0), cb(CellState::Bad, false), "a bad cell");
    ensure(e.claim(run(0, 1)).is_empty(), "a bad cell is not claimable");
    ensure(!e.state().ready(run(0, 1)), "a bad cell is not ready");
    expect_err(
        e.read_cell(0),
        |err| {
            matches!(
                err,
                EntryError::NotReadable {
                    cell: 0,
                    state: CellByte(0x05),
                    ..
                }
            )
        },
        "a bad cell is not readable",
    );
    ensure_eq(e.counts().bad, 1, "a bad cell is counted bad");
    ensure(
        !e.settle(0, Settle::RecheckOk),
        "a bad cell does not settle as a recheck",
    );
    ensure(
        e.settle(0, Settle::ExpireBad),
        "an expired bad cell settles",
    );
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Absent),
        "an expired bad cell is absent",
    );
    ensure(!e.settle(0, Settle::ExpireBad), "a cell expires once");
    ensure_eq(
        cells(&e.claim(run(0, 1))),
        vec![0],
        "an expired bad cell is claimable",
    );
}

/// A recheck cell is readable and counted, settles to present on `RecheckOk` and to absent on
/// `RecheckFailed`, and no settle wins from another state or outside the pack.
pub fn settle_moves_recheck_cells_and_refuses_other_states(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 3 * MIB);
    for c in 0..2 {
        f.restore_as_recheck(&*store, &pack("p"), c, &bytes(&*e, c, 8));
    }
    ensure_eq(
        at(&*e, 0),
        cb(CellState::Recheck, false),
        "a restored cell is recheck",
    );
    ensure(
        !e.state().ready(run(0, 1)),
        "a recheck cell is not servable",
    );
    ensure(
        e.claim(run(0, 2)).is_empty(),
        "a recheck cell is not claimable",
    );
    ensure_eq(
        expect_ok(e.read_cell(1), "read of a recheck cell"),
        bytes(&*e, 1, 8),
        "a recheck cell reads its bytes",
    );
    ensure_eq(e.counts().recheck, 2, "two recheck cells");
    ensure_eq(e.counts().used(), 2, "a recheck cell counts as use");
    ensure(
        !e.settle(2, Settle::RecheckOk),
        "an absent cell does not settle as recheck",
    );
    ensure(e.settle(0, Settle::RecheckOk), "a matching re-hash settles");
    ensure_eq(
        at(&*e, 0),
        cb(CellState::Present, false),
        "RecheckOk makes the cell present",
    );
    ensure(
        e.settle(1, Settle::RecheckFailed),
        "a failed re-hash settles",
    );
    ensure_eq(
        state_of(&*e, 1),
        Some(CellState::Absent),
        "RecheckFailed makes the cell absent",
    );
    ensure(
        !e.settle(0, Settle::ExpireBad),
        "a present cell does not expire",
    );
    ensure(
        !e.settle(0, Settle::RecheckFailed),
        "a present cell is not a failed re-hash",
    );
    ensure(
        !e.settle(9, Settle::RecheckOk),
        "a cell outside the pack does not settle",
    );
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Present),
        "refused settles changed nothing",
    );
}

/// The evictor's steps keep the durable bit through evicting (0x82 -> 0x84 -> 0x82 on abort, 0x02
/// -> 0x04 -> 0x02), each wins once and only from its predecessor, a claimed cell is never changed
/// by any step, and a finished eviction leaves absent.
pub fn evict_steps_keep_the_durable_bit_and_refuse_wrong_states(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 4 * MIB);
    publish(&*e, 0, 1);
    expect_ok(e.commit_batch(), "commit of cell 0");
    publish(&*e, 1, 2);
    let claimed = e.claim(run(2, 1));
    ensure_eq(at(&*e, 0), Some(CellByte(0x82)), "a committed cell is 0x82");
    ensure_eq(
        at(&*e, 1),
        Some(CellByte(0x02)),
        "an uncommitted cell is 0x02",
    );

    ensure(
        e.evict_cell_cas(0, EvictStep::Begin),
        "Begin wins a durable present cell",
    );
    ensure_eq(
        at(&*e, 0),
        Some(CellByte(0x84)),
        "evicting keeps the durable bit",
    );
    ensure(
        !e.evict_cell_cas(0, EvictStep::Begin),
        "Begin does not win twice",
    );
    ensure(
        e.evict_cell_cas(0, EvictStep::Abort),
        "Abort wins an evicting cell",
    );
    ensure_eq(
        at(&*e, 0),
        Some(CellByte(0x82)),
        "an aborted eviction is as durable as before",
    );
    ensure(
        e.evict_cell_cas(1, EvictStep::Begin),
        "Begin wins a present cell",
    );
    ensure_eq(at(&*e, 1), Some(CellByte(0x04)), "evicting");
    ensure(e.evict_cell_cas(1, EvictStep::Abort), "Abort wins");
    ensure_eq(
        at(&*e, 1),
        Some(CellByte(0x02)),
        "an aborted non-durable eviction",
    );
    ensure(
        !e.evict_cell_cas(1, EvictStep::Abort),
        "Abort needs evicting",
    );
    ensure(
        !e.evict_cell_cas(1, EvictStep::Finish),
        "Finish needs evicting",
    );

    for step in [EvictStep::Begin, EvictStep::Abort, EvictStep::Finish] {
        ensure(
            !e.evict_cell_cas(2, step),
            format_args!("{step:?} on a claimed cell"),
        );
        ensure(
            !e.evict_cell_cas(3, step),
            format_args!("{step:?} on an absent cell"),
        );
        ensure(
            !e.evict_cell_cas(9, step),
            format_args!("{step:?} outside the pack"),
        );
    }
    ensure_eq(
        state_of(&*e, 2),
        Some(CellState::Claimed),
        "the evictor never changes a claimed cell",
    );
    ensure_eq(
        state_of(&*e, 3),
        Some(CellState::Absent),
        "the evictor never changes an absent cell",
    );
    drop(claimed);

    ensure(
        e.evict_cell_cas(0, EvictStep::Begin),
        "Begin before the punch",
    );
    punch_as_able(f, &*e, &[run(0, 1)], "punch of cell 0");
    ensure_eq(
        state_of(&*e, 0),
        Some(CellState::Evicting),
        "a punch does not change the state",
    );
    ensure(
        e.evict_cell_cas(0, EvictStep::Finish),
        "Finish wins after the punch",
    );
    ensure_eq(
        at(&*e, 0),
        cb(CellState::Absent, false),
        "Finish leaves absent, not durable",
    );
    ensure(!e.evict_cell_cas(0, EvictStep::Finish), "Finish wins once");
}

/// `punch` refuses a run with a cell that is not evicting (`NotEvicting`) and one with a cell
/// outside the pack (`CellOutOfRange`), each with no effect: the bytes of the evicting cells in
/// the call are still there after an abort. Empty runs report zeros.
pub fn punch_refuses_wrong_cells_with_no_effect(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 3 * MIB);
    publish(&*e, 0, 4);
    publish(&*e, 1, 5);
    ensure(e.evict_cell_cas(0, EvictStep::Begin), "Begin of cell 0");

    expect_err(
        e.punch(&[run(0, 2)]),
        |err| matches!(err, EntryError::NotEvicting { cell: 1, .. }),
        "a punch that includes a present cell",
    );
    expect_err(
        e.punch(&[run(2, 1)]),
        |err| matches!(err, EntryError::NotEvicting { cell: 2, .. }),
        "a punch of an absent cell",
    );
    expect_err(
        e.punch(&[run(0, 1), run(7, 1)]),
        |err| {
            matches!(
                err,
                EntryError::CellOutOfRange {
                    cell: 7,
                    cells: 3,
                    ..
                }
            )
        },
        "a punch that includes a cell past the pack",
    );
    ensure_eq(
        state_of(&*e, 1),
        Some(CellState::Present),
        "a refused punch changes no state",
    );
    ensure(e.evict_cell_cas(0, EvictStep::Abort), "Abort of cell 0");
    ensure_eq(
        expect_ok(e.read_cell(0), "read of cell 0 after refused punches"),
        bytes(&*e, 0, 4),
        "a refused punch released no bytes",
    );
    ensure_eq(
        expect_ok(e.read_cell(1), "read of cell 1 after refused punches"),
        bytes(&*e, 1, 5),
        "a refused punch released no bytes of a present cell",
    );

    ensure_eq(
        expect_ok(e.punch(&[]), "punch of no runs"),
        PunchReport::default(),
        "no runs report zeros",
    );
    ensure_eq(
        expect_ok(e.punch(&[run(0, 0), run(5, 0)]), "punch of empty runs"),
        PunchReport::default(),
        "empty runs report zeros, even past the pack",
    );
}

/// `punch` of evicting runs journals one record per non-empty run and syncs the journal once
/// for the call, reports the bytes released (the short last cell by its length), changes no
/// state, and after `Finish` the cells are absent, not readable and claimable again.
///
/// A store whose fixture says it cannot punch ([`EntryStoreFixture::punches`] is `false`: a
/// volume that cannot release a run inside a file, 5.6 (b)) is held to the refusal instead: the
/// batch is [`EntryError::Io`] of kind [`std::io::ErrorKind::Unsupported`] with no effect, so the
/// cells stay `evicting` with their bytes and the evictor aborts them back to `present`.
pub fn punch_releases_runs_with_one_journal_sync_per_call(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 4 * MIB + 7);
    for c in 0..5 {
        publish(&*e, c, 6);
    }
    for c in [0, 1, 4] {
        ensure(
            e.evict_cell_cas(c, EvictStep::Begin),
            format_args!("Begin of cell {c}"),
        );
    }
    if !f.punches() {
        expect_err(
            e.punch(&[run(0, 2), run(2, 0), run(4, 1)]),
            unsupported,
            "a punch on a store that cannot punch",
        );
        ensure_eq(
            e.counts().evicting,
            3,
            "a refused punch leaves the cells evicting",
        );
        for c in [0, 1, 4] {
            ensure(
                e.evict_cell_cas(c, EvictStep::Abort),
                format_args!("Abort of cell {c}"),
            );
            ensure_eq(
                expect_ok(e.read_cell(c), "read of a cell whose punch was refused"),
                bytes(&*e, c, 6),
                format_args!("cell {c} keeps its bytes"),
            );
        }
        return;
    }
    let report = expect_ok(
        e.punch(&[run(0, 2), run(2, 0), run(4, 1)]),
        "punch of two runs",
    );
    ensure_eq(
        report,
        PunchReport {
            journal_syncs: 1,
            records: 2,
            bytes_punched: 2 * MIB + 7,
        },
        "two runs: two records, one sync, the bytes of three cells",
    );
    ensure_eq(e.counts().evicting, 3, "a punch leaves the cells evicting");
    ensure_eq(e.counts().present, 2, "a punch leaves other cells alone");
    for c in [0, 1, 4] {
        ensure(
            e.evict_cell_cas(c, EvictStep::Finish),
            format_args!("Finish of cell {c}"),
        );
        ensure_eq(
            state_of(&*e, c),
            Some(CellState::Absent),
            format_args!("cell {c} after Finish"),
        );
        expect_err(
            e.read_cell(c),
            |err| matches!(err, EntryError::NotReadable { .. }),
            "a punched cell is not readable",
        );
    }
    ensure_eq(
        expect_ok(e.read_cell(2), "read of an unpunched cell"),
        bytes(&*e, 2, 6),
        "an unpunched cell keeps its bytes",
    );
    ensure_eq(
        cells(&e.claim(run(0, 5))),
        vec![0, 1, 4],
        "punched cells are claimable again",
    );
}

/// `commit_batch` makes only the present cells written since the last commit durable: one data
/// sync, one record per run of cells, nothing for an idle commit, nothing for cells that are
/// claimed or were taken by the evictor, and a second commit repeats nothing.
pub fn commit_batch_makes_written_present_cells_durable_once(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 7 * MIB);
    ensure_eq(
        expect_ok(e.commit_batch(), "commit with nothing written"),
        CommitReport::default(),
        "an idle commit does nothing",
    );
    for c in [0, 1, 3] {
        publish(&*e, c, 1);
    }
    let claimed = e.claim(run(4, 1));
    publish(&*e, 5, 2);
    ensure(e.evict_cell_cas(5, EvictStep::Begin), "Begin of cell 5");
    ensure_eq(
        expect_ok(e.commit_batch(), "commit of three cells"),
        CommitReport {
            data_syncs: 1,
            cells_durable: 3,
            records: 2,
        },
        "cells 0, 1 and 3: one sync, two runs",
    );
    for c in [0, 1, 3] {
        ensure_eq(
            at(&*e, c),
            cb(CellState::Present, true),
            format_args!("cell {c} is durable"),
        );
    }
    ensure_eq(
        state_of(&*e, 4),
        Some(CellState::Claimed),
        "a claimed cell is not committed",
    );
    ensure_eq(
        state_of(&*e, 5),
        Some(CellState::Evicting),
        "an evicting cell is not committed",
    );
    ensure(
        at(&*e, 4).is_some_and(|b| !b.durable()),
        "a claimed cell is not durable",
    );
    ensure_eq(
        expect_ok(e.commit_batch(), "a second commit"),
        CommitReport::default(),
        "a second commit has nothing to do",
    );
    publish(&*e, 2, 3);
    ensure_eq(
        expect_ok(e.commit_batch(), "commit of one cell"),
        CommitReport {
            data_syncs: 1,
            cells_durable: 1,
            records: 1,
        },
        "only the cell written since the last commit",
    );
    drop(claimed);
}

/// `counts` sorts the cells by state and `used` is present + claimed + recheck, so absent, bad
/// and evicting cells are not use.
pub fn counts_sort_cells_by_state(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 8 * MIB);
    publish(&*e, 0, 1);
    publish(&*e, 1, 1);
    ensure(e.evict_cell_cas(1, EvictStep::Begin), "Begin of cell 1");
    let claimed = e.claim(run(2, 2));
    for token in e.claim(run(4, 1)) {
        e.mark_bad(token);
    }
    f.restore_as_recheck(&*store, &pack("p"), 5, &bytes(&*e, 5, 2));
    let counts = e.counts();
    ensure_eq(
        counts,
        CellCounts {
            absent: 2,
            claimed: 2,
            present: 1,
            recheck: 1,
            evicting: 1,
            bad: 1,
        },
        "one cell of each state, two claimed and two absent",
    );
    ensure_eq(counts.used(), 4, "used is present + claimed + recheck");
    drop(claimed);
}

/// `touch` raises `last_use` to the newest `now_ms` and sets the group's epoch to the epoch of
/// that last use (also for an older clock); an untouched group stays 0; a group outside the
/// pack changes `last_use` only and has no epoch.
pub fn touch_records_last_use_and_group_epochs(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 40 * MIB);
    ensure_eq(e.touch_epoch(0), Some(0), "an untouched group has epoch 0");
    ensure_eq(e.touch_epoch(3), None, "a pack of 40 cells has 3 groups");
    e.touch(5_000, 1);
    ensure_eq(e.last_use(), 5_000, "last_use after a touch");
    ensure_eq(
        e.touch_epoch(1),
        Some(5),
        "the touched group's epoch is its seconds",
    );
    ensure_eq(e.touch_epoch(0), Some(0), "another group is not touched");
    e.touch(3_000, 2);
    ensure_eq(e.last_use(), 5_000, "last_use never goes back");
    ensure_eq(
        e.touch_epoch(2),
        Some(epoch_of(5_000)),
        "an older clock records the epoch of last_use",
    );
    e.touch(9_999, 0);
    ensure_eq(e.touch_epoch(0), Some(9), "epochs truncate to seconds");
    e.touch(12_000, 77);
    ensure_eq(
        e.last_use(),
        12_000,
        "a group outside the pack still moves last_use",
    );
    ensure_eq(
        e.touch_epoch(77),
        None,
        "a group outside the pack has no epoch",
    );
    ensure_eq(
        (e.touch_epoch(0), e.touch_epoch(1), e.touch_epoch(2)),
        (Some(9), Some(5), Some(5)),
        "a touch outside the pack changes no epoch",
    );
    let infos = store.entries();
    ensure_eq(
        infos.iter().map(|i| i.last_use).collect::<Vec<_>>(),
        vec![12_000],
        "entries() reports the last use",
    );
}

/// `read_cell` refuses absent and claimed cells with the byte seen, and a cell outside the pack
/// with `CellOutOfRange`.
pub fn read_cell_refuses_unreadable_cells(f: &dyn EntryStoreFixture) {
    let store = f.store();
    let e = open(&*store, "p", 2 * MIB);
    let claimed = e.claim(run(1, 1));
    expect_err(
        e.read_cell(0),
        |err| {
            matches!(
                err,
                EntryError::NotReadable {
                    cell: 0,
                    state: CellByte(0x00),
                    ..
                }
            )
        },
        "an absent cell is not readable",
    );
    expect_err(
        e.read_cell(1),
        |err| {
            matches!(
                err,
                EntryError::NotReadable {
                    cell: 1,
                    state: CellByte(0x01),
                    ..
                }
            )
        },
        "a claimed cell is not readable",
    );
    expect_err(
        e.read_cell(2),
        |err| {
            matches!(
                err,
                EntryError::CellOutOfRange {
                    cell: 2,
                    cells: 2,
                    ..
                }
            )
        },
        "a cell past the pack is out of range",
    );
    drop(claimed);
}
