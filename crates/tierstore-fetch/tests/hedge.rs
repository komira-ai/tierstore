//! Hedging of demand parts (design 4.6, 10.6; the section 14 rows "Hedge only for demand, and only
//! with an alternative" and "Hedge and original both deliver a cell"), through the engine's `wait`
//! on a rig whose clock moves only when a test moves it.
//!
//! Proves: a demand part that has no first byte after the hedge trigger (30 ms floor) is hedged to
//! the other hot replica (not before the trigger) and the waiter is served from the hedge; a first
//! byte before the trigger, a `prepare` part, a single copy, a hot alternative with a longer
//! advertised queue wait, a device that answered 503 (either end) and a second stalled part while
//! a hedge is in flight are not hedged; the hedge permit is released when its hedge ends; when
//! hedge and original both deliver, every cell is published once; a cold range of at most 4 MiB is
//! hedged by a degraded read charged `k` parts when its `k` source drives advertise no queue, and
//! a longer range, an unknown queue or a queue above zero is not.
//!
//! Catches: a hedge that fires before the trigger or after a first byte; a hedge of `prepare`
//! parts or of a part with no other copy; a hedge to a more loaded replica or to or from a device
//! in a decrease state; more than one hedge in flight per node, or a permit that is never given
//! back; a cell published twice when hedge and original both deliver; a degraded hedge of a range
//! over the limit, or without the source drives idle, or charged one part instead of `k`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::future::Future;
use std::sync::Arc;

use support::{MIB, Pack, Rig, Script, cold, degraded, drive, generation, hot, location};
use tierstore_fetch::{EngineConfig, GetKind, TimerKind};
use tierstore_fetch_api::engine::{Engine, WaitOutcome};
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, Rank};
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, Location, UpstreamError};
use tierstore_format::ids::NodeId;
use tokio::sync::Semaphore;

const JOB: JobId = JobId(1);

fn hot_dev(node: u8) -> DeviceRef {
    DeviceRef::Hot(NodeId(node))
}

/// Two hot replicas (nodes 1 and 2) of a pack of `size` bytes.
fn replicas(size: u64) -> Location {
    location(Some(generation(1)), size, vec![hot(1, size), hot(2, size)])
}

fn queue_wait(ms: u32) -> DriveSignals {
    DriveSignals {
        queue_wait_ms: Some(ms),
        ..DriveSignals::default()
    }
}

fn drive_queue(n: u32) -> DriveSignals {
    DriveSignals {
        drive_queue: Some(n),
        ..DriveSignals::default()
    }
}

fn unavailable(pack: &Pack, node: u8) -> Script {
    Script::Fail(UpstreamError::Unavailable {
        key: pack.key(),
        device: hot_dev(node),
        retry_after_ms: 5_000,
        signals: DriveSignals::default(),
    })
}

fn demand_cell(first: u32) -> [CellRun; 1] {
    [CellRun { first, count: 1 }]
}

/// A rig with one pack of `size` bytes on two hot replicas.
fn two_replicas(size: u64, config: EngineConfig) -> (Rig, Pack) {
    let rig = Rig::new(8, config);
    let pack = Pack::new("hot/pack", size, 5);
    rig.upstream.add(&pack, replicas(size));
    (rig, pack)
}

/// A `wait` on `range` of `pack`, run beside `driver`, which moves the clock and releases gates.
async fn wait_beside(
    rig: &Rig,
    pack: &Pack,
    range: ByteRange,
    driver: impl Future<Output = ()>,
) -> WaitOutcome {
    let (outcome, ()) = tokio::join!(rig.engine.wait(&pack.spec, range, JOB), driver);
    outcome
}

/// Once the first GET of a demand part is waiting and its hedge trigger is armed: passes the
/// trigger (30 ms), waits until the engine has handled it, then passes the 2 s pre-first-byte
/// timeout, so a part that was not hedged goes on by its stall retry and every wait ends.
async fn pass_trigger_then_first_byte_timeout(rig: &Rig) {
    rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
        .await;
    rig.clock.advance(30);
    rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
    rig.clock.advance(1_970);
}

/// A demand for the first cell of `pack` whose first choice (node 1) never answers, ended by the
/// stall retry unless the engine hedges.
async fn first_cell_with_silent_first_choice(rig: &Rig, pack: &Pack) -> WaitOutcome {
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    wait_beside(
        rig,
        pack,
        pack.range(0, MIB),
        pass_trigger_then_first_byte_timeout(rig),
    )
    .await
}

/// A demand part with no first byte is hedged to the other hot replica once the 30 ms trigger has
/// passed (not at 29 ms), and the waiter is served from the hedge's bytes; no stall retry runs.
#[tokio::test]
async fn a_demand_with_no_first_byte_is_hedged_to_the_other_hot_replica_at_the_trigger() {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        rig.clock.advance(29);
        let before = rig.engine.stats();
        assert_eq!(before.gets(GetKind::Hedge), 0);
        assert_eq!(before.armed(TimerKind::Hedge), 1);
        rig.clock.advance(1);
        rig.until(|s| s.gets(GetKind::Hedge) == 1).await;
    };
    let outcome = wait_beside(&rig, &pack, pack.range(0, MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 1);
    assert_eq!(stats.gets(GetKind::DemandSub), 1);
    assert_eq!(stats.gets(GetKind::Retry), 0);
    let gets = rig.upstream.gets();
    assert_eq!(
        gets.iter().map(|g| g.source.device).collect::<Vec<_>>(),
        vec![hot_dev(1), hot_dev(2)]
    );
    assert_eq!(gets[0].range, gets[1].range);
    assert_eq!(
        rig.entry_bytes(&pack, 0, MIB),
        pack.bytes[..MIB as usize].to_vec()
    );
}

/// A first byte that arrives before the trigger means the part is being served: nothing is hedged
/// when the trigger passes, and the part goes on by the stall retry once its body stops.
#[tokio::test]
async fn a_first_byte_before_the_trigger_cancels_the_hedge() {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    rig.upstream
        .script(hot_dev(1), Script::StallAfter(MIB as usize / 2));
    let driver = async {
        rig.until(|s| {
            s.armed(TimerKind::Hedge) == 1
                && s.armed(TimerKind::Stall) == 1
                && s.bytes_fetched == MIB / 2
        })
        .await;
        rig.clock.advance(30);
        rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
        rig.clock.advance(470);
    };
    let outcome = wait_beside(&rig, &pack, pack.range(0, MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(stats.stall_retries, 1);
    assert_eq!(stats.bytes_wasted, MIB / 2);
}

/// A part fetched for `prepare` is never hedged: no hedge trigger is armed, and no hedge GET is
/// made when the clock goes far past 30 ms; it goes on by the stall retry.
#[tokio::test]
async fn a_prepare_part_is_never_hedged() {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    rig.engine.ensure(
        &pack.spec,
        &demand_cell(0),
        Class::Prepare,
        JOB,
        Rank::default(),
    );
    rig.until(|s| s.total_gets() == 1 && s.armed(TimerKind::FirstByte) == 1)
        .await;
    assert_eq!(rig.engine.stats().armed(TimerKind::Hedge), 0);
    rig.clock.advance(30);
    rig.clock.advance(1_970);
    let stats = rig.until(|s| s.cells_published == 1).await;
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(stats.stall_retries, 1);
    assert_eq!(rig.upstream.gets().len(), 2);
    assert_eq!(
        rig.entry_bytes(&pack, 0, MIB),
        pack.bytes[..MIB as usize].to_vec()
    );
}

/// A demand part of a pack with one copy has no alternative to hedge to: nothing is hedged and the
/// stall retry asks the same copy again.
#[tokio::test]
async fn a_demand_with_only_one_copy_is_never_hedged() {
    let rig = Rig::new(8, EngineConfig::default());
    let pack = Pack::new("hot/single", 4 * MIB, 6);
    rig.upstream.add(
        &pack,
        location(Some(generation(1)), 4 * MIB, vec![hot(1, 4 * MIB)]),
    );
    let outcome = first_cell_with_silent_first_choice(&rig, &pack).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
    assert_eq!(
        rig.upstream
            .gets()
            .iter()
            .map(|g| g.source.device)
            .collect::<Vec<_>>(),
        vec![hot_dev(1), hot_dev(1)]
    );
}

/// A hot replica that advertises a longer queue wait than the stalled device is not a hedge
/// target (the hedge goes only to a copy not more loaded than the current one).
#[tokio::test]
async fn a_hot_alternative_with_a_longer_advertised_queue_wait_is_not_used() {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    rig.engine.observe_signals(hot_dev(2), queue_wait(50));
    let outcome = first_cell_with_silent_first_choice(&rig, &pack).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
}

/// Once the stalled device's advertised queue wait has grown past the other replica's, the other
/// replica is a hedge target.
#[tokio::test]
async fn a_hot_alternative_with_a_shorter_advertised_queue_wait_than_the_stalled_device_is_used() {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        rig.engine.observe_signals(hot_dev(1), queue_wait(100));
        rig.engine.observe_signals(hot_dev(2), queue_wait(40));
        rig.clock.advance(30);
        rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
        rig.clock.advance(1_970);
    };
    let outcome = wait_beside(&rig, &pack, pack.range(0, MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 1);
    assert_eq!(stats.gets(GetKind::Retry), 0);
}

/// A replica more loaded than the stalled device stays unused even when both advertise a wait.
#[tokio::test]
async fn a_hot_alternative_more_loaded_than_the_stalled_device_is_not_used_when_both_advertise_a_wait()
 {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        rig.engine.observe_signals(hot_dev(1), queue_wait(100));
        rig.engine.observe_signals(hot_dev(2), queue_wait(200));
        rig.clock.advance(30);
        rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
        rig.clock.advance(1_970);
    };
    let outcome = wait_beside(&rig, &pack, pack.range(0, MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
}

/// Pack `a` (both replicas) is silent on node 1 while pack `b`, which only `busy_node` holds,
/// is answered 503 by it; the hedge of `a` finds one of its two devices in a decrease state and
/// does not happen. Returns `a`'s wait outcome.
async fn one_device_answers_503_while_a_part_is_silent(busy_node: u8) -> (Rig, WaitOutcome) {
    let rig = Rig::new(8, EngineConfig::default());
    let a = Pack::new("hot/a", MIB, 1);
    let b = Pack::new("hot/b", MIB, 2);
    rig.upstream.add(&a, replicas(MIB));
    rig.upstream.add(
        &b,
        location(Some(generation(1)), MIB, vec![hot(busy_node, MIB)]),
    );
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    rig.upstream
        .script(hot_dev(busy_node), unavailable(&b, busy_node));
    let driver = async {
        rig.until(|s| s.armed(TimerKind::FirstByte) == 1).await;
        rig.engine.ensure(
            &b.spec,
            &demand_cell(0),
            Class::Demand,
            JOB,
            Rank::default(),
        );
        rig.until(|s| s.armed(TimerKind::Backoff) == 1).await;
        rig.until(|s| s.armed(TimerKind::Hedge) == 2).await;
        rig.clock.advance(30);
        rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
        rig.clock.advance(1_970);
    };
    let outcome = wait_beside(&rig, &a, a.range(0, MIB), driver).await;
    (rig, outcome)
}

/// A demand whose current device answered 503 within the busy window is not hedged (the device is
/// in a decrease state), even though the other replica is idle.
#[tokio::test]
async fn a_demand_whose_device_answered_503_in_the_busy_window_is_not_hedged() {
    let (rig, outcome) = one_device_answers_503_while_a_part_is_silent(1).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.stall_retries, 1);
}

/// A hedge never goes to a replica that answered 503 within the busy window.
#[tokio::test]
async fn a_hot_alternative_that_answered_503_in_the_busy_window_is_not_a_hedge_target() {
    let (rig, outcome) = one_device_answers_503_while_a_part_is_silent(2).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.stall_retries, 1);
}

/// Two demand parts on two packs stall at once on node 1: only one hedge GET is made (one hedge
/// permit per node); the other part goes on by its stall retry; both waits end Ready.
#[tokio::test]
async fn two_stalled_demand_parts_at_once_make_exactly_one_hedge_get() {
    let rig = Rig::new(8, EngineConfig::default());
    let a = Pack::new("hot/a", MIB, 1);
    let b = Pack::new("hot/b", MIB, 2);
    rig.upstream.add(&a, replicas(MIB));
    rig.upstream.add(&b, replicas(MIB));
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    let gate = Arc::new(Semaphore::new(0));
    for _ in 0..3 {
        rig.upstream.script(hot_dev(2), Script::Gated(gate.clone()));
    }
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 2 && s.armed(TimerKind::FirstByte) == 2)
            .await;
        rig.clock.advance(30);
        rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
        rig.clock.advance(1_970);
        let stats = rig
            .until(|s| s.stall_retries + s.gets(GetKind::Hedge) >= 2)
            .await;
        assert_eq!(stats.gets(GetKind::Hedge), 1);
        assert_eq!(stats.stall_retries, 1);
        gate.add_permits(3);
    };
    let (oa, ob, ()) = tokio::join!(
        rig.engine.wait(&a.spec, a.range(0, MIB), JOB),
        rig.engine.wait(&b.spec, b.range(0, MIB), JOB),
        driver
    );
    assert_eq!((oa, ob), (WaitOutcome::Ready, WaitOutcome::Ready));
    assert_eq!(rig.engine.stats().gets(GetKind::Hedge), 1);
    assert_eq!(rig.entry_bytes(&a, 0, MIB), a.bytes.to_vec());
    assert_eq!(rig.entry_bytes(&b, 0, MIB), b.bytes.to_vec());
}

/// The hedge permit is given back when its hedge ends: a later demand that stalls is hedged again
/// (two hedge GETs in all).
#[tokio::test]
async fn a_hedge_permit_is_released_when_its_hedge_ends_so_a_later_stall_can_hedge_again() {
    let rig = Rig::new(8, EngineConfig::default());
    let a = Pack::new("hot/a", MIB, 1);
    let b = Pack::new("hot/b", MIB, 2);
    rig.upstream.add(&a, replicas(MIB));
    rig.upstream.add(&b, replicas(MIB));
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    rig.upstream.script(hot_dev(1), Script::NoAnswer);
    let first = wait_beside(
        &rig,
        &a,
        a.range(0, MIB),
        pass_trigger_then_first_byte_timeout(&rig),
    )
    .await;
    assert_eq!(first, WaitOutcome::Ready);
    assert_eq!(rig.engine.stats().gets(GetKind::Hedge), 1);
    let second = wait_beside(
        &rig,
        &b,
        b.range(0, MIB),
        pass_trigger_then_first_byte_timeout(&rig),
    )
    .await;
    assert_eq!(second, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 2);
    assert_eq!(stats.gets(GetKind::Retry), 0);
}

/// Hedge and original both deliver the same cells: each cell is published once (the loser's copy
/// is never written), the entry holds the pack's bytes, and nothing delivered is lost from the
/// byte accounting.
#[tokio::test]
async fn a_hedge_and_its_original_that_both_deliver_publish_each_cell_once() {
    let (rig, pack) = two_replicas(4 * MIB, EngineConfig::default());
    let original = Arc::new(Semaphore::new(0));
    let hedge = Arc::new(Semaphore::new(0));
    rig.upstream
        .script(hot_dev(1), Script::Gated(original.clone()));
    rig.upstream
        .script(hot_dev(2), Script::Gated(hedge.clone()));
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        rig.clock.advance(30);
        rig.until(|s| s.gets(GetKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 2)
            .await;
        original.add_permits(1);
        hedge.add_permits(1);
    };
    let outcome = wait_beside(&rig, &pack, pack.range(0, 2 * MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.until(|s| s.cells_published >= 2).await;
    assert_eq!(stats.cells_published, 2);
    assert_eq!(stats.bytes_published, 2 * MIB);
    assert!(stats.bytes_published + stats.bytes_wasted <= stats.bytes_fetched);
    assert_eq!(stats.gets(GetKind::Hedge), 1);
    assert_eq!(
        rig.entry_bytes(&pack, 0, 2 * MIB),
        pack.bytes[..2 * MIB as usize].to_vec()
    );
}

/// A cold pack (one drive) with a degraded read from drives 2 and 3, of `size` bytes.
fn cold_with_degraded_read(size: u64, config: EngineConfig) -> (Rig, Pack, [DeviceRef; 2]) {
    let rig = Rig::new(8, config);
    let pack = Pack::new("cold/pack", size, 3);
    let sources = [drive(1, 2), drive(1, 3)];
    rig.upstream.add(
        &pack,
        location(
            Some(generation(1)),
            size,
            vec![cold(1, 1, 0, size), degraded(2, &sources, 0, size)],
        ),
    );
    rig.upstream.script(drive(1, 1), Script::NoAnswer);
    (rig, pack, sources)
}

/// A cold demand range of exactly 4 MiB whose `k = 2` source drives advertise an empty queue is
/// hedged by a degraded read, charged `k` parts on those drives; the waiter is served by it.
#[tokio::test]
async fn a_cold_range_of_4_mib_is_hedged_by_a_degraded_read_when_its_source_drives_are_idle() {
    let (rig, pack, sources) = cold_with_degraded_read(4 * MIB, EngineConfig::default());
    rig.engine.observe_signals(sources[0], drive_queue(0));
    rig.engine.observe_signals(sources[1], drive_queue(0));
    let driver = async {
        rig.until(|s| s.armed(TimerKind::Hedge) == 1 && s.armed(TimerKind::FirstByte) == 1)
            .await;
        rig.clock.advance(30);
        rig.until(|s| s.armed(TimerKind::Hedge) == 0).await;
        rig.clock.advance(1_970);
    };
    let outcome = wait_beside(&rig, &pack, pack.range(0, 4 * MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 1);
    assert_eq!(stats.gets(GetKind::Retry), 0);
    let charges = rig.budget.charges();
    assert_eq!(
        charges.iter().map(|c| c.parts).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(charges[1].devices, sources.to_vec());
    assert_eq!(charges[1].class, Class::Demand);
    assert_eq!(rig.entry_bytes(&pack, 0, 4 * MIB), pack.bytes.to_vec());
}

/// A cold demand range longer than `degraded_hedge_max` (5 MiB against 4 MiB) is not hedged, even
/// with every source drive idle; it goes on by the stall retry.
#[tokio::test]
async fn a_cold_range_over_the_degraded_hedge_limit_is_not_hedged() {
    let config = EngineConfig {
        first_piece_max_cells: 5,
        ..EngineConfig::default()
    };
    let (rig, pack, sources) = cold_with_degraded_read(5 * MIB, config);
    rig.engine.observe_signals(sources[0], drive_queue(0));
    rig.engine.observe_signals(sources[1], drive_queue(0));
    let driver = pass_trigger_then_first_byte_timeout(&rig);
    let outcome = wait_beside(&rig, &pack, pack.range(0, 5 * MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::GridPart), 1);
    assert_eq!(stats.gets(GetKind::Retry), 1);
}

/// A source drive whose queue is not known (no response or ping has said) keeps the degraded read
/// from being a hedge.
#[tokio::test]
async fn a_degraded_hedge_needs_every_source_drive_queue_to_be_known() {
    let (rig, pack, sources) = cold_with_degraded_read(4 * MIB, EngineConfig::default());
    rig.engine.observe_signals(sources[0], drive_queue(0));
    let driver = pass_trigger_then_first_byte_timeout(&rig);
    let outcome = wait_beside(&rig, &pack, pack.range(0, 4 * MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
}

/// A source drive with a queued request keeps the degraded read from being a hedge.
#[tokio::test]
async fn a_degraded_hedge_needs_every_source_drive_queue_to_be_empty() {
    let (rig, pack, sources) = cold_with_degraded_read(4 * MIB, EngineConfig::default());
    rig.engine.observe_signals(sources[0], drive_queue(0));
    rig.engine.observe_signals(sources[1], drive_queue(2));
    let driver = pass_trigger_then_first_byte_timeout(&rig);
    let outcome = wait_beside(&rig, &pack, pack.range(0, 4 * MIB), driver).await;
    assert_eq!(outcome, WaitOutcome::Ready);
    let stats = rig.engine.stats();
    assert_eq!(stats.gets(GetKind::Hedge), 0);
    assert_eq!(stats.gets(GetKind::Retry), 1);
}
