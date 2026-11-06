//! The runs of the explorer. Each exploration names its actors, the state it starts from, and the
//! bound that keeps it inside the test budget; a rule broken on any schedule fails the test with
//! that schedule. The two fixed schedules at the end are the counterexamples of the design's
//! model, played as the exact order of store calls that made them.
//!
//! Every test runs on a paused tokio clock, so the delete's waits pass at once.

use tierstore::{EntryOp, EntryState, Op, Phase};

use super::check::{Event, Kind};
use super::search::{exhaust, sample};
use super::world::{ClientOp, Scenario, Seed, Side, Space, World};

const OLD: &str = "bytes0";
const NEW: &str = "bytes1";
const OTHER: &str = "bytes2";

/// A hot budget no run comes near: nothing is evicted.
const ROOMY: u64 = 1 << 20;
/// A hot budget of 7 bytes: a single 6-byte object is above its high water, so the reconciler
/// evicts a replicated object in the step it measures it.
const TIGHT: u64 = 7;

const DELETE: ClientOp = ClientOp::Delete;
const GET: ClientOp = ClientOp::Get;

fn run(seed: Seed, budget: u64, clients: Vec<Vec<ClientOp>>, reconcile: usize) -> Scenario {
    Scenario {
        seed,
        budget,
        clients,
        reconcile,
    }
}

/// Runs every schedule within `bound` preemptions. `at_least` is the number the exploration ran when
/// it was written: fewer means the actors no longer reach as many store calls, and a run that
/// visits less cannot pass for a clean one.
async fn explored(scenario: Scenario, bound: u32, at_least: usize) {
    let schedules = exhaust(&scenario, bound).await;
    assert!(
        schedules >= at_least,
        "{schedules} schedules ran, expected at least {at_least}"
    );
}

/// Asserts that a delete followed by a get, against a put the reconciler is copying, breaks no
/// rule in any schedule of up to 3 preemptions: the get after the acknowledged delete answers not
/// found, whether the copy published before, during or after the delete.
#[tokio::test(start_paused = true)]
async fn a_delete_then_a_get_against_a_copy_in_flight_break_no_rule() {
    let scenario = run(Seed::Pending(OLD), ROOMY, vec![vec![DELETE, GET]], 2);
    explored(scenario, 3, 1089).await;
}

/// Asserts that two puts of one key and a reconciler break no rule in any schedule of up to 2
/// preemptions: at most one is acknowledged, and the acknowledged one is in both tiers at the end.
#[tokio::test(start_paused = true)]
async fn two_puts_of_one_key_against_the_reconciler_break_no_rule() {
    let scenario = run(
        Seed::Empty,
        ROOMY,
        vec![vec![ClientOp::Put(NEW)], vec![ClientOp::Put(OTHER)]],
        2,
    );
    explored(scenario, 2, 566).await;
}

/// Asserts that a put, a delete followed by a get, and the reconciler break no rule in any
/// schedule of up to 2 preemptions: the delete either covers the put or is refused while it is in
/// flight, and what is left agrees with what was acknowledged.
#[tokio::test(start_paused = true)]
async fn a_put_against_a_delete_and_a_get_break_no_rule() {
    let scenario = run(
        Seed::Empty,
        ROOMY,
        vec![vec![ClientOp::Put(NEW)], vec![DELETE, GET]],
        2,
    );
    explored(scenario, 2, 847).await;
}

/// Asserts that a get of a cold-only object (which asks for a copy-back) against a delete and the
/// reconciler breaks no rule in any schedule of up to 2 preemptions: a copy-back never brings back
/// what an acknowledged delete took.
#[tokio::test(start_paused = true)]
async fn a_copy_back_against_a_delete_breaks_no_rule() {
    let scenario = run(Seed::ColdOnly(OLD), ROOMY, vec![vec![GET], vec![DELETE]], 2);
    explored(scenario, 2, 354).await;
}

/// Asserts that a copy-back, a delete followed by a put of new bytes and a get, and the reconciler
/// break no rule in any schedule of up to 2 preemptions: the new put is acknowledged, held in both
/// tiers at the end, and never overwritten by the copy-back of the old bytes.
#[tokio::test(start_paused = true)]
async fn a_copy_back_against_a_delete_and_a_new_put_breaks_no_rule() {
    let scenario = run(
        Seed::ColdOnly(OLD),
        ROOMY,
        vec![vec![GET], vec![DELETE, ClientOp::Put(NEW), GET]],
        3,
    );
    explored(scenario, 2, 1157).await;
}

/// Asserts that two deletes of a put the reconciler is copying, the second followed by a get,
/// break no rule in any schedule of up to 2 preemptions: a delete that claims the entry another
/// delete handed over waits for the reconciler too.
#[tokio::test(start_paused = true)]
async fn two_deletes_against_a_copy_in_flight_break_no_rule() {
    let scenario = run(
        Seed::Pending(OLD),
        ROOMY,
        vec![vec![DELETE], vec![DELETE, GET]],
        2,
    );
    explored(scenario, 2, 2925).await;
}

/// Asserts that the eviction of a replicated object against a delete and a re-put of identical
/// bytes breaks no rule in any schedule of up to 3 preemptions: the eviction takes its entry
/// create-only and looks at both tags again under it, so it never removes the new object, which
/// is acknowledged and read back.
#[tokio::test(start_paused = true)]
async fn an_eviction_against_a_delete_and_an_identical_put_breaks_no_rule() {
    let scenario = run(
        Seed::Replicated(OLD),
        TIGHT,
        vec![vec![DELETE, ClientOp::Put(OLD), GET]],
        2,
    );
    explored(scenario, 3, 776).await;
}

/// Asserts that the eviction of a replicated object against a get and a delete breaks no rule in
/// any schedule of up to 2 preemptions: a get that misses the hot tier because the eviction took
/// the object reads the cold tier and answers the bytes, or not found after the delete.
#[tokio::test(start_paused = true)]
async fn an_eviction_against_a_get_and_a_delete_breaks_no_rule() {
    let scenario = run(
        Seed::Replicated(OLD),
        TIGHT,
        vec![vec![GET], vec![DELETE]],
        2,
    );
    explored(scenario, 2, 515).await;
}

/// Asserts that 200 walks of seed 1 over four actors (a delete and a get, a delete, a get, the
/// reconciler) break no rule: more actors than an exhaustive search of them could visit, sampled
/// by a seeded generator.
#[tokio::test(start_paused = true)]
async fn seeded_walks_over_four_actors_break_no_rule() {
    let scenario = run(
        Seed::Pending(OLD),
        ROOMY,
        vec![vec![DELETE, GET], vec![DELETE], vec![GET]],
        3,
    );
    let walks = sample(&scenario, 1, 200).await;
    assert_eq!(walks, 200);
}

fn done(who: usize, kind: Kind) -> Event {
    Event { who, kind }
}

/// Asserts the first counterexample of the model: the reconciler holds the put and is about to
/// publish its copy to the cold tier when a delete amends the entry, removes both tiers and hands
/// the entry over. The copy then publishes. The delete is not acknowledged while the cold tier
/// holds the copy; it is acknowledged after the reconciler has finished the delete, and the get
/// that follows it answers not found.
#[tokio::test(start_paused = true)]
async fn a_delete_that_amends_a_copy_in_flight_is_acknowledged_only_after_the_release() {
    let scenario = run(Seed::Pending(OLD), ROOMY, vec![vec![DELETE, GET]], 2);
    let mut world = World::start(&scenario).await;
    let (deleter, leader) = (0, world.reconciler());
    world
        .run_until_parked(leader, Side::Cold, Op::Put, Space::Data)
        .await;
    let held = Some((EntryOp::Put, EntryState::Held, Some("r1".to_owned())));
    assert_eq!(world.shape().await, held);

    let handed_over = Some((EntryOp::Del, EntryState::Pending, None));
    while world.shape().await != handed_over {
        world.advance(deleter).await;
    }
    assert!(!world.hot_has().await);
    assert!(!world.ended(deleter));

    world
        .run_until_parked(leader, Side::Cold, Op::Delete, Space::Data)
        .await;
    assert!(
        world.cold_has().await,
        "the copy published after the delete"
    );
    assert!(
        !world.ended(deleter),
        "the delete was acknowledged with the copy in the cold tier"
    );

    world.run_to_end(leader).await;
    world.run_to_end(deleter).await;
    let history = world.history();
    assert!(history.contains(&done(0, Kind::DeleteDone { acked: true })));
    assert!(history.contains(&done(0, Kind::GetDone { got: None })));
    assert_eq!(world.verdict().await, Vec::<String>::new());
}

/// Asserts the second counterexample of the model: the reconciler holds the put; a delete amends
/// it, its cold delete lands and its answer is lost, so it hands the entry over and answers
/// unavailable; the reconciler is still copying when the retry claims the handed-over entry and
/// deletes both tiers. The retry is not acknowledged while the cold tier holds the copy the
/// reconciler then publishes; it is acknowledged after the reconciler has finished the delete, and
/// the get that follows answers not found.
#[tokio::test(start_paused = true)]
async fn a_retried_delete_that_claims_a_handed_over_entry_waits_for_the_release() {
    let scenario = run(
        Seed::Pending(OLD),
        ROOMY,
        vec![vec![DELETE], vec![DELETE, GET]],
        2,
    );
    let mut world = World::start(&scenario).await;
    let (first, retry, leader) = (0, 1, world.reconciler());
    world
        .run_until_parked(leader, Side::Cold, Op::Put, Space::Data)
        .await;
    world.hold_after(first, Side::Cold, Op::Delete, Space::Data);
    while matches!(world.parked_at(first), Some((_, _, _, Phase::Pre))) {
        world.advance(first).await;
    }
    world.advance_failing(first).await;
    world.run_to_end(first).await;
    let handed_over = Some((EntryOp::Del, EntryState::Pending, None));
    assert_eq!(world.shape().await, handed_over);
    let history = world.history();
    assert!(history.contains(&done(0, Kind::DeleteDone { acked: false })));

    while world.hot_has().await || world.shape().await != handed_over {
        world.advance(retry).await;
    }
    assert!(!world.ended(retry));

    world
        .run_until_parked(leader, Side::Cold, Op::Delete, Space::Data)
        .await;
    assert!(
        world.cold_has().await,
        "the copy published after the retry's deletes"
    );
    assert!(
        !world.ended(retry),
        "the retry was acknowledged with the copy in the cold tier"
    );

    world.run_to_end(leader).await;
    world.run_to_end(retry).await;
    let history = world.history();
    assert!(history.contains(&done(1, Kind::DeleteDone { acked: true })));
    assert!(history.contains(&done(1, Kind::GetDone { got: None })));
    assert_eq!(world.verdict().await, Vec::<String>::new());
}
