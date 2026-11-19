//! Single flight per pack (4.1), cell claims (4.2) and demand sub-ranges (4.5), driven through
//! the frozen `Engine` on the rig: concurrent readers and ensures of one pack share one fill,
//! one `LOCATE` and one GET per cell; a demand for a few cells of a part that has not started is
//! fetched on its own and the part skips those cells; a demand inside a started part waits for
//! it; parts finish in any order and a read is served as soon as its own cells land.
//!
//! Catches: a second fill (or second `LOCATE`) for a pack that is already being filled; a cell
//! fetched twice, or a GET range overlapping another; a part that re-fetches cells a demand
//! sub-range already claimed; a demand sub-range for a wide demand (or none for a narrow one);
//! an extra GET for a demand inside a part that is already in flight; a read that waits for
//! cells outside its range; GETs that do not carry the class, job and rank of the want; bytes
//! fetched that differ from bytes published when nothing fails.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use support::{MIB, Pack, Rig, Script, generation, hot, location};
use tierstore_fetch::plan::{PlanInput, plan};
use tierstore_fetch::{EngineConfig, GetKind};
use tierstore_fetch_api::budget::Budget;
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::ids::{CellRun, Class, JobId, Rank};
use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

/// A rig whose upstream holds `pack` on one hot replica.
fn rig_for(pack: &Pack, permits: usize, config: EngineConfig) -> Rig {
    let rig = Rig::new(permits, config);
    let size = pack.spec.size;
    rig.upstream
        .add(pack, location(generation(1), size, vec![hot(1, size)]));
    rig
}

fn all_cells(pack: &Pack) -> [CellRun; 1] {
    [CellRun {
        first: 0,
        count: (pack.spec.size / MIB) as u32,
    }]
}

/// The `(offset, len)` of every GET of `pack`, ascending.
fn sorted_ranges(rig: &Rig, pack: &Pack) -> Vec<(u64, u64)> {
    let mut ranges = rig.get_ranges(pack);
    ranges.sort_unstable();
    ranges
}

/// Whether no range of the ascending `ranges` overlaps the next.
fn disjoint(ranges: &[(u64, u64)]) -> bool {
    ranges.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0)
}

fn lens(ranges: &[(u64, u64)]) -> u64 {
    ranges.iter().map(|r| r.1).sum()
}

/// Which cells of `pack` are present.
fn present(rig: &Rig, pack: &Pack) -> Vec<bool> {
    rig.cells(pack)
        .iter()
        .map(|c| *c == CellState::Present)
        .collect()
}

/// A `present` vector with exactly the cells `first..end` set.
fn only(cells: usize, first: usize, end: usize) -> Vec<bool> {
    (0..cells).map(|c| (first..end).contains(&c)).collect()
}

/// Concurrent waits and an ensure on one pack: one fill, one `LOCATE`, every cell fetched once.
#[tokio::test]
async fn concurrent_readers_and_an_ensure_on_one_pack_start_one_fill_and_fetch_every_cell_once() {
    let pack = Pack::new("many-readers", 12 * MIB, 3);
    let rig = rig_for(&pack, 64, EngineConfig::default());
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(1),
        Rank(0),
    );
    // Reader spans in MiB: disjoint ones, an overlapping one and the whole pack.
    let spans = [
        (0, 2),
        (2, 2),
        (4, 2),
        (6, 2),
        (8, 2),
        (10, 2),
        (3, 4),
        (0, 12),
    ];
    let outcomes = futures::future::join_all(spans.iter().map(|(at, len)| {
        rig.engine
            .wait(&pack.spec, pack.range(at * MIB, len * MIB), JobId(2))
    }))
    .await;
    assert_eq!(outcomes, vec![WaitOutcome::Ready; spans.len()]);
    // A wait is ready once its cells are present; the op counts them just after (`until`).
    let stats = rig.until(|s| s.cells_published >= 12).await;
    assert_eq!(stats.fills, 1);
    assert_eq!(stats.locates, 1);
    assert_eq!(rig.upstream.locates().len(), 1);
    let ranges = sorted_ranges(&rig, &pack);
    assert!(disjoint(&ranges), "GET ranges overlap: {ranges:?}");
    assert_eq!(lens(&ranges), 12 * MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, 12 * MIB), pack.bytes.to_vec());
    assert_eq!(stats.cells_published, 12);
    assert_eq!(stats.bytes_fetched, 12 * MIB);
    assert_eq!(stats.bytes_published, 12 * MIB);
}

/// Readers alone fetch the union of their cells once and leave the other cells absent.
#[tokio::test]
async fn readers_on_overlapping_spans_fetch_the_union_of_their_cells_once() {
    let pack = Pack::new("union", 16 * MIB, 4);
    let rig = rig_for(&pack, 64, EngineConfig::default());
    // Reader `i` reads cells `i..i + 4`: together cells 0..11.
    let outcomes = futures::future::join_all((0..8u64).map(|i| {
        rig.engine
            .wait(&pack.spec, pack.range(i * MIB, 4 * MIB), JobId(i))
    }))
    .await;
    assert_eq!(outcomes, vec![WaitOutcome::Ready; 8]);
    // A wait is ready once its cells are present; the op counts them just after (`until`).
    let stats = rig.until(|s| s.cells_published >= 11).await;
    assert_eq!(stats.fills, 1);
    assert_eq!(stats.locates, 1);
    let ranges = sorted_ranges(&rig, &pack);
    assert!(disjoint(&ranges), "GET ranges overlap: {ranges:?}");
    assert_eq!(lens(&ranges), 11 * MIB);
    assert_eq!(
        rig.entry_bytes(&pack, 0, 11 * MIB),
        pack.bytes[..(11 * MIB) as usize].to_vec()
    );
    assert_eq!(present(&rig, &pack), only(16, 0, 11));
    assert_eq!(stats.bytes_fetched, 11 * MIB);
    assert_eq!(stats.bytes_published, 11 * MIB);
}

/// `ensure` returns at once, and repeating it (also for a subset, or at another class) never
/// starts a second fill.
#[tokio::test]
async fn ensure_is_idempotent_and_returns_before_any_get() {
    let pack = Pack::new("idempotent", 16 * MIB, 5);
    let rig = rig_for(&pack, 64, EngineConfig::default());
    let whole = all_cells(&pack);
    rig.engine
        .ensure(&pack.spec, &whole, Class::Prepare, JobId(1), Rank(0));
    assert_eq!(rig.engine.stats().fills, 1);
    assert!(rig.upstream.gets().is_empty());
    rig.engine
        .ensure(&pack.spec, &whole, Class::Prepare, JobId(1), Rank(0));
    rig.engine.ensure(
        &pack.spec,
        &[CellRun { first: 4, count: 4 }],
        Class::Background,
        JobId(2),
        Rank(7),
    );
    assert_eq!(rig.engine.stats().fills, 1);
    let outcome = rig.engine.wait(&pack.spec, pack.all(), JobId(1)).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.fills, 1);
    assert_eq!(stats.locates, 1);
    let ranges = sorted_ranges(&rig, &pack);
    assert!(disjoint(&ranges), "GET ranges overlap: {ranges:?}");
    assert_eq!(lens(&ranges), 16 * MIB);
    assert_eq!(stats.bytes_fetched, 16 * MIB);
}

/// A wait after an ensure joins that fill: it is served by it and the rest of the ensure goes on.
#[tokio::test]
async fn a_wait_after_an_ensure_attaches_to_the_same_fill() {
    let pack = Pack::new("attach", 16 * MIB, 6);
    let rig = rig_for(&pack, 64, EngineConfig::default());
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(1),
        Rank(0),
    );
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(9 * MIB, 2 * MIB), JobId(2))
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(rig.engine.stats().fills, 1);
    rig.until(|s| s.bytes_published == 16 * MIB).await;
    let stats = rig.engine.stats();
    assert_eq!(stats.fills, 1);
    assert_eq!(stats.locates, 1);
    assert_eq!(rig.entry_bytes(&pack, 0, 16 * MIB), pack.bytes.to_vec());
    assert_eq!(stats.bytes_fetched, 16 * MIB);
}

/// A demand for the first cell of a part that has not started: a one-cell demand GET, and the
/// part then fetches only the cells after it (GETs <= parts + sub-ranges).
#[tokio::test]
async fn a_demand_for_one_cell_of_a_part_not_started_is_fetched_alone_and_the_part_skips_it() {
    let pack = Pack::new("sub-edge", 16 * MIB, 7);
    let config = EngineConfig {
        acquire_slots: 1,
        ..EngineConfig::default()
    };
    let rig = rig_for(&pack, 2, config);
    // The plan of a 16 MiB pack first touched at cell 0 for 16 cells: cells 0..4 and 4..16.
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(7),
        Rank(3),
    );
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(4 * MIB, MIB), JobId(9))
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    rig.until(|s| s.bytes_published == 16 * MIB).await;
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::DemandSub), 1);
    assert_eq!(stats.gets(GetKind::GridPart), 2);
    assert_eq!(stats.total_gets(), 3);
    assert_eq!(
        sorted_ranges(&rig, &pack),
        vec![(0, 4 * MIB), (4 * MIB, MIB), (5 * MIB, 11 * MIB)]
    );
    let sub = rig
        .upstream
        .gets()
        .into_iter()
        .find(|g| g.range.offset == 4 * MIB)
        .unwrap();
    assert_eq!((sub.class, sub.job), (Class::Demand, JobId(9)));
    assert_eq!(stats.bytes_fetched, 16 * MIB);
    assert_eq!(stats.bytes_published, 16 * MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, 16 * MIB), pack.bytes.to_vec());
}

/// A demand for a cell in the middle of a part not started: the part later fetches the runs of
/// cells before and after it.
#[tokio::test]
async fn a_demand_for_a_middle_cell_splits_the_part_into_the_runs_around_it() {
    let pack = Pack::new("sub-middle", 16 * MIB, 8);
    let config = EngineConfig {
        acquire_slots: 1,
        ..EngineConfig::default()
    };
    let rig = rig_for(&pack, 2, config);
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(7),
        Rank(3),
    );
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(10 * MIB, MIB), JobId(9))
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    rig.until(|s| s.bytes_published == 16 * MIB).await;
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::DemandSub), 1);
    assert_eq!(stats.gets(GetKind::GridPart), 3);
    assert_eq!(
        sorted_ranges(&rig, &pack),
        vec![
            (0, 4 * MIB),
            (4 * MIB, 6 * MIB),
            (10 * MIB, MIB),
            (11 * MIB, 5 * MIB)
        ]
    );
    assert_eq!(stats.bytes_fetched, 16 * MIB);
    assert_eq!(stats.bytes_published, 16 * MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, 16 * MIB), pack.bytes.to_vec());
}

/// A demand of exactly `sub_range_max_cells` cells is still a sub-range of exactly those cells.
#[tokio::test]
async fn a_demand_of_sub_range_max_cells_cells_is_a_sub_range_of_exactly_those_cells() {
    let pack = Pack::new("sub-max", 16 * MIB, 9);
    let rig = rig_for(&pack, 64, EngineConfig::default());
    assert_eq!(EngineConfig::default().sub_range_max_cells, 4);
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(7),
        Rank(0),
    );
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(6 * MIB, 4 * MIB), JobId(9))
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    rig.until(|s| s.bytes_published == 16 * MIB).await;
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::DemandSub), 1);
    let sub = rig
        .upstream
        .gets()
        .into_iter()
        .find(|g| g.range.offset == 6 * MIB)
        .unwrap();
    assert_eq!((sub.range.len, sub.class), (4 * MIB, Class::Demand));
    assert_eq!(stats.bytes_fetched, 16 * MIB);
}

/// A demand wider than `sub_range_max_cells` promotes the whole part to demand class.
#[tokio::test]
async fn a_demand_wider_than_sub_range_max_cells_promotes_the_part_to_demand_class() {
    let pack = Pack::new("promote", 16 * MIB, 10);
    let rig = rig_for(&pack, 64, EngineConfig::default());
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(7),
        Rank(0),
    );
    // Five cells inside the part of cells 4..16.
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(6 * MIB, 5 * MIB), JobId(9))
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    rig.until(|s| s.bytes_published == 16 * MIB).await;
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::DemandSub), 0);
    assert_eq!(stats.gets(GetKind::GridPart), 2);
    let gets = rig.upstream.gets();
    let class_of = |offset: u64| {
        gets.iter()
            .find(|g| g.range.offset == offset)
            .map(|g| (g.range.len, g.class))
            .unwrap()
    };
    assert_eq!(class_of(0), (4 * MIB, Class::Prepare));
    assert_eq!(class_of(4 * MIB), (12 * MIB, Class::Demand));
}

/// A demand for a cell of a part already in flight adds no GET and is served when that cell
/// lands, not when the next part does.
#[tokio::test]
async fn a_demand_inside_a_started_part_issues_no_get_and_is_served_when_the_cell_lands() {
    let pack = Pack::new("started", 16 * MIB, 11);
    let config = EngineConfig {
        acquire_slots: 1,
        ..EngineConfig::default()
    };
    let rig = rig_for(&pack, 1, config);
    let gates = [Arc::new(Semaphore::new(0)), Arc::new(Semaphore::new(0))];
    for gate in &gates {
        rig.upstream
            .script(DeviceRef::Hot(NodeId(1)), Script::Gated(gate.clone()));
    }
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(7),
        Rank(0),
    );
    // With one permit the part of cells 0..4 is the only GET, held at its gate.
    rig.until(|s| s.total_gets() >= 1).await;
    let (outcome, ()) = tokio::join!(
        rig.engine
            .wait(&pack.spec, pack.range(2 * MIB, MIB), JobId(9)),
        async { gates[0].add_permits(1) }
    );
    assert_eq!(outcome, WaitOutcome::Ready);
    // The second part is still held: no cell after the first part is present.
    let cells = present(&rig, &pack);
    assert!(cells[2]);
    assert!(cells[4..].iter().all(|c| !c));
    gates[1].add_permits(1);
    rig.until(|s| s.bytes_published == 16 * MIB).await;
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::DemandSub), 0);
    assert_eq!(stats.gets(GetKind::GridPart), 2);
    assert_eq!(stats.total_gets(), 2);
    assert_eq!(stats.bytes_fetched, 16 * MIB);
    assert_eq!(rig.entry_bytes(&pack, 0, 16 * MIB), pack.bytes.to_vec());
}

/// Every GET carries the class, job and rank of the want that caused it.
#[tokio::test]
async fn gets_carry_the_class_job_and_rank_of_the_want() {
    let rig = Rig::default_rig();
    let mut published_before = 0;
    for (class, job, rank) in [
        (Class::Prepare, 7, 3),
        (Class::Background, 5, 9),
        (Class::Demand, 2, 1),
    ] {
        let pack = Pack::new(&format!("headers-{job}"), 8 * MIB, job as u8);
        rig.upstream.add(
            &pack,
            location(generation(1), 8 * MIB, vec![hot(1, 8 * MIB)]),
        );
        rig.engine
            .ensure(&pack.spec, &all_cells(&pack), class, JobId(job), Rank(rank));
        rig.until(|s| s.bytes_published == published_before + 8 * MIB)
            .await;
        published_before += 8 * MIB;
        let seen: Vec<_> = rig
            .upstream
            .gets()
            .iter()
            .filter(|g| g.key == pack.key())
            .map(|g| (g.class, g.job, g.rank))
            .collect();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen, vec![(class, JobId(job), Rank(rank)); 2], "{class:?}");
    }
}

/// With every part held at a gate and released from the last to the first, each read is served
/// once its own part lands and the parts before it are still absent.
#[tokio::test]
async fn parts_released_in_reverse_order_serve_each_read_when_its_own_part_lands() {
    let pack = Pack::new("reverse", 16 * MIB, 12);
    let config = EngineConfig {
        small_pack_max: 0,
        ..EngineConfig::default()
    };
    let rig = rig_for(&pack, 64, config.clone());
    rig.budget.set_part_size(4 * MIB);
    let pieces = plan(
        &config,
        PlanInput {
            pack_size: pack.spec.size,
            part_size: 4 * MIB,
            first_touch: 0,
            first_touch_cells: 16,
        },
        &BTreeSet::new(),
    );
    assert!(pieces.len() >= 3);
    let gates: Vec<Arc<Semaphore>> = pieces.iter().map(|_| Arc::new(Semaphore::new(0))).collect();
    for gate in &gates {
        rig.upstream
            .script(DeviceRef::Hot(NodeId(1)), Script::Gated(gate.clone()));
    }
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(1),
        Rank(0),
    );
    let parts = pieces.len();
    rig.until(|s| s.total_gets() >= parts as u64).await;
    // The script of a device is consumed in GET order, so GET `i` waits at gate `i`.
    let mut by_offset: Vec<(u64, u64, usize)> = rig
        .upstream
        .gets()
        .iter()
        .enumerate()
        .map(|(i, g)| (g.range.offset, g.range.len, i))
        .collect();
    by_offset.sort_unstable();
    assert_eq!(by_offset.len(), parts);
    for (offset, len, i) in by_offset.into_iter().rev() {
        gates[i].add_permits(1);
        let outcome = rig
            .engine
            .wait(&pack.spec, pack.range(offset, len), JobId(1))
            .await;
        assert_eq!(outcome, WaitOutcome::Ready);
        assert_eq!(present(&rig, &pack), only(16, (offset / MIB) as usize, 16));
    }
    assert_eq!(rig.entry_bytes(&pack, 0, 16 * MIB), pack.bytes.to_vec());
    // A wait is ready once its cells are present; the op counts them just after (`until`).
    let stats = rig.until(|s| s.bytes_published >= 16 * MIB).await;
    assert_eq!(stats.bytes_fetched, 16 * MIB);
    assert_eq!(stats.bytes_published, 16 * MIB);
}

/// A read at the end of a large pack is served while every earlier part is still in flight.
#[tokio::test]
async fn a_read_at_the_end_of_a_large_pack_waits_only_for_its_own_cells() {
    let size = 100 * MIB;
    let pack = Pack::new("large-span", size, 13);
    let config = EngineConfig::default();
    let rig = rig_for(&pack, 64, config.clone());
    let pieces = plan(
        &config,
        PlanInput {
            pack_size: size,
            part_size: rig.budget.part_size(size),
            first_touch: 0,
            first_touch_cells: 100,
        },
        &BTreeSet::new(),
    );
    let gates: Vec<Arc<Semaphore>> = pieces.iter().map(|_| Arc::new(Semaphore::new(0))).collect();
    for gate in &gates {
        rig.upstream
            .script(DeviceRef::Hot(NodeId(1)), Script::Gated(gate.clone()));
    }
    rig.engine.ensure(
        &pack.spec,
        &all_cells(&pack),
        Class::Prepare,
        JobId(1),
        Rank(0),
    );
    let parts = pieces.len() as u64;
    rig.until(|s| s.total_gets() >= parts).await;
    let last = *pieces.last().unwrap();
    let held_at = rig
        .upstream
        .gets()
        .iter()
        .position(|g| g.range.offset == u64::from(last.first) * MIB)
        .unwrap();
    gates[held_at].add_permits(1);
    let outcome = rig
        .engine
        .wait(&pack.spec, pack.range(size - MIB, MIB), JobId(2))
        .await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let cells = present(&rig, &pack);
    assert!(cells[99]);
    assert!(cells[..last.first as usize].iter().all(|c| !c));
    for gate in &gates {
        gate.add_permits(1);
    }
    rig.until(|s| s.bytes_published == size).await;
    assert_eq!(rig.engine.stats().bytes_fetched, size);
}
