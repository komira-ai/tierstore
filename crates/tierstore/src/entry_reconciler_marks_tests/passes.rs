//! The passes the leader's step runs after the copy (`entry_reconciler/passes.rs`): the copy-back
//! of the `WANT` markers, the stray sweep and the eviction, the records the copier leaves for the
//! eviction, and the lease that passes from the entries to the markers and back with no second
//! holder. No test sleeps or reads a clock: the monotonic and the wall clock are moved by hand.

use super::*;
use crate::journal::LEASE_KEY;
use crate::{LeaderLease, Phase as GatePhase};

/// Thirty bytes made of `fill`.
fn thirty(fill: &str) -> Bytes {
    Bytes::from(fill.repeat(30))
}

/// A hot tier budgeted at 100 bytes (high water 80, low water 70), one item at a time so that the
/// order of the calls is the order of the candidates, and a cold copy that counts as old at once.
fn tight(config: &mut EntryReconcilerConfig) {
    config.base.hot_budget = 100;
    config.base.parallel = 1;
    config.evict_min_age = Duration::ZERO;
}

/// As [`tight`], with a cold copy that must be ten seconds old.
fn tight_and_slow(config: &mut EntryReconcilerConfig) {
    tight(config);
    config.evict_min_age = Duration::from_secs(10);
}

/// Puts `a`, `b` and `c` in the hot tier, thirty bytes each, `a` the oldest, and the same bytes in
/// the cold tier under the keys of `cold`.
async fn crowded(rig: &Rig, cold: &[&str]) {
    for key in ["a", "b", "c"] {
        rig.hot.put(key, thirty(key)).await.unwrap();
        rig.clock.advance(1000);
    }
    for key in cold {
        rig.cold.put(key, thirty(key)).await.unwrap();
    }
}

fn want(key: &str) -> Marker {
    marker(key, MarkerState::Want, false, false)
}

/// A step that has done `evicted` evictions, `changed` changes in all.
fn evicting(evicted: usize, changed: usize, pinned: usize) -> StepReport {
    StepReport {
        evicted,
        ..report(changed, pinned)
    }
}

/// Asserts that above the high water the oldest hot objects that the cold tier holds are evicted
/// down to the low water by the step itself, with no other caller: of three objects of thirty
/// bytes in a budget of a hundred the oldest goes, the others stay, the cold tier keeps its copy,
/// no marker is left, and the usage published is the sixty bytes that remain.
#[tokio::test]
async fn a_step_under_active_marks_evicts_the_oldest_objects_down_to_the_low_water() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    let mut leader = rig.leader_with("r1", tight);
    assert_eq!(leader.step().await, evicting(1, 1, 0));
    assert!(rig.hot_body("a").await.is_none());
    assert!(rig.hot_body("b").await.is_some());
    assert!(rig.hot_body("c").await.is_some());
    assert_eq!(rig.cold_body("a").await, Some(thirty("a")));
    assert_eq!(rig.shape("a").await, None);
    assert_eq!(rig.usage().await.hot_bytes, 60);
    let step = leader.step().await;
    assert_eq!(step, report(0, 0));
}

/// Asserts that the eviction leaves a key that has a marker of a live owner (nothing is destroyed
/// under another actor's lock) and goes on to the next candidate: `a` keeps its lock and its
/// bytes, `b` is evicted in its place.
#[tokio::test]
async fn a_key_under_a_live_owners_lock_is_not_evicted_and_the_next_candidate_is() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    rig.register("p1").await;
    rig.plant(lock("a", MarkerWhy::Overwrite, "p1", false))
        .await;
    let mut leader = rig.leader_with("r1", tight);
    assert_eq!(leader.step().await, evicting(1, 1, 1));
    assert!(rig.hot_body("a").await.is_some());
    assert!(rig.hot_body("b").await.is_none());
    assert_eq!(rig.shape("a").await, Some((MarkerState::Lock, false, true)));
}

/// Asserts that a key whose cold copy has other bytes is not evicted but marked `DIRTY` for the
/// copier (hot is the only place a newer version can be), the next candidate is evicted instead,
/// and the next step copies the key and clears the marker.
#[tokio::test]
async fn an_object_whose_cold_copy_differs_is_marked_dirty_and_copied_not_evicted() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["b", "c"]).await;
    rig.cold.put("a", thirty("x")).await.unwrap();
    let mut leader = rig.leader_with("r1", tight);
    assert_eq!(leader.step().await, evicting(1, 2, 0));
    assert!(rig.hot_body("a").await.is_some());
    assert!(rig.hot_body("b").await.is_none());
    assert_eq!(
        rig.shape("a").await,
        Some((MarkerState::Dirty, false, true))
    );
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("a").await, Some(thirty("a")));
    assert_eq!(rig.shape("a").await, None);
}

/// Asserts that the copier leaves the pair of tags and the time of its cold write for the
/// eviction: the key it copied is "young" until `evict_min_age` has passed on the leader's clock
/// (and so are the others, which this leader never saw copied, counted from its takeover), the
/// step that finds nothing to evict asks for the next listing in `evict_min_age` and not in a day,
/// and when it comes the copied key is evicted with no read of its bytes in either tier beyond
/// the copier's own one.
#[tokio::test]
async fn the_eviction_skips_the_compare_for_a_key_the_copier_copied_and_waits_out_a_young_copy() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["b", "c"]).await;
    rig.plant(dirty("a")).await;
    let mut leader = rig.leader_with("r1", tight_and_slow);

    let first = leader.step().await;
    assert_eq!(
        first,
        StepReport {
            evicting: true,
            ..report(1, 0)
        }
    );
    assert!(rig.hot_body("a").await.is_some());
    let side = leader.marks.clone().unwrap();
    let hot_tag = rig.hot.head("a").await.unwrap().tag;
    let cold_tag = rig.cold.head("a").await.unwrap().tag;
    assert_eq!(side.records.copied_at("a", &hot_tag, &cold_tag), Some(0));

    rig.pass(Duration::from_secs(5));
    let waiting = leader.step().await;
    assert_eq!(
        waiting,
        StepReport {
            evicting: true,
            ..report(0, 0)
        }
    );
    assert!(rig.hot_body("a").await.is_some());

    rig.pass(Duration::from_secs(5));
    assert_eq!(leader.step().await, evicting(1, 1, 0));
    assert!(rig.hot_body("a").await.is_none());
    assert_eq!(rig.cold_body("a").await, Some(thirty("a")));
    assert_eq!(rig.cold_calls.calls_on(Op::Get, "a"), 0);
    assert_eq!(rig.hot_calls.calls_on(Op::Get, "a"), 1);
}

/// Asserts the same for a key the copier did not have to copy because both tiers hold it under
/// one plain MD5 tag: the pair is recorded as copied at the takeover, so that eviction does not
/// compare bytes the tags already say are equal.
#[tokio::test]
async fn a_copy_found_equal_by_its_tags_is_recorded_at_the_takeover_and_not_compared() {
    let rig = Rig::with_md5_tags();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    rig.plant(dirty("a")).await;
    let mut leader = rig.leader_with("r1", tight_and_slow);
    let first = leader.step().await;
    assert_eq!(first.changed, 1);
    assert_eq!(rig.cold_calls.calls(Op::Put), 0);
    assert!(rig.hot_body("a").await.is_some());
    rig.pass(Duration::from_secs(10));
    assert_eq!(leader.step().await, evicting(1, 1, 0));
    assert!(rig.hot_body("a").await.is_none());
    assert_eq!(rig.cold_calls.calls_on(Op::Get, "a"), 0);
}

/// Asserts that a failing call of an eviction is reported, stops the batches of the step and
/// leaves the key first among the candidates, so that the next step evicts it.
#[tokio::test]
async fn an_eviction_that_fails_is_reported_and_tried_again_by_the_next_step() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    rig.hot_calls
        .fail_next_on(Op::Head, "a", Error::Unavailable("down".to_owned()));
    let mut leader = rig.leader_with("r1", tight);
    let first = leader.step().await;
    assert_eq!(first.failed, vec![Error::Unavailable("down".to_owned())]);
    assert_eq!((first.evicted, first.evicting), (0, true));
    assert!(rig.hot_body("a").await.is_some());
    assert_eq!(leader.step().await, evicting(1, 1, 0));
    assert!(rig.hot_body("a").await.is_none());
}

/// Asserts that an eviction whose read of the format fails reports it for the key and keeps the
/// candidate, and that one that finds the format no longer opening the markers to the leader does
/// nothing and keeps all its candidates.
#[tokio::test]
async fn the_eviction_keeps_its_candidates_when_the_format_cannot_be_read_or_has_moved() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    let mut leader = rig.leader_with("r1", |_| ());
    leader.step().await;
    leader.config.base.hot_budget = 100;
    leader.config.base.parallel = 1;
    let side = leader.marks.clone().unwrap();
    assert_eq!(leader.books.candidates.len(), 3);

    let mut done = StepReport::default();
    rig.hot_calls
        .fail_next_on(Op::Get, FORMAT_KEY, Error::Unavailable("down".to_owned()));
    leader.mark_evict(&side, 0, &mut done).await;
    assert_eq!(done.failed, vec![Error::Unavailable("down".to_owned())]);
    assert_eq!(leader.books.candidates.len(), 3);

    let mut done = StepReport::default();
    rig.format(FormatPhase::Drain, Some(BUCKET)).await;
    leader.mark_evict(&side, 0, &mut done).await;
    assert_eq!(done.failed, Vec::new());
    assert_eq!((done.evicted, done.changed), (0, 0));
    assert_eq!(leader.books.candidates.len(), 3);
    assert!(rig.hot_body("a").await.is_some());
}

/// Asserts that a `WANT` marker is copied back in the step that finds it: the cold object lands in
/// the hot tier, the marker is gone, and the step counts one change.
#[tokio::test]
async fn a_want_marker_is_copied_back_into_the_hot_tier_by_the_step() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("from cold")).await.unwrap();
    rig.plant(want("k")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.hot_body("k").await, Some(bytes("from cold")));
    assert_eq!(rig.cold_body("k").await, Some(bytes("from cold")));
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a `WANT` marker for a key the hot tier has already is cleared with nothing
/// copied, and that one for a key cold lacks is cleared too.
#[tokio::test]
async fn a_want_marker_with_nothing_to_copy_is_cleared() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("held", bytes("hot")).await.unwrap();
    rig.cold.put("held", bytes("cold")).await.unwrap();
    rig.plant(want("held")).await;
    rig.plant(want("lost")).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(2, 0));
    assert_eq!(rig.hot_body("held").await, Some(bytes("hot")));
    assert!(rig.hot_body("lost").await.is_none());
    assert_eq!(rig.shape("held").await, None);
    assert_eq!(rig.shape("lost").await, None);
}

/// Asserts the bounds of the settings reach the copy-back: with `copy_back_max` of one the oldest
/// ask is copied and the other dropped (its marker removed, nothing copied); with a `copy_back_ttl`
/// of zero every ask is dropped; an object above `copy_back_max_bytes` is dropped.
#[tokio::test]
async fn the_copy_back_settings_drop_the_asks_beyond_their_bounds() {
    type Tweak = fn(&mut EntryReconcilerConfig);
    let cases: [(Tweak, [bool; 2]); 3] = [
        (|config| config.copy_back_max = 1, [true, false]),
        (
            |config| config.copy_back_ttl = Duration::ZERO,
            [false, false],
        ),
        (|config| config.copy_back_max_bytes = 4, [false, false]),
    ];
    for (tweak, landed) in cases {
        let rig = Rig::new();
        rig.activate().await;
        for key in ["x", "y"] {
            rig.cold.put(key, bytes("12345")).await.unwrap();
            rig.plant(want(key)).await;
        }
        let mut leader = rig.leader_with("r1", tweak);
        let done = leader.step().await;
        assert_eq!(done.changed, 2);
        for (key, landed) in ["x", "y"].into_iter().zip(landed) {
            assert_eq!(rig.hot_body(key).await.is_some(), landed, "{key}");
            assert_eq!(rig.shape(key).await, None, "{key}");
        }
    }
}

/// Asserts that a copy-back that fails (the cold tier is down) is reported, and that its ask is
/// released and removed: a later read asks again, and the step after it does nothing.
#[tokio::test]
async fn a_copy_back_that_fails_is_reported_and_its_ask_is_released() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    rig.plant(want("k")).await;
    rig.cold_calls.set_down(true);
    let mut leader = rig.leader("r1");
    let done = leader.step().await;
    assert_eq!((done.failed.len(), done.changed), (1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert!(rig.hot_body("k").await.is_none());
    rig.cold_calls.set_down(false);
    assert_eq!(leader.step().await, report(0, 0));
}

/// Asserts that the stray sweep runs in the step at the takeover and then every `stray_interval`:
/// an old hot object with no marker and no cold copy gets a `DIRTY` marker in the first step (and
/// is copied by the next), another that turns up old afterwards waits for the interval, and is
/// adopted by the step after it.
#[tokio::test]
async fn the_stray_sweep_runs_at_the_takeover_and_every_stray_interval() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("s", bytes("stray")).await.unwrap();
    rig.clock.advance(700_000);
    let mut leader = rig.leader_with("r1", |config| {
        config.stray_interval = Duration::from_secs(100);
    });
    let first = leader.step().await;
    assert_eq!(
        first,
        StepReport {
            strays_adopted: 1,
            ..report(0, 0)
        }
    );
    assert_eq!(
        rig.shape("s").await,
        Some((MarkerState::Dirty, false, true))
    );
    assert_eq!(leader.step().await, report(1, 0));
    assert_eq!(rig.cold_body("s").await, Some(bytes("stray")));

    rig.hot.put("t", bytes("stray too")).await.unwrap();
    rig.clock.advance(700_000);
    assert_eq!(leader.step().await, report(0, 0));
    assert_eq!(rig.shape("t").await, None);

    rig.pass(Duration::from_secs(100));
    let later = leader.step().await;
    assert_eq!(later.strays_adopted, 1);
    assert_eq!(leader.strays_adopted(), 2);
    assert_eq!(
        rig.shape("t").await,
        Some((MarkerState::Dirty, false, true))
    );
}

/// Asserts that a record that says `active-marks` but lacks the epoch, or has an earlier one, is
/// not one the leader works under, as it is not one a proxy writes under: no marker is read or
/// written, and the copy waits.
#[tokio::test]
async fn a_record_without_epoch_three_is_inert_for_the_leader_as_for_the_proxies() {
    for epoch in [None, Some(2)] {
        let rig = Rig::new();
        rig.format_at(FormatPhase::ActiveMarks, epoch, Some(BUCKET))
            .await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        rig.plant(dirty("k")).await;
        let mut leader = rig.leader("r1");
        assert_eq!(leader.step().await, StepReport::default(), "{epoch:?}");
        assert!(rig.marks_calls.call_log().is_empty(), "{epoch:?}");
        assert!(rig.cold_body("k").await.is_none(), "{epoch:?}");
    }
}

/// Asserts that the lease the passes ask the margin of is the reconciler's own: a lease of the
/// same holder made on its own (a second nonce) cannot take the lease the reconciler holds, while
/// the one made from the reconciler's lease holds it as the reconciler does.
#[tokio::test]
async fn the_passes_hold_the_reconcilers_lease_and_a_lease_made_beside_it_does_not() {
    let rig = Rig::new();
    rig.activate().await;
    let mut leader = rig.leader("r1");
    assert!(leader.step().await.held);
    let beside = LeaderLease::new(rig.hot_calls.clone(), &rig.config("r1").base);
    assert!(!beside.take().await);
    let shared = LeaderLease::of(leader.lease.clone());
    assert!(shared.take().await);
    assert!(shared.require().await.is_ok());
}

/// Asserts that the lease passes from the entries to the markers and back with one holder: two
/// leaders, the first holds the lease through the entry phase, the activation, a rollback to the
/// entries and a second activation, taking the lease object once; the second never holds it and
/// never reads a marker; and a leader that comes back to the markers starts its passes afresh
/// (the records of its copies are gone).
#[tokio::test]
async fn the_lease_passes_across_the_phases_with_no_second_leader() {
    let rig = Rig::new();
    rig.format(FormatPhase::Active, None).await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant(dirty("k")).await;
    let mut first = rig.leader("r1");
    let mut second = rig.leader("r2");
    let side = first.marks.clone().unwrap();

    assert!(first.step().await.held);
    assert!(!second.step().await.held);
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Dirty, false, true))
    );

    rig.activate().await;
    let calls = rig.marks_calls.call_log().len();
    assert!(!second.step().await.held);
    assert_eq!(rig.marks_calls.call_log().len(), calls);
    assert_eq!(first.step().await, report(1, 0));
    assert_eq!(rig.cold_body("k").await, Some(bytes("v")));
    let records = |key: &str| side.records.get(key);
    assert!(records("k").is_some());

    rig.format(FormatPhase::Active, None).await;
    assert!(first.step().await.held);
    assert!(!second.step().await.held);
    assert!(records("k").is_some());

    rig.activate().await;
    let again = first.step().await;
    assert!(again.held);
    assert!(records("k").is_none());
    assert!(!second.step().await.held);
    // Nobody replaced the lease object or renewed it: the one lease was taken once, by the first.
    let body = rig.hot.get(LEASE_KEY, None).await.unwrap();
    assert_eq!(body.split(|byte| *byte == b'\n').next(), Some(&b"r1"[..]));
    assert_eq!(rig.hot_calls.calls_on(Op::PutIfMatch, LEASE_KEY), 0);
    assert_eq!(rig.hot_calls.calls_on(Op::Delete, LEASE_KEY), 0);
}

/// A leader `r1` that dies in the middle of the eviction of `a`, holding the lease and the
/// `LOCK(EVICT)` of `a`, with `a`, `b` and `c` over the high water and a cold copy of each.
async fn dies_in_an_eviction(rig: &Rig) {
    rig.activate().await;
    crowded(rig, &["a", "b", "c"]).await;
    let gated = Arc::new(GateBackend::new(rig.hot_calls.clone()));
    let gate = gated.gate(Op::DeleteIfMatch, "a", GatePhase::Pre);
    let mut dying = rig.leader_over_with(gated, rig.cold_calls.clone(), "r1", tight);
    let mut step = Box::pin(dying.step());
    park(&mut step, &gate).await;
    drop(step);
    assert_eq!(
        rig.shape("a").await,
        Some((MarkerState::Lock, false, false))
    );
}

/// Asserts the takeover after a death in an eviction: the successor (another holder) cannot take
/// the lease before the ttl, then takes it, reaps the lock the dead leader left (its owner has no
/// registration), and evicts `a` under a lock of its own, which it releases.
#[tokio::test]
async fn the_successor_of_a_leader_that_died_in_an_eviction_reaps_its_lock_and_evicts() {
    let rig = Rig::new();
    dies_in_an_eviction(&rig).await;
    let mut successor = rig.leader_with("r2", tight);
    let first = successor.step().await;
    assert_eq!((first.active, first.held), (true, false));
    assert_eq!(
        rig.shape("a").await,
        Some((MarkerState::Lock, false, false))
    );
    rig.pass(Duration::from_secs(31));
    assert_eq!(successor.step().await, evicting(1, 2, 0));
    assert!(rig.hot_body("a").await.is_none());
    assert_eq!(rig.cold_body("a").await, Some(thirty("a")));
    assert_eq!(rig.shape("a").await, None);
}

/// Asserts that a leader restarted under the same holder name waits out the lease of the lock its
/// dead pass left (the holder of a leader has no registration, which must not make its lock
/// count as departed): the lock stands, `a` is not evicted (`b` is, instead) until `lock_lease`
/// has passed on the new leader's clock, and then the lock is reaped.
#[tokio::test]
async fn a_leader_restarted_under_the_same_holder_leaves_the_lock_of_its_dead_pass_until_it_expires()
 {
    let rig = Rig::new();
    dies_in_an_eviction(&rig).await;
    let mut restarted = rig.leader_with("r1", tight);
    restarted.step().await;
    rig.pass(Duration::from_secs(31));
    assert_eq!(restarted.step().await, evicting(1, 1, 1));
    assert_eq!(
        rig.shape("a").await,
        Some((MarkerState::Lock, false, false))
    );
    assert!(rig.hot_body("a").await.is_some());
    assert!(rig.hot_body("b").await.is_none());

    rig.pass(Duration::from_secs(119));
    assert_eq!(restarted.step().await, report(0, 1));
    assert_eq!(
        rig.shape("a").await,
        Some((MarkerState::Lock, false, false))
    );
    rig.pass(Duration::from_secs(1));
    assert_eq!(restarted.step().await, report(1, 0));
    assert_eq!(rig.shape("a").await, None);
    assert!(rig.hot_body("a").await.is_some());
}

/// Asserts the takeover after a death in a copy-back: the successor takes the lease after the
/// ttl, reaps the copy-back's lock (it owes nothing, so the marker is removed and the ask is
/// lost), copies nothing, and copies the object back when a read asks again.
#[tokio::test]
async fn the_successor_of_a_leader_that_died_in_a_copy_back_reaps_its_lock_and_serves_the_next_ask()
{
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    rig.plant(want("k")).await;
    let gated = Arc::new(GateBackend::new(rig.hot_calls.clone()));
    let gate = gated.gate(Op::Put, "k", GatePhase::Pre);
    let mut dying = rig.leader_over_with(gated, rig.cold_calls.clone(), "r1", |_| ());
    let mut step = Box::pin(dying.step());
    park(&mut step, &gate).await;
    drop(step);
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Lock, false, false))
    );
    assert!(rig.hot_body("k").await.is_none());

    let mut successor = rig.leader("r2");
    assert!(!successor.step().await.held);
    rig.pass(Duration::from_secs(31));
    assert_eq!(successor.step().await, report(1, 0));
    assert_eq!(rig.shape("k").await, None);
    assert!(rig.hot_body("k").await.is_none());

    rig.plant(want("k")).await;
    assert_eq!(successor.step().await, report(1, 0));
    assert_eq!(rig.hot_body("k").await, Some(bytes("v")));
}

/// Asserts that an eviction cycle that runs out of candidates above the low water stays open, so
/// that the next step goes on with it though the figure is under the high water: of three objects
/// of ten, thirty and forty-five bytes with one candidate offered, the step evicts the oldest
/// (seventy-five bytes left) and reports the cycle open, the next step has nothing to offer and
/// reports it still open, and a leader with no figure yet evicts nothing.
#[tokio::test]
async fn a_cycle_that_runs_out_of_candidates_stays_open_for_the_next_step() {
    let rig = Rig::new();
    rig.activate().await;
    for (key, size) in [("a", 10), ("b", 30), ("c", 45)] {
        rig.hot
            .put(key, Bytes::from("x".repeat(size)))
            .await
            .unwrap();
        rig.cold
            .put(key, Bytes::from("x".repeat(size)))
            .await
            .unwrap();
        rig.clock.advance(1000);
    }
    let mut leader = rig.leader_with("r1", |config| {
        tight(config);
        config.candidate_limit = 1;
    });
    let mut idle = StepReport::default();
    let side = leader.marks.clone().unwrap();
    leader.mark_evict(&side, 0, &mut idle).await;
    assert_eq!(idle, StepReport::default());

    assert_eq!(
        leader.step().await,
        StepReport {
            evicting: true,
            ..evicting(1, 1, 0)
        }
    );
    assert!(rig.hot_body("a").await.is_none());
    let next = leader.step().await;
    assert_eq!(
        next,
        StepReport {
            evicting: true,
            ..report(0, 0)
        }
    );
    assert!(rig.hot_body("b").await.is_some());
}

/// Asserts that the `CLEARING` marker of a live owner is left (counted as pinned) until its owner
/// departs, the lease of the lock runs out, or it is the leader's own.
#[tokio::test]
async fn a_clearing_marker_of_a_live_owner_is_left() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    let mut clearing = lock("k", MarkerWhy::Overwrite, "p1", false);
    clearing.st = MarkerState::Clearing;
    rig.plant(clearing).await;
    let mut leader = rig.leader("r1");
    assert_eq!(leader.step().await, report(0, 1));
    assert_eq!(
        rig.shape("k").await,
        Some((MarkerState::Clearing, false, true))
    );
}
