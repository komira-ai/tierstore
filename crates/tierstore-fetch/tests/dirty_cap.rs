//! The memory bounds of 4.8: the dirty-byte cap a cell waits on before its publish, with the
//! `dirty_bytes` gauge and `writeback_stall_seconds`, and the pool of 1 MiB cell buffers parts
//! are received into. Publishes are held on a gate of the test's entry store, as a slow `pwrite`
//! would hold them; nothing waits on time.
//!
//! Catches: a cell handed to its publish past the cap, or a cap that stops at fewer cells than it
//! holds; a gauge that does not follow the cells in flight or does not come back to zero; a wait
//! that records no stall; a cap that is not `min(256 MiB, 2 x ceiling)`; a failed publish that
//! keeps its share; a hedge's loser counted as dirty; more cell buffers in use than the pool has;
//! a demand cell admitted after prepare cells that waited before it; a cap below one cell that
//! admits nothing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use support::gated::{GatedStore, PublishGate};
use support::{MIB, Pack, Rig, Script, generation, hot, location};
use tierstore_fetch::config::{
    DEFAULT_CEILING_BYTES, DIRTY_CAP_MAX_BYTES, buffer_bytes_for, dirty_cap_for,
};
use tierstore_fetch::{EngineConfig, GetKind, TimerKind};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::ids::{CellRun, Class, JobId, Rank};
use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

const JOB: JobId = JobId(1);

/// An engine whose publishes wait on the returned gate, with `config`.
fn gated(config: EngineConfig) -> (Rig, Arc<PublishGate>) {
    let mem = Arc::new(MemEntryStore::new());
    let store = Arc::new(GatedStore::new(mem.clone()));
    let gate = store.gate.clone();
    (Rig::with_store(64, config, store, mem), gate)
}

/// The default configuration with a dirty-byte cap of `bytes`.
fn capped(bytes: u64) -> EngineConfig {
    EngineConfig {
        dirty_cap_bytes: bytes,
        ..EngineConfig::default()
    }
}

/// A pack of `size` bytes under `key`, held by hot node 1 of `rig`.
fn pack_on(rig: &Rig, key: &str, size: u64, seed: u8) -> Pack {
    let pack = Pack::new(key, size, seed);
    rig.upstream
        .add(&pack, location(generation(1), size, vec![hot(1, size)]));
    pack
}

fn all_cells(pack: &Pack) -> [CellRun; 1] {
    [CellRun {
        first: 0,
        count: u32::try_from(pack.spec.size.div_ceil(MIB)).expect("cells fit"),
    }]
}

fn prepare(rig: &Rig, pack: &Pack) {
    rig.engine
        .ensure(&pack.spec, &all_cells(pack), Class::Prepare, JOB, Rank(0));
}

fn present(rig: &Rig, pack: &Pack) -> usize {
    rig.cells(pack)
        .iter()
        .filter(|c| **c == CellState::Present)
        .count()
}

/// With a dirty cap of 4 cells and publishes held, exactly 4 of 8 verified cells are handed to
/// their publish and the gauge reads 4 MiB; the other 4 are not handed over. Once the publishes
/// return, all 8 are present, the gauge is back at 0 and each wait was a stall sample.
#[tokio::test]
async fn cells_wait_before_publish_when_dirty_bytes_reach_the_cap() {
    let (rig, gate) = gated(capped(4 * MIB));
    let pack = pack_on(&rig, "dirty/eight", 8 * MIB, 3);
    prepare(&rig, &pack);
    gate.until(|p| p.in_flight == 4).await;
    let stats = rig.until(|s| s.dirty_waiting >= 1).await;
    let held = gate.publishes();
    assert_eq!(
        (held.started.len(), held.in_flight, held.returned),
        (4, 4, 0)
    );
    assert_eq!(stats.dirty_bytes, 4 * MIB);
    assert_eq!(rig.dirty_gauge(), (4 * MIB) as f64);
    assert_eq!(stats.cells_published, 0);
    assert_eq!(present(&rig, &pack), 0);

    gate.open();
    let stats = rig
        .until(|s| s.cells_published == 8 && s.dirty_bytes == 0)
        .await;
    assert_eq!(rig.entry_bytes(&pack, 0, 8 * MIB), pack.bytes.to_vec());
    assert_eq!(present(&rig, &pack), 8);
    assert_eq!(rig.dirty_gauge(), 0.0);
    assert_eq!(gate.publishes().started.len(), 8);
    assert_eq!(stats.dirty_bytes_peak, 4 * MIB);
    assert_eq!(stats.dirty_bytes_admitted, 8 * MIB);
    assert!(stats.dirty_stalls >= 1);
    assert_eq!(rig.writeback_stalls(), stats.dirty_stalls);
}

/// The cap is `min(256 MiB, 2 x ceiling)` and the pool `ceiling + 32 MiB`: twice a small
/// ceiling, exactly 256 MiB at a ceiling of 128 MiB, 256 MiB above it (and at a ceiling too large
/// to double); the defaults are those of the 384 MiB ceiling of 4.3's table.
#[test]
fn the_dirty_cap_is_the_smaller_of_256_mib_and_twice_the_ceiling() {
    assert_eq!(dirty_cap_for(MIB), 2 * MIB);
    assert_eq!(dirty_cap_for(16 * MIB), 32 * MIB);
    assert_eq!(dirty_cap_for(127 * MIB), 254 * MIB);
    assert_eq!(dirty_cap_for(128 * MIB), DIRTY_CAP_MAX_BYTES);
    assert_eq!(dirty_cap_for(384 * MIB), DIRTY_CAP_MAX_BYTES);
    assert_eq!(dirty_cap_for(u64::MAX), DIRTY_CAP_MAX_BYTES);
    assert_eq!(DIRTY_CAP_MAX_BYTES, 256 * MIB);
    assert_eq!(buffer_bytes_for(16 * MIB), 48 * MIB);
    assert_eq!(buffer_bytes_for(u64::MAX), u64::MAX);
    let small = EngineConfig::default().with_ceiling(16 * MIB);
    assert_eq!(
        (small.buffer_bytes, small.dirty_cap_bytes),
        (48 * MIB, 32 * MIB)
    );
    let d = EngineConfig::default();
    assert_eq!(DEFAULT_CEILING_BYTES, 384 * MIB);
    assert_eq!((d.buffer_bytes, d.dirty_cap_bytes), (416 * MIB, 256 * MIB));
    assert_eq!(d.clone().with_ceiling(DEFAULT_CEILING_BYTES), d);
}

/// An engine made for a ceiling of 1 MiB has a cap of 2 MiB: of 4 cells with their publishes
/// held, 2 are handed over.
#[tokio::test]
async fn a_small_ceiling_caps_dirty_bytes_at_twice_the_ceiling() {
    let (rig, gate) = gated(EngineConfig::default().with_ceiling(MIB));
    let pack = pack_on(&rig, "dirty/small-ceiling", 4 * MIB, 4);
    prepare(&rig, &pack);
    gate.until(|p| p.in_flight == 2).await;
    let stats = rig.until(|s| s.dirty_waiting >= 1).await;
    assert_eq!(gate.publishes().started.len(), 2);
    assert_eq!(stats.dirty_bytes, 2 * MIB);
    gate.open();
    let stats = rig
        .until(|s| s.cells_published == 4 && s.dirty_bytes == 0)
        .await;
    assert_eq!(stats.dirty_bytes_peak, 2 * MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, 4 * MIB), pack.bytes.to_vec());
}

/// A publish that fails gives its share of the cap back at once: with a cap of 2 cells, cell 0's
/// publish fails and cells 1 and 2 are both handed over while cell 3 waits. The failed cell is
/// left absent and the others are published.
#[tokio::test]
async fn a_failed_publish_releases_its_share_of_the_cap() {
    let (rig, gate) = gated(capped(2 * MIB));
    let pack = pack_on(&rig, "dirty/failed", 4 * MIB, 5);
    gate.fail(&pack.key(), 0);
    prepare(&rig, &pack);
    let stats = rig
        .until(|s| s.dirty_waiting == 1 && s.dirty_bytes_admitted == 3 * MIB)
        .await;
    assert_eq!(stats.total_gets(), 1);
    let held = gate.until(|p| p.in_flight == 2).await;
    assert_eq!(held.returned, 1);
    // The publishes run on threads of the blocking pool: they may start in any order.
    let mut started = held.started;
    started.sort();
    assert_eq!(
        started,
        vec![(pack.key(), 0), (pack.key(), 1), (pack.key(), 2)]
    );
    assert_eq!(stats.dirty_bytes, 2 * MIB);

    gate.open();
    let stats = rig
        .until(|s| s.cells_published == 3 && s.dirty_bytes == 0)
        .await;
    assert_eq!(stats.bytes_wasted, MIB);
    assert_eq!(stats.dirty_bytes_admitted, 4 * MIB);
    assert_eq!(rig.dirty_gauge(), 0.0);
    assert_eq!(
        rig.cells(&pack),
        vec![
            CellState::Absent,
            CellState::Present,
            CellState::Present,
            CellState::Present
        ]
    );
}

fn hot_dev(node: u8) -> DeviceRef {
    DeviceRef::Hot(NodeId(node))
}

/// A hedge's loser never counts as dirty: the hedge delivers cells 0 and 1 and stalls, then the
/// original delivers all three cells. Its copies of cells 0 and 1, whose tokens were already
/// spent, are dropped before the cap; only the three published cells were ever dirty.
#[tokio::test]
async fn a_hedge_losers_bytes_never_count_as_dirty() {
    let rig = Rig::new(8, EngineConfig::default());
    let size = 3 * MIB;
    let pack = Pack::new("dirty/hedge", size, 6);
    rig.upstream.add(
        &pack,
        location(generation(1), size, vec![hot(1, size), hot(2, size)]),
    );
    let original = Arc::new(Semaphore::new(0));
    rig.upstream
        .script(hot_dev(1), Script::Gated(original.clone()));
    rig.upstream
        .script(hot_dev(2), Script::StallAfter(2 * MIB as usize));
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        rig.clock.advance(30);
        rig.until(|s| s.gets(GetKind::Hedge) == 1 && s.cells_published == 2)
            .await;
        original.add_permits(1);
    };
    let (outcome, ()) = tokio::join!(rig.engine.wait(&pack.spec, pack.all(), JOB), driver);
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig
        .until(|s| s.cells_published == 3 && s.dirty_bytes == 0)
        .await;
    assert_eq!(stats.bytes_wasted, 2 * MIB);
    assert_eq!(stats.dirty_bytes_admitted, 3 * MIB);
    assert_eq!(stats.bytes_published, 3 * MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// Across six packs fetched at once into a pool of 4 cell buffers, with a cap of 2 cells and the
/// publishes held, every buffer is in use and parts wait for one, but never more than 4 are in
/// use; once the publishes return every cell is published and every buffer is back.
#[tokio::test]
async fn in_flight_cell_buffers_never_exceed_the_pool_across_many_parts() {
    let config = EngineConfig {
        buffer_bytes: 4 * MIB,
        dirty_cap_bytes: 2 * MIB,
        ..EngineConfig::default()
    };
    let (rig, gate) = gated(config);
    let packs: Vec<Pack> = (0..6u8)
        .map(|i| pack_on(&rig, &format!("dirty/many-{i}"), 4 * MIB, 10 + i))
        .collect();
    for pack in &packs {
        prepare(&rig, pack);
    }
    let stats = rig
        .until(|s| s.buffers == 4 && s.buffers_waiting >= 1)
        .await;
    assert_eq!(stats.buffers_peak, 4);
    gate.until(|p| p.in_flight == 2).await;

    gate.open();
    let stats = rig
        .until(|s| s.cells_published == 24 && s.buffers == 0 && s.buffers_waiting == 0)
        .await;
    assert_eq!(stats.buffers_peak, 4);
    assert_eq!(stats.dirty_bytes, 0);
    for pack in &packs {
        assert_eq!(rig.entry_bytes(pack, 0, 4 * MIB), pack.bytes.to_vec());
    }
}

/// A demand cell is not starved behind prepare cells that wait on the cap: with a cap of 2 cells
/// held by a prepare pack whose next cell waits, a reader's cell of another pack waits too, and
/// the first share given back goes to the reader's cell, not to the prepare cell queued first.
#[tokio::test]
async fn a_demand_cell_is_not_starved_behind_prepare_cells_that_wait_on_the_cap() {
    let (rig, gate) = gated(capped(2 * MIB));
    let background = pack_on(&rig, "dirty/prepare", 4 * MIB, 7);
    let read = pack_on(&rig, "dirty/demand", MIB, 8);
    prepare(&rig, &background);
    gate.until(|p| p.in_flight == 2).await;
    rig.until(|s| s.dirty_waiting == 1).await;
    let driver = async {
        rig.until(|s| s.dirty_waiting == 2).await;
        gate.release(1);
        let p = gate.until(|p| p.started.len() == 3).await;
        assert_eq!(p.started[2], (read.key(), 0));
        gate.open();
    };
    let (outcome, ()) = tokio::join!(rig.engine.wait(&read.spec, read.all(), JOB), driver);
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig
        .until(|s| s.cells_published == 5 && s.dirty_bytes == 0)
        .await;
    assert_eq!(stats.dirty_waiting, 0);
    assert_eq!(
        rig.entry_bytes(&background, 0, 4 * MIB),
        background.bytes.to_vec()
    );
    assert_eq!(rig.entry_bytes(&read, 0, MIB), read.bytes.to_vec());
}

/// A cap below one cell still publishes, one cell at a time: a cell is let in whenever nothing is
/// being published, so the dirty bytes reach one cell and no more.
#[tokio::test]
async fn a_cap_below_one_cell_publishes_one_cell_at_a_time() {
    let (rig, gate) = gated(capped(MIB / 2));
    let pack = pack_on(&rig, "dirty/below-a-cell", 3 * MIB, 9);
    prepare(&rig, &pack);
    gate.until(|p| p.in_flight == 1).await;
    let stats = rig.until(|s| s.dirty_waiting == 1).await;
    assert_eq!(stats.dirty_bytes, MIB);
    assert_eq!(gate.publishes().started.len(), 1);
    gate.open();
    let stats = rig
        .until(|s| s.cells_published == 3 && s.dirty_bytes == 0)
        .await;
    assert_eq!(stats.dirty_bytes_peak, MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, 3 * MIB), pack.bytes.to_vec());
}
