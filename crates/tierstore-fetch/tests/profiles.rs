//! The one `EngineConfig::default()` serves both deployment profiles of section 21: what differs
//! between NOW (six combined nodes, two hot replicas, cold `4+2`, a legacy upstream with recorded
//! upload parts) and LATER (three storage nodes, three hot replicas, cold `4+4`) reaches the
//! engine as data, in the locations the upstream answers.
//!
//! Catches: a hot read that ignores the second replica or sends every part to one copy; a stall
//! that is not failed over to the other copy; a legacy pack not cut at its recorded upload parts,
//! a first legacy part that does not go alone, or a cut applied to a pack read from the storage
//! tier; a degraded read charged as one part instead of `k`; a hot read that does not go to the
//! least-loaded copy of three; a chain object whose part crosses the boundary between its blocks
//! or goes to the block that does not cover it; a node that is down not routed around by a
//! degraded read.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{MIB, Pack, Rig, Script, cold, degraded, drive, generation, hot, legacy, location};
use tierstore_fetch::{GetKind, TimerKind};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::engine::Engine;
use tierstore_fetch_api::ids::{CellRun, Class, JobId, Rank};
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, UpstreamError};
use tierstore_format::ids::NodeId;

/// Asks for every cell of `pack` at the prepare class; returns the cell count.
fn prepare(rig: &Rig, pack: &Pack) -> u64 {
    let cells = pack.spec.size.div_ceil(MIB);
    let count = u32::try_from(cells).expect("cells fit");
    rig.engine.ensure(
        &pack.spec,
        &[CellRun { first: 0, count }],
        Class::Prepare,
        JobId(1),
        Rank(0),
    );
    cells
}

/// Fills `pack` at the prepare class and waits until all its cells are published.
async fn fill(rig: &Rig, pack: &Pack) {
    let before = rig.engine.stats().cells_published;
    let cells = prepare(rig, pack);
    rig.until(|s| s.cells_published == before + cells).await;
}

fn queue_wait(ms: u32) -> DriveSignals {
    DriveSignals {
        queue_wait_ms: Some(ms),
        ..DriveSignals::default()
    }
}

fn hot_node(n: u8) -> DeviceRef {
    DeviceRef::Hot(NodeId(n))
}

/// The devices the GETs of `pack` went to, in order.
fn get_devices(rig: &Rig) -> Vec<DeviceRef> {
    rig.upstream
        .gets()
        .iter()
        .map(|g| g.source.device)
        .collect()
}

/// `(parts, devices, node)` of every charge, in order.
fn charges(rig: &Rig) -> Vec<(u32, Vec<DeviceRef>, Option<NodeId>)> {
    rig.budget
        .charges()
        .into_iter()
        .map(|c| (c.parts, c.devices, c.node))
        .collect()
}

fn present(cells: usize) -> Vec<CellState> {
    vec![CellState::Present; cells]
}

/// NOW: a hot pack with two replicas is filled from the first replica alone, one part charged to
/// its device, and the cells hold the pack's bytes.
#[tokio::test]
async fn a_now_hot_pack_with_two_replicas_fills_from_one_replica_with_one_charge() {
    let rig = Rig::default_rig();
    let pack = Pack::new("now-hot", 3 * MIB, 1);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
    );
    fill(&rig, &pack).await;
    assert_eq!(get_devices(&rig), vec![hot_node(1)]);
    assert_eq!(charges(&rig), vec![(1, vec![hot_node(1)], Some(NodeId(1)))]);
    assert_eq!(rig.cells(&pack), present(3));
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// NOW: a stall before the first byte on one replica is retried on the other replica, once, and
/// the pack still fills.
#[tokio::test]
async fn a_now_stall_on_one_hot_replica_fails_over_to_the_other_replica() {
    let rig = Rig::default_rig();
    let pack = Pack::new("now-stall", 3 * MIB, 2);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
    );
    rig.upstream.script(hot_node(1), Script::NoAnswer);
    prepare(&rig, &pack);
    rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
    rig.clock.advance(2_000);
    let stats = rig.until(|s| s.cells_published == 3).await;
    assert_eq!(get_devices(&rig), vec![hot_node(1), hot_node(2)]);
    assert_eq!(stats.gets(GetKind::GridPart), 1);
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(stats.stall_retries, 1);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// NOW: a legacy pack with a recorded upload part size is cut at those boundaries (no GET crosses
/// 8 MiB or 16 MiB), and its first part goes alone: the part that stalls is the only GET until
/// its answer pins the generation, and every later GET carries the pinned generation.
#[tokio::test]
async fn a_now_legacy_pack_is_cut_at_its_upload_parts_and_its_first_part_goes_alone() {
    let rig = Rig::default_rig();
    let mut pack = Pack::new("now-legacy", 20 * MIB, 3);
    pack.spec.upload_part_size = 8 * MIB;
    let size = pack.spec.size;
    rig.upstream
        .add(&pack, location(None, size, vec![legacy(size)]));
    rig.upstream.script(DeviceRef::Legacy, Script::NoAnswer);
    prepare(&rig, &pack);
    rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
    rig.clock.advance(2_000);
    rig.until(|s| s.cells_published == 20).await;
    let mut ranges = rig.get_ranges(&pack);
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        vec![
            (0, 4 * MIB),
            (0, 4 * MIB),
            (4 * MIB, 4 * MIB),
            (8 * MIB, 8 * MIB),
            (16 * MIB, 4 * MIB),
        ]
    );
    let generations: Vec<_> = rig.upstream.gets().iter().map(|g| g.generation).collect();
    assert_eq!(
        generations,
        vec![
            None,
            None,
            Some(generation(1)),
            Some(generation(1)),
            Some(generation(1)),
        ]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// The same recorded upload part size is ignored when the pack is read from the storage tier:
/// the parts are the first-touch piece and the rest, with no cut at 8 MiB or 16 MiB.
#[tokio::test]
async fn a_pack_with_an_upload_part_size_read_from_the_storage_tier_is_not_cut() {
    let rig = Rig::default_rig();
    let mut pack = Pack::new("now-stored", 20 * MIB, 4);
    pack.spec.upload_part_size = 8 * MIB;
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)]),
    );
    fill(&rig, &pack).await;
    let mut ranges = rig.get_ranges(&pack);
    ranges.sort_unstable();
    assert_eq!(ranges, vec![(0, 4 * MIB), (4 * MIB, 16 * MIB)]);
}

/// NOW: a cold `4+2` pack whose data-shard drive stalls is retried as a degraded read of the four
/// other shards, charged four parts against the client ceiling and one permit on each of the four
/// drives.
#[tokio::test]
async fn a_now_cold_pack_with_a_stalled_data_shard_drive_is_read_degraded_at_four_parts() {
    let rig = Rig::default_rig();
    let pack = Pack::new("now-cold", 3 * MIB, 5);
    let size = pack.spec.size;
    let shards = [drive(2, 2), drive(3, 3), drive(4, 4), drive(5, 5)];
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![cold(1, 1, 0, size), degraded(6, &shards, 0, size)],
        ),
    );
    rig.upstream.script(drive(1, 1), Script::NoAnswer);
    prepare(&rig, &pack);
    rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
    rig.clock.advance(2_000);
    let stats = rig.until(|s| s.cells_published == 3).await;
    assert_eq!(get_devices(&rig), vec![drive(1, 1), hot_node(6)]);
    assert_eq!(
        charges(&rig),
        vec![
            (1, vec![drive(1, 1)], Some(NodeId(1))),
            (4, shards.to_vec(), Some(NodeId(6))),
        ]
    );
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// NOW: a healthy cold pack is one stream on one drive, charged one part; the degraded source of
/// its location is not used.
#[tokio::test]
async fn a_now_healthy_cold_pack_is_read_from_its_drive_at_one_part() {
    let rig = Rig::default_rig();
    let pack = Pack::new("now-cold-ok", 3 * MIB, 6);
    let size = pack.spec.size;
    let shards = [drive(2, 2), drive(3, 3), drive(4, 4), drive(5, 5)];
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![degraded(6, &shards, 0, size), cold(1, 1, 0, size)],
        ),
    );
    fill(&rig, &pack).await;
    assert_eq!(get_devices(&rig), vec![drive(1, 1)]);
    assert_eq!(charges(&rig), vec![(1, vec![drive(1, 1)], Some(NodeId(1)))]);
}

/// LATER: a hot pack held by three nodes goes to the copy advertising the shortest queue wait,
/// and to the next one once the signals change.
#[tokio::test]
async fn a_later_hot_pack_with_three_replicas_goes_to_the_least_loaded_copy() {
    let rig = Rig::default_rig();
    let first = Pack::new("later-hot-a", 2 * MIB, 7);
    let second = Pack::new("later-hot-b", 2 * MIB, 8);
    for pack in [&first, &second] {
        let size = pack.spec.size;
        rig.upstream.add(
            pack,
            location(
                Some(generation(1)),
                size,
                vec![hot(1, size), hot(2, size), hot(3, size)],
            ),
        );
    }
    rig.engine.observe_signals(hot_node(1), queue_wait(80));
    rig.engine.observe_signals(hot_node(2), queue_wait(10));
    rig.engine.observe_signals(hot_node(3), queue_wait(30));
    fill(&rig, &first).await;
    assert_eq!(get_devices(&rig), vec![hot_node(2)]);
    assert_eq!(charges(&rig), vec![(1, vec![hot_node(2)], Some(NodeId(2)))]);
    rig.engine.observe_signals(hot_node(2), queue_wait(200));
    fill(&rig, &second).await;
    assert_eq!(get_devices(&rig), vec![hot_node(2), hot_node(3)]);
}

/// LATER: a stall on the least-loaded copy of three is retried on the next least-loaded copy.
#[tokio::test]
async fn a_later_stall_on_the_least_loaded_copy_fails_over_to_the_next_least_loaded() {
    let rig = Rig::default_rig();
    let pack = Pack::new("later-hot-stall", 2 * MIB, 9);
    let size = pack.spec.size;
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![hot(1, size), hot(2, size), hot(3, size)],
        ),
    );
    rig.engine.observe_signals(hot_node(1), queue_wait(80));
    rig.engine.observe_signals(hot_node(2), queue_wait(10));
    rig.engine.observe_signals(hot_node(3), queue_wait(30));
    rig.upstream.script(hot_node(2), Script::NoAnswer);
    prepare(&rig, &pack);
    rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
    rig.clock.advance(2_000);
    rig.until(|s| s.cells_published == 2).await;
    assert_eq!(get_devices(&rig), vec![hot_node(2), hot_node(3)]);
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// LATER: a chain object spanning two blocks (extents `[0, 3 MiB)` and `[3 MiB, 6 MiB)`) is
/// fetched with no GET crossing the boundary, every GET to the block that covers it.
#[tokio::test]
async fn a_later_chain_object_is_fetched_without_a_get_crossing_its_block_boundary() {
    let rig = Rig::default_rig();
    let pack = Pack::new("later-chain", 6 * MIB, 10);
    let size = pack.spec.size;
    let x = 3 * MIB;
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![cold(1, 1, 0, x), cold(2, 2, x, size - x)],
        ),
    );
    fill(&rig, &pack).await;
    let mut seen: Vec<(u64, u64, DeviceRef)> = rig
        .upstream
        .gets()
        .iter()
        .map(|g| (g.range.offset, g.range.len, g.source.device))
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        vec![
            (0, x, drive(1, 1)),
            (x, MIB, drive(2, 2)),
            (4 * MIB, 2 * MIB, drive(2, 2)),
        ]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}

/// LATER: with the node holding the data shard down (its connection fails), the part is retried
/// as a degraded read of four shards on the surviving nodes, charged four parts.
#[tokio::test]
async fn a_later_degraded_read_serves_a_pack_whose_data_shard_node_is_down() {
    let rig = Rig::default_rig();
    let pack = Pack::new("later-down", 3 * MIB, 11);
    let size = pack.spec.size;
    let shards = [drive(1, 10), drive(1, 11), drive(3, 12), drive(3, 13)];
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![cold(2, 2, 0, size), degraded(1, &shards, 0, size)],
        ),
    );
    rig.upstream.script(
        drive(2, 2),
        Script::Fail(UpstreamError::Transport {
            key: pack.key(),
            device: drive(2, 2),
            reason: "connection refused".to_owned(),
        }),
    );
    fill(&rig, &pack).await;
    assert_eq!(get_devices(&rig), vec![drive(2, 2), hot_node(1)]);
    assert_eq!(
        charges(&rig),
        vec![
            (1, vec![drive(2, 2)], Some(NodeId(2))),
            (4, shards.to_vec(), Some(NodeId(1))),
        ]
    );
    assert_eq!(rig.entry_bytes(&pack, 0, size), pack.bytes.to_vec());
}
