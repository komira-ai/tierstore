//! `evict(target)` (section 7, 5.1, 8): whole unheld entries by LRU, then cell-group eviction of
//! held entries, the reader wait and its timeout, one journaled punch per entry, and the grace
//! window.
//!
//! Section 14 rows: "Live reader stuck beyond `evict_wait`: won cells restored `present`, nothing
//! journaled, `evict_timeouts_total` increments"; "Crash after a batched PUNCH fsync and
//! before/after the punch: replay never marks a punched cell `present`; one fsync per batch"
//! (driven by the evictor); "No eviction before the grace window"; the eviction order of 7.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io::ErrorKind;
use std::sync::Arc;

use support::*;
use tierstore_evict::{EvictConfig, EvictError, EvictFailure, Evictor, EvictorLock};
use tierstore_fetch_api::cell::CellState::{Absent, Bad, Claimed, Evicting, Present, Recheck};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::{Entry, EntryError, ReaderCounters, Settle};
use tierstore_fetch_api::ids::{CellRun, HolderId};
use tierstore_journal::FileKind;
use tierstore_journal::fake::{Crash, FsEntry, Keep, Op};

const HOLDER: HolderId = HolderId(7);

/// Three unheld entries of 4 used cells each, last used at 30, 10 and 20 s.
fn three_unheld(store: &TestStore) {
    for (n, used_ms) in [(1, 30_000), (2, 10_000), (3, 20_000)] {
        let e = mem(n, 4, 0..4);
        e.touch(used_ms, 0);
        store.insert(e);
    }
}

#[test]
fn unheld_entries_are_removed_least_recently_used_first_and_held_ones_kept() {
    let store = TestStore::new();
    three_unheld(&store);
    let held = mem(4, 4, 0..4);
    held.hold(HOLDER);
    store.insert(held.clone());
    let rig = Rig::plain(store.clone());

    // 16 cells used; down to 8 takes two whole entries: the two least recently used.
    let r = rig.evictor.evict(8 * MIB).unwrap();
    assert_eq!(*store.removals.lock().unwrap(), [pack(2), pack(3)]);
    assert!(store.has(1) && store.has(4));
    assert_eq!(r.entries_removed, 2);
    assert_eq!(r.removed_bytes, 8 * MIB);
    assert_eq!((r.used_before, r.used_after), (16 * MIB, 8 * MIB));
    assert_eq!((r.cells_punched, r.journal_syncs), (0, 0));
    assert_eq!(states(held.as_ref(), 0..4), [Present; 4]);
    assert_eq!(rig.metrics.evictions_total.with(()).get(), 8);
    assert_eq!(rig.metrics.holds.with(()).get(), 1.0);
    assert!(r.failures.is_empty());
}

#[test]
fn entries_used_at_the_same_time_are_removed_in_pack_order() {
    let store = TestStore::new();
    for n in [3, 1, 2] {
        store.insert(mem(n, 2, 0..2));
    }
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(2 * MIB).unwrap();
    assert_eq!(*store.removals.lock().unwrap(), [pack(1), pack(2)]);
    assert_eq!(r.used_after, 2 * MIB);
}

#[test]
fn nothing_is_evicted_when_use_is_at_the_target() {
    let store = TestStore::new();
    three_unheld(&store);
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(12 * MIB).unwrap();
    assert!(store.removals.lock().unwrap().is_empty());
    assert_eq!((r.used_before, r.used_after), (12 * MIB, 12 * MIB));
    assert_eq!(rig.metrics.evictions_total.with(()).get(), 0);
}

#[test]
fn a_held_only_cache_punches_the_groups_untouched_longest() {
    let store = TestStore::new();
    // Three groups of 16 present cells; group 1 is the coldest, then group 2.
    let e = mem(1, 48, 0..48);
    e.hold(HOLDER);
    // A touch moves the group to the epoch of the entry's newest use, so touches go in time
    // order.
    for (group, at_s) in [(1, 10), (2, 30), (0, 50)] {
        e.touch(at_s * 1000, group);
    }
    store.insert(e.clone());
    let rig = Rig::plain(store.clone());

    let r = rig.evictor.evict(32 * MIB).unwrap();
    assert!(
        store.removals.lock().unwrap().is_empty(),
        "a held entry is never removed"
    );
    assert_eq!(states(e.as_ref(), 16..32), [Absent; 16]);
    assert_eq!(states(e.as_ref(), 0..16), [Present; 16]);
    assert_eq!(states(e.as_ref(), 32..48), [Present; 16]);
    assert_eq!(r.cells_punched, 16);
    assert_eq!(r.bytes_punched, 16 * MIB);
    assert_eq!(r.journal_syncs, 1);
    assert_eq!(r.used_after, 32 * MIB);
    assert_eq!(rig.metrics.punched_bytes_total.with(()).get(), 16 * MIB);
    assert_eq!(rig.metrics.evictions_total.with(()).get(), 16);

    // One more group's worth: the next coldest, group 2.
    let r = rig.evictor.evict(17 * MIB).unwrap();
    assert_eq!(r.cells_punched, 16);
    assert_eq!(states(e.as_ref(), 32..48), [Absent; 16]);
    assert_eq!(states(e.as_ref(), 0..16), [Present; 16]);
}

#[test]
fn groups_are_chosen_across_entries_by_epoch_and_punched_once_per_entry() {
    let store = TestStore::new();
    let a = mem(1, 32, 0..32);
    let b = mem(2, 32, 0..32);
    for e in [&a, &b] {
        e.hold(HOLDER);
    }
    // Coldest first: a/1 (5 s), b/0 (6 s), a/0 (7 s), b/1 (9 s).
    a.touch(5_000, 1);
    b.touch(6_000, 0);
    a.touch(7_000, 0);
    b.touch(9_000, 1);
    store.insert(a.clone());
    store.insert(b.clone());
    let rig = Rig::plain(store.clone());

    let r = rig.evictor.evict(64 * MIB - 48 * MIB).unwrap();
    assert_eq!(states(a.as_ref(), [0, 16]), [Absent, Absent]);
    assert_eq!(states(b.as_ref(), [0, 16]), [Absent, Present]);
    assert_eq!(r.cells_punched, 48);
    // One punch, so one journal sync, per entry.
    assert_eq!(r.journal_syncs, 2);
}

#[test]
fn only_present_cells_are_evicted_and_a_claimed_cell_still_publishes() {
    let store = TestStore::new();
    let e = mem(1, 8, [0, 1]);
    e.hold(HOLDER);
    let claimed = e.claim(CellRun { first: 2, count: 1 });
    e.restore_as_recheck(3, &cell_bytes(8 * MIB, 3)).unwrap();
    let bad = e.claim(CellRun { first: 4, count: 1 });
    e.mark_bad(bad.into_iter().next().unwrap());
    store.insert(e.clone());
    let rig = Rig::plain(store.clone());

    // Use is present + claimed + recheck = 4 cells; the target 0 takes what can go.
    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(
        states(e.as_ref(), 0..6),
        [Absent, Absent, Claimed, Recheck, Bad, Absent]
    );
    assert_eq!(r.cells_punched, 2);
    assert_eq!((r.used_before, r.used_after), (4 * MIB, 2 * MIB));
    for token in claimed {
        e.publish(token, &cell_bytes(8 * MIB, 2)).unwrap();
    }
    assert_eq!(state(e.as_ref(), 2), Present);
    assert!(e.settle(3, Settle::RecheckOk));
}

#[test]
fn a_short_last_cell_is_punched_to_the_end_of_the_pack() {
    let store = TestStore::new();
    let size = 2 * MIB + 1000;
    let (_fs, e) = journaled(1, size, 0..3);
    e.hold(HOLDER);
    store.insert(e.clone());
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(r.cells_punched, 3);
    assert_eq!(r.bytes_punched, 2 * MIB + 1000);
    assert_eq!(
        rig.metrics.punched_bytes_total.with(()).get(),
        2 * MIB + 1000
    );
}

#[test]
fn a_reader_that_leaves_lets_the_punch_go_ahead() {
    let store = TestStore::new();
    let e = mem(1, 16, 0..16);
    e.hold(HOLDER);
    store.insert(e.clone());
    let clock = ManualClock::new();
    e.enter(0);
    let reader = e.clone();
    // The reader commits its reply while the evictor is in its second pause.
    let pause = ClockPause::with(&clock, 500, move |attempt| {
        if attempt == 1 {
            reader.leave(0);
        }
    });
    let rig = Rig::new(store.clone(), clock, pause.clone());

    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(*pause.attempts.lock().unwrap(), [0, 1]);
    assert_eq!(r.groups_timed_out, 0);
    assert_eq!(r.cells_punched, 16);
    assert_eq!(states(e.as_ref(), 0..16), [Absent; 16]);
}

#[test]
fn a_reader_stuck_past_evict_wait_restores_the_cells_with_nothing_journaled() {
    let store = TestStore::new();
    // Group 0 has a reader that never leaves; group 1 is clear.
    let (fs, e) = journaled(1, 32 * MIB, 0..32);
    e.hold(HOLDER);
    e.enter(0);
    store.insert(e.clone());
    let clock = ManualClock::new();
    let pause = ClockPause::new(&clock, 500);
    let rig = Rig::new(store.clone(), clock, pause.clone());
    let syncs = fs.count(Op::SyncData, Some(FileKind::Journal));

    let r = rig.evictor.evict(0).unwrap();
    // 2000 ms in pauses of 500 ms: the wait gives up when the clock reaches the deadline.
    assert_eq!(pause.count(), 4);
    assert_eq!(r.groups_timed_out, 1);
    assert_eq!(r.cells_restored, 16);
    assert_eq!(r.cells_punched, 16);
    assert_eq!(states(e.as_ref(), 0..16), [Present; 16]);
    assert_eq!(states(e.as_ref(), 16..32), [Absent; 16]);
    assert_eq!(rig.metrics.evict_timeouts_total.with(()).get(), 1);
    // One journal sync for the clear group's batch; the busy group's cells are in no record.
    assert_eq!(fs.count(Op::SyncData, Some(FileKind::Journal)), syncs + 1);
    let after = FsEntry::open(fs.crash(Crash::Process), &spec(1, 32 * MIB), Some(BOOT))
        .unwrap()
        .0;
    assert_eq!(states(&after, 0..16), [Present; 16]);
    assert_eq!(states(&after, 16..32), [Absent; 16]);
}

#[test]
fn a_reader_stuck_on_every_group_punches_nothing() {
    let store = TestStore::new();
    let (fs, e) = journaled(1, 16 * MIB, 0..16);
    e.hold(HOLDER);
    e.enter(0);
    store.insert(e.clone());
    let rig = Rig::plain(store.clone());
    let syncs = fs.count(Op::SyncData, Some(FileKind::Journal));
    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(
        (r.cells_punched, r.journal_syncs, r.cells_restored),
        (0, 0, 16)
    );
    assert_eq!(fs.count(Op::SyncData, Some(FileKind::Journal)), syncs);
    assert_eq!(fs.count(Op::PunchData, Some(FileKind::Data)), 0);
    assert_eq!(r.used_after, 16 * MIB);
}

#[test]
fn one_batch_is_one_journal_sync_and_a_crash_never_restores_a_punched_cell() {
    for how in [
        Crash::Process,
        Crash::Power(Keep::Nothing),
        Crash::Power(Keep::Everything),
    ] {
        let store = TestStore::new();
        let (fs, e) = journaled(1, 48 * MIB, 0..48);
        e.hold(HOLDER);
        // Groups 0 and 2 are cold, group 1 hot: two runs, one batch.
        e.touch(1_000, 0);
        e.touch(2_000, 2);
        e.touch(9_000, 1);
        store.insert(e.clone());
        let rig = Rig::plain(store.clone());
        let syncs = fs.count(Op::SyncData, Some(FileKind::Journal));

        let r = rig.evictor.evict(16 * MIB).unwrap();
        assert_eq!(r.journal_syncs, 1, "{how:?}");
        assert_eq!(fs.count(Op::SyncData, Some(FileKind::Journal)), syncs + 1);
        assert_eq!(fs.count(Op::PunchData, Some(FileKind::Data)), 2);

        let (after, rec) = FsEntry::open(fs.crash(how), &spec(1, 48 * MIB), Some(BOOT)).unwrap();
        assert_eq!(rec.count(Present) + rec.count(Recheck), 16, "{how:?}");
        assert!(
            states(&after, (0..16).chain(32..48))
                .iter()
                .all(|s| *s == Absent)
        );
    }
}

#[test]
fn a_failed_punch_is_reported_and_its_cells_are_absent_not_servable() {
    let store = TestStore::new();
    let (fs, e) = journaled(1, 16 * MIB, 0..16);
    e.hold(HOLDER);
    store.insert(e.clone());
    fs.fail(Op::PunchData, Some(FileKind::Data), 0, ErrorKind::Other);
    let rig = Rig::plain(store.clone());

    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(r.failures.len(), 1);
    assert!(matches!(
        &r.failures[0],
        EvictFailure::Punch { pack: p, source: EntryError::Io { .. } } if *p == pack(1)
    ));
    assert_eq!(states(e.as_ref(), 0..16), [Absent; 16]);
    assert_eq!(r.cells_punched, 16);
    // The records were synced before the failed punch, so a restart restores none of them.
    let after = FsEntry::open(fs.crash(Crash::Process), &spec(1, 16 * MIB), Some(BOOT))
        .unwrap()
        .0;
    assert_eq!(states(&after, 0..16), [Absent; 16]);
}

#[test]
fn a_failed_removal_is_reported_and_the_entry_is_then_punched() {
    let store = TestStore::new();
    let e = mem(1, 4, 0..4);
    store.insert(e.clone());
    store.fail_remove.lock().unwrap().insert(pack(1));
    let rig = Rig::plain(store.clone());

    let r = rig.evictor.evict(0).unwrap();
    assert!(store.has(1));
    assert_eq!(r.entries_removed, 0);
    assert!(matches!(
        &r.failures[..],
        [EvictFailure::Remove { pack: p, source: EntryError::Io { .. } }] if *p == pack(1)
    ));
    // Still over the target, so its cells go by the protocol.
    assert_eq!(r.cells_punched, 4);
    assert_eq!(r.used_after, 0);
}

#[test]
fn an_entry_held_between_the_listing_and_its_removal_is_kept() {
    let store = TestStore::new();
    store.insert(mem(1, 4, 0..4));
    store.insert(mem(2, 4, 0..4));
    store.held_at_remove.lock().unwrap().insert(pack(1));
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(4 * MIB).unwrap();
    assert_eq!(*store.removals.lock().unwrap(), [pack(1), pack(2)]);
    assert!(store.has(1) && !store.has(2));
    assert_eq!(r.entries_removed, 1);
    assert_eq!(r.used_after, 4 * MIB);
}

#[test]
fn an_entry_gone_between_the_listing_and_its_lookup_is_skipped() {
    let store = TestStore::new();
    let a = mem(1, 4, 0..4);
    let b = mem(2, 4, 0..4);
    for e in [&a, &b] {
        e.hold(HOLDER);
        store.insert(e.clone());
    }
    store.gone_at_get.lock().unwrap().insert(pack(1));
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(states(a.as_ref(), 0..4), [Present; 4]);
    assert_eq!(states(b.as_ref(), 0..4), [Absent; 4]);
    assert_eq!(r.cells_punched, 4);
    // What could not be reached stays in the use left.
    assert_eq!(r.used_after, 4 * MIB);
}

#[test]
fn an_empty_cache_and_cells_already_evicting_are_left_alone() {
    let store = TestStore::new();
    let rig = Rig::plain(store.clone());
    let r = rig.evictor.evict(0).unwrap();
    assert_eq!((r.used_before, r.used_after, r.cells_punched), (0, 0, 0));

    // A held entry whose only present cell another evictor run left `evicting` is not counted
    // in use and not touched.
    let e = mem(1, 2, 0..2);
    e.hold(HOLDER);
    assert!(e.evict_cell_cas(0, tierstore_fetch_api::entry::EvictStep::Begin));
    store.insert(e.clone());
    let r = rig.evictor.evict(0).unwrap();
    assert_eq!(states(e.as_ref(), 0..2), [Evicting, Absent]);
    assert_eq!(r.cells_punched, 1);
}

#[test]
fn nothing_is_evicted_before_the_grace_window_has_passed() {
    let store = TestStore::new();
    three_unheld(&store);
    let dir = scratch("grace");
    let clock = ManualClock::new();
    clock.advance(5_000);
    let metrics = metrics();
    let evictor = Evictor::new(
        store.clone(),
        EvictorLock::acquire(dir.path()).unwrap(),
        Arc::new(clock.clone()),
        ClockPause::new(&clock, 500),
        EvictConfig::new(64 * MIB),
        metrics.clone(),
    );
    // The window runs from the evictor's making at 5 s, not from the clock's origin.
    clock.advance(EvictConfig::GRACE_MS - 1);
    assert_eq!(
        evictor.evict(0).unwrap_err(),
        EvictError::Grace {
            now_ms: 64_999,
            until_ms: 65_000
        }
    );
    assert!(store.removals.lock().unwrap().is_empty());
    assert_eq!(metrics.evictions_total.with(()).get(), 0);

    clock.advance(1);
    let r = evictor.evict(0).unwrap();
    assert_eq!(r.entries_removed, 3);
    assert_eq!(evictor.config().grace_ms, 60_000);
}

#[test]
fn the_reports_and_errors_say_what_happened() {
    let grace = EvictError::Grace {
        now_ms: 10,
        until_ms: 60_000,
    };
    assert_eq!(
        grace.to_string(),
        "eviction waits for the grace window: 10 ms of 60000 ms"
    );
    let io = || EntryError::Io {
        pack: pack(1),
        op: "punch",
        source: std::io::Error::other("disk"),
    };
    let remove = EvictFailure::Remove {
        pack: pack(1),
        source: io(),
    };
    let punch = EvictFailure::Punch {
        pack: pack(1),
        source: io(),
    };
    let p = format!("{:?}", pack(1));
    assert_eq!(
        remove.to_string(),
        format!("removing {p}: {p}: punch: disk")
    );
    assert_eq!(
        punch.to_string(),
        format!("punching cells of {p}: {p}: punch: disk")
    );
    assert!(std::error::Error::source(&punch).is_some());

    let rig = Rig::plain(TestStore::new());
    let shown = format!("{:?}", rig.evictor);
    assert!(shown.starts_with("Evictor { config: EvictConfig { budget_bytes: 67108864"));
    assert!(shown.contains("grace_until_ms: 60000"));
}
