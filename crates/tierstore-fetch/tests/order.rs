//! The order parts go in (4.5), driven through the frozen `Engine` on the rig: deficit round
//! robin across jobs, `(rank, offset)` within a job, and the active-pack window of a class.
//! The budget grants one permit at a time and the class loop hands out one part at a time, so the
//! order of the GET log is the order the queue picked; parts are one cell and the budget's part
//! size is one cell, so every part is exactly one quantum.
//!
//! Catches: a job that is served twice in a row while another waits; a job that starves another;
//! a later-ranked pack served before an earlier-ranked one of the same job, or its parts out of
//! offset order; more than the window's packs partly filled at prepare class; a pack outside the
//! window that never starts once one finishes; a demand read held by the window.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use support::{MIB, Pack, Rig, Script, generation, hot, location};
use tierstore_fetch::EngineConfig;
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::ids::{CellRun, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

/// A rig that has no permit yet, takes one class loop pick at a time, and plans every part as a
/// single cell.
fn serial_rig() -> Rig {
    let config = EngineConfig {
        small_pack_max: 0,
        acquire_slots: 1,
        ..EngineConfig::default()
    };
    let rig = Rig::new(0, config);
    rig.budget.set_part_size(MIB);
    rig
}

fn add(rig: &Rig, pack: &Pack, node: u8) {
    let size = pack.spec.size;
    rig.upstream.add(
        pack,
        location(Some(generation(1)), size, vec![hot(node, size)]),
    );
}

fn cells(pack: &Pack) -> [CellRun; 1] {
    [CellRun {
        first: 0,
        count: (pack.spec.size / MIB) as u32,
    }]
}

/// Two jobs ensuring a pack each at rank 0 are served in turn, one part each.
#[tokio::test]
async fn two_jobs_ensuring_a_pack_each_at_rank_zero_alternate_by_deficit_round_robin() {
    let a = Pack::new("job-a", 6 * MIB, 1);
    let b = Pack::new("job-b", 6 * MIB, 2);
    let rig = serial_rig();
    add(&rig, &a, 1);
    add(&rig, &b, 1);
    rig.engine
        .ensure(&a.spec, &cells(&a), Class::Prepare, JobId(1), Rank(0));
    rig.engine
        .ensure(&b.spec, &cells(&b), Class::Prepare, JobId(2), Rank(0));
    rig.until(|s| s.locates >= 2).await;
    rig.budget.add_permits(1);
    rig.until(|s| s.bytes_published == 12 * MIB).await;
    let jobs: Vec<u64> = rig.upstream.gets().iter().map(|g| g.job.0).collect();
    assert_eq!(jobs.len(), 12);
    assert_eq!(jobs.iter().filter(|j| **j == 1).count(), 6);
    assert_eq!(jobs.iter().filter(|j| **j == 2).count(), 6);
    // The first part goes to whichever job's pack was located first, alone; from then on the
    // jobs take turns until one has no part left.
    assert!(
        jobs[1..11].windows(2).all(|w| w[0] != w[1]),
        "one job was served twice in a row: {jobs:?}"
    );
    let stats = rig.engine.stats();
    assert_eq!(stats.bytes_fetched, 12 * MIB);
}

/// Within one job a pack of an earlier rank goes before the remaining parts of a longer pack of
/// a later rank, and the parts of each pack go in offset order.
#[tokio::test]
async fn within_one_job_a_pack_at_rank_one_goes_before_the_tail_of_a_pack_at_rank_five() {
    let early = Pack::new("a-rank-1", 3 * MIB, 3);
    let late = Pack::new("b-rank-5", 8 * MIB, 4);
    let rig = serial_rig();
    add(&rig, &early, 1);
    add(&rig, &late, 1);
    rig.engine
        .ensure(&late.spec, &cells(&late), Class::Prepare, JobId(1), Rank(5));
    rig.engine.ensure(
        &early.spec,
        &cells(&early),
        Class::Prepare,
        JobId(1),
        Rank(1),
    );
    rig.until(|s| s.locates >= 2).await;
    rig.budget.add_permits(1);
    rig.until(|s| s.bytes_published == 11 * MIB).await;
    let keys: Vec<String> = rig
        .upstream
        .gets()
        .iter()
        .map(|g| g.key.0.clone())
        .collect();
    assert_eq!(keys.len(), 11);
    // The first part may belong to the late pack if it was located first and picked alone.
    assert!(
        keys[1..].windows(2).all(|w| w[0] <= w[1]),
        "a rank-5 part went before a rank-1 part: {keys:?}"
    );
    let steps = |n: u64| (0..n).map(|i| (i * MIB, MIB)).collect::<Vec<_>>();
    assert_eq!(rig.get_ranges(&early), steps(3));
    assert_eq!(rig.get_ranges(&late), steps(8));
    let gets = rig.upstream.gets();
    assert!(gets.iter().all(|g| g.job == JobId(1)));
}

/// Four packs ensured at prepare class: three are partly filled at once, the fourth starts only
/// after one of them is done.
#[tokio::test]
async fn the_fourth_pack_at_prepare_class_starts_only_after_one_of_the_first_three_finished() {
    let (rig, packs, gates) = window_rig();
    let s = rig.until(|s| s.total_gets() >= 9).await;
    assert_eq!(s.fills, 4);
    assert_eq!(s.bytes_published, 0);
    let log = rig.upstream.gets();
    let first_pack = log[0].key.clone();
    let idle = idle_pack(&packs, &log);
    // Pack of the first GET finishes; only then may the fourth pack's parts go.
    let released = packs.iter().position(|p| p.key() == first_pack).unwrap();
    gates[released].add_permits(3);
    let s = rig.until(|s| s.total_gets() >= 10).await;
    assert!(
        s.bytes_published >= 3 * MIB,
        "a fourth pack's part started before any pack finished"
    );
    for gate in &gates {
        gate.add_permits(3);
    }
    rig.until(|s| s.bytes_published == 12 * MIB).await;
    let log = rig.upstream.gets();
    assert_eq!(log.len(), 12);
    let started: BTreeSet<PackKey> = log[..9].iter().map(|g| g.key.clone()).collect();
    assert_eq!(started.len(), 3);
    assert!(!started.contains(&packs[idle].key()));
    assert!(log[9..].iter().all(|g| g.key == packs[idle].key()));
    assert_eq!(
        rig.get_ranges(&packs[idle]),
        vec![(0, MIB), (MIB, MIB), (2 * MIB, MIB)]
    );
}

/// A demand read of a pack outside the window is fetched at once, at demand class, while three
/// prepare packs are partly filled and nothing has finished.
#[tokio::test]
async fn a_demand_read_of_a_pack_outside_the_prepare_window_is_fetched_without_waiting() {
    let (rig, packs, gates) = window_rig();
    rig.until(|s| s.total_gets() >= 9).await;
    let idle = idle_pack(&packs, &rig.upstream.gets());
    let p = &packs[idle];
    let (outcome, tenth) =
        tokio::join!(rig.engine.wait(&p.spec, p.range(0, MIB), JobId(1)), async {
            let s = rig.until(|s| s.total_gets() >= 10).await;
            gates[idle].add_permits(1);
            s
        });
    assert_eq!(outcome, WaitOutcome::Ready);
    assert_eq!(tenth.bytes_published, 0);
    let get = rig.upstream.gets()[9].clone();
    assert_eq!((get.key, get.class), (p.key(), Class::Demand));
    for gate in &gates {
        gate.add_permits(3);
    }
    // Once a held pack finishes, the window lets the fourth pack's other two cells go too.
    let s = rig.until(|s| s.bytes_published == 12 * MIB).await;
    assert_eq!(s.bytes_fetched, s.bytes_published);
}

/// Four three-cell packs on four hot devices, each device holding every part at its own gate,
/// all ensured at prepare class by one job.
fn window_rig() -> (Rig, Vec<Pack>, Vec<Arc<Semaphore>>) {
    let config = EngineConfig {
        small_pack_max: 0,
        ..EngineConfig::default()
    };
    let rig = Rig::new(64, config);
    rig.budget.set_part_size(MIB);
    let packs: Vec<Pack> = (0..4u8)
        .map(|i| Pack::new(&format!("window-{i}"), 3 * MIB, i + 1))
        .collect();
    let gates: Vec<Arc<Semaphore>> = (0..4).map(|_| Arc::new(Semaphore::new(0))).collect();
    for (i, pack) in packs.iter().enumerate() {
        let node = i as u8 + 1;
        add(&rig, pack, node);
        for _ in 0..3 {
            rig.upstream.script(
                DeviceRef::Hot(NodeId(node)),
                Script::Gated(gates[i].clone()),
            );
        }
        rig.engine
            .ensure(&pack.spec, &cells(pack), Class::Prepare, JobId(1), Rank(0));
    }
    (rig, packs, gates)
}

/// The index of the one pack that has no GET in `log`.
fn idle_pack(packs: &[Pack], log: &[tierstore_fetch_api::upstream::GetRequest]) -> usize {
    let started: BTreeSet<PackKey> = log.iter().map(|g| g.key.clone()).collect();
    assert_eq!(started.len(), 3, "the window let in a fourth pack");
    packs
        .iter()
        .position(|p| !started.contains(&p.key()))
        .unwrap()
}
