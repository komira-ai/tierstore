//! The edges of `Engine::ensure` and `Engine::wait` and of the engine's own surface: ranges that
//! are empty, start past the pack or end past it, a pack of no bytes, a cell the evictor owns, a
//! publish that fails for one cell, a fill nobody wants, the counters of a finished fill, the
//! signals the routing reads, and the `Debug` and `Drop` of the engine.
//!
//! Catches: an empty range that starts a fill; a range past the pack that waits forever or is
//! reported ready; a range that runs past the end of the pack and is not clipped; a wait that
//! spins on or fetches a cell the evictor owns, or never sees it come back; an evicted cell that
//! is not fetched again; a failed publish that fails the cells around it or leaves its waiter
//! hanging; a fill with nothing wanted that stays in the table; counters that disagree with the
//! bytes of the pack; signals that do not reach the copy choice; a `Debug` that prints the
//! engine's insides; class loops that outlive the engine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use support::{
    MIB, Pack, Rig, Script, Sha256Verifier, TestBudget, TestUpstream, generation, hot, location,
};
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineParts, GetKind, TimerKind};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::entry::{ClaimedCell, Entry, EvictStep};
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::ids::{CellRun, Class, JobId, Rank};
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, DriveState};
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

const JOB: JobId = JobId(1);

fn node(n: u8) -> DeviceRef {
    DeviceRef::Hot(NodeId(n))
}

/// A rig with a pack of `size` bytes held by node 1 under `key`.
fn rig_with(key: &str, size: u64) -> (Rig, Pack) {
    let rig = Rig::default_rig();
    let pack = Pack::new(key, size, 3);
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size)]),
    );
    (rig, pack)
}

fn all_cells(pack: &Pack) -> CellRun {
    CellRun {
        first: 0,
        count: u32::try_from(pack.spec.size.div_ceil(MIB)).expect("cells fit"),
    }
}

/// The devices of the GETs, in order.
fn get_devices(rig: &Rig) -> Vec<DeviceRef> {
    rig.upstream
        .gets()
        .iter()
        .map(|g| g.source.device)
        .collect()
}

/// A wait on an empty range is ready at once: no fill starts, nothing is located or fetched.
#[tokio::test]
async fn a_wait_on_an_empty_range_is_ready_without_a_fill() {
    let (rig, pack) = rig_with("empty-range", 3 * MIB);
    let outcome = rig.engine.wait(&pack.spec, pack.range(MIB, 0), JOB).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!((stats.fills, stats.locates, stats.total_gets()), (0, 0, 0));
}

/// A wait on a range that starts at or past the end of the pack is an error naming the cell the
/// range starts in, and it neither starts a fill nor fetches.
#[tokio::test]
async fn a_wait_on_a_range_starting_past_the_pack_is_eio_on_its_first_cell() {
    let (rig, pack) = rig_with("past-the-pack", 3 * MIB);
    let at_end = rig
        .engine
        .wait(&pack.spec, pack.range(3 * MIB, 1), JOB)
        .await;
    let beyond = rig
        .engine
        .wait(&pack.spec, pack.range(7 * MIB + 5, MIB), JOB)
        .await;
    assert_eq!(at_end, WaitOutcome::Eio { cell: 3 });
    assert_eq!(beyond, WaitOutcome::Eio { cell: 7 });
    let stats = rig.engine.stats();
    assert_eq!((stats.fills, stats.total_gets()), (0, 0));
}

/// A wait on a range that runs past the end of the pack is clipped to the pack: it is ready when
/// the cells inside the pack are, and only those cells are fetched.
#[tokio::test]
async fn a_wait_on_a_range_ending_past_the_pack_is_clipped_to_the_pack() {
    let (rig, pack) = rig_with("clipped", 3 * MIB);
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(2 * MIB, 10 * MIB), JOB)
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(rig.get_ranges(&pack), vec![(2 * MIB, MIB)]);
    assert_eq!(
        rig.cells(&pack),
        vec![CellState::Absent, CellState::Absent, CellState::Present]
    );
    assert_eq!(
        rig.entry_bytes(&pack, 2 * MIB, MIB),
        pack.bytes.slice(2 * MIB as usize..).to_vec()
    );
}

/// A pack of no bytes has no cell: an empty wait is ready, a wait for byte 0 is an error on cell
/// 0, and asking for a cell of it fetches nothing.
#[tokio::test]
async fn a_pack_of_no_bytes_has_no_cell_to_wait_for_or_fetch() {
    let (rig, pack) = rig_with("no-bytes", 0);
    let empty = rig.engine.wait(&pack.spec, pack.range(0, 0), JOB).await;
    let first = rig.engine.wait(&pack.spec, pack.range(0, 1), JOB).await;
    rig.engine.ensure(
        &pack.spec,
        &[CellRun { first: 0, count: 1 }],
        Class::Prepare,
        JOB,
        Rank(0),
    );
    rig.until(|s| s.locates == 1).await;
    assert_eq!(empty, WaitOutcome::Ready);
    assert_eq!(first, WaitOutcome::Eio { cell: 0 });
    assert_eq!(rig.engine.stats().total_gets(), 0);
}

/// A cell the evictor owns is not fetched: the waiter polls (one poll timer armed), and when the
/// evictor aborts, the cell is present again and the wait is ready after the poll interval, with
/// no second GET.
#[tokio::test]
async fn a_wait_on_an_evicting_cell_polls_and_is_ready_after_the_eviction_aborts() {
    let (rig, pack) = rig_with("evict-abort", 3 * MIB);
    assert_eq!(
        rig.engine.wait(&pack.spec, pack.all(), JOB).await,
        WaitOutcome::Ready
    );
    let entry = rig.store.mem_entry(&pack.spec.id).expect("entry");
    assert!(entry.evict_cell_cas(1, EvictStep::Begin));
    let wait = rig.engine.wait(&pack.spec, pack.range(MIB, MIB), JOB);
    let evictor = async {
        let stats = rig.until(|s| s.armed(TimerKind::EvictPoll) == 1).await;
        assert_eq!(stats.total_gets(), 1);
        assert!(entry.evict_cell_cas(1, EvictStep::Abort));
        rig.clock.advance(50);
    };
    let (outcome, ()) = tokio::join!(wait, evictor);
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(rig.engine.stats().total_gets(), 1);
    assert_eq!(rig.get_ranges(&pack), vec![(0, 3 * MIB)]);
}

/// A cell the evictor finishes with is absent again: the waiter that was polling it fetches that
/// one cell again and is ready.
#[tokio::test]
async fn a_wait_on_a_cell_whose_eviction_finishes_fetches_the_cell_again() {
    let (rig, pack) = rig_with("evict-finish", 3 * MIB);
    assert_eq!(
        rig.engine.wait(&pack.spec, pack.all(), JOB).await,
        WaitOutcome::Ready
    );
    let entry = rig.store.mem_entry(&pack.spec.id).expect("entry");
    assert!(entry.evict_cell_cas(1, EvictStep::Begin));
    let wait = rig.engine.wait(&pack.spec, pack.range(MIB, MIB), JOB);
    let evictor = async {
        rig.until(|s| s.armed(TimerKind::EvictPoll) == 1).await;
        assert!(entry.evict_cell_cas(1, EvictStep::Finish));
        rig.clock.advance(50);
    };
    let (outcome, ()) = tokio::join!(wait, evictor);
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(rig.get_ranges(&pack), vec![(0, 3 * MIB), (MIB, MIB)]);
    assert_eq!(rig.cells(&pack), vec![CellState::Present; 3]);
}

/// The `Debug` of the engine and of its parts print their names only, none of the state behind.
#[tokio::test]
async fn the_debug_of_the_engine_and_its_parts_prints_only_their_names() {
    let (rig, _pack) = rig_with("debug", MIB);
    let parts = EngineParts {
        store: Arc::new(MemEntryStore::new()),
        upstream: Arc::new(TestUpstream::new()),
        budget: Arc::new(TestBudget::new(1)),
        verifier: Arc::new(Sha256Verifier),
        timer: Arc::new(ManualTimer::new(ManualClock::new())),
    };
    assert_eq!(format!("{:?}", rig.engine), "FetchEngine { .. }");
    assert_eq!(format!("{parts:?}"), "EngineParts { .. }");
}

/// Dropping the engine stops its class loops: the engine's hold on its parts is released, and
/// the runtime goes on without a panic.
#[tokio::test]
async fn dropping_the_engine_stops_its_loops_and_releases_its_parts() {
    let (rig, pack) = rig_with("drop", 2 * MIB);
    assert_eq!(
        rig.engine.wait(&pack.spec, pack.all(), JOB).await,
        WaitOutcome::Ready
    );
    let Rig { engine, budget, .. } = rig;
    assert_eq!(Arc::strong_count(&budget), 2);
    drop(engine);
    tokio::task::yield_now().await;
    assert_eq!(Arc::strong_count(&budget), 1);
}

/// After a successful fill the counters say what happened: every byte of the pack (the last cell
/// short) fetched and published once, nothing wasted, nothing failed.
#[tokio::test]
async fn the_counters_of_a_finished_fill_agree_with_the_size_of_the_pack() {
    let size = 3 * MIB + 100;
    let (rig, pack) = rig_with("counters", size);
    assert_eq!(
        rig.engine.wait(&pack.spec, pack.all(), JOB).await,
        WaitOutcome::Ready
    );
    let stats = rig.until(|s| s.cells_published == 4).await;
    assert_eq!(stats.bytes_fetched, size);
    assert_eq!(stats.bytes_published, size);
    assert_eq!(stats.cells_published, 4);
    assert_eq!(stats.bytes_wasted, 0);
    assert_eq!(
        (
            stats.fills,
            stats.locates,
            stats.fills_failed,
            stats.packs_lost
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(stats.gets(GetKind::GridPart), 1);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// What `observe_signals` says about a device reaches the copy choice: a copy reported `down` is
/// passed over for the other, and chosen again once it reports `up`.
#[tokio::test]
async fn observed_signals_change_which_copy_serves_a_part() {
    let rig = Rig::default_rig();
    let first = Pack::new("signals-a", MIB, 4);
    let second = Pack::new("signals-b", MIB, 5);
    for pack in [&first, &second] {
        let size = pack.spec.size;
        rig.upstream.add(
            pack,
            location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
        );
    }
    let state = |s| DriveSignals {
        drive_state: Some(s),
        ..DriveSignals::default()
    };
    rig.engine.observe_signals(node(1), state(DriveState::Down));
    assert_eq!(
        rig.engine.wait(&first.spec, first.all(), JOB).await,
        WaitOutcome::Ready
    );
    rig.engine.observe_signals(node(1), state(DriveState::Up));
    assert_eq!(
        rig.engine.wait(&second.spec, second.all(), JOB).await,
        WaitOutcome::Ready
    );
    assert_eq!(get_devices(&rig), vec![node(2), node(1)]);
}

/// A publish that fails for one cell (its claim was taken from under the op) fails that cell for
/// its waiter with EIO; the cells around it are published and their waiters are ready.
#[tokio::test]
async fn a_failed_publish_fails_its_cell_and_leaves_the_others_ready() {
    let (rig, pack) = rig_with("publish-fails", 3 * MIB);
    let gate = Arc::new(Semaphore::new(0));
    rig.upstream.script(node(1), Script::Gated(gate.clone()));
    rig.engine.ensure(
        &pack.spec,
        &[all_cells(&pack)],
        Class::Prepare,
        JOB,
        Rank(0),
    );
    let cell = |c: u64| rig.engine.wait(&pack.spec, pack.range(c * MIB, MIB), JOB);
    let saboteur = async {
        rig.until(|s| s.total_gets() == 1).await;
        let entry = rig.store.mem_entry(&pack.spec.id).expect("entry");
        entry.abandon(ClaimedCell::won(1));
        gate.add_permits(1);
    };
    let (first, second, third, ()) = tokio::join!(cell(0), cell(1), cell(2), saboteur);
    assert_eq!(first, WaitOutcome::Ready);
    assert_eq!(second, WaitOutcome::Eio { cell: 1 });
    assert_eq!(third, WaitOutcome::Ready);
    let stats = rig.until(|s| s.cells_published == 2).await;
    assert_eq!(stats.bytes_wasted, MIB);
    assert_eq!(
        rig.cells(&pack),
        vec![CellState::Present, CellState::Absent, CellState::Present]
    );
}

/// A fill with nothing wanted (an `ensure` with no runs) and no waiter is forgotten once it has
/// located the pack: it fetches nothing, and a later wait starts a new fill.
#[tokio::test]
async fn a_fill_with_nothing_wanted_is_forgotten_and_a_later_wait_starts_a_new_one() {
    let (rig, pack) = rig_with("forgotten", 2 * MIB);
    rig.engine
        .ensure(&pack.spec, &[], Class::Background, JOB, Rank(0));
    let stats = rig.until(|s| s.locates == 1).await;
    assert_eq!((stats.fills, stats.total_gets()), (1, 0));
    assert_eq!(
        rig.engine.wait(&pack.spec, pack.all(), JOB).await,
        WaitOutcome::Ready
    );
    let stats = rig.engine.stats();
    assert_eq!((stats.fills, stats.locates), (2, 2));
    assert_eq!(stats.gets(GetKind::GridPart), 1);
}
