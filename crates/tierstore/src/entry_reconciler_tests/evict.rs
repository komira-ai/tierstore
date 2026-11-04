//! The usage figure and the eviction: what the leader publishes, which keys it evicts and which it
//! never touches, and every race of an eviction with a client, each driven one store call at a
//! time through a gate. No test reads a clock or waits on one: the monotonic clock is moved by
//! hand.

use super::*;
use crate::entry_reconciler::evict::Tags;
use crate::journal::{USAGE_KEY, Usage};

/// A body of thirty bytes made of `fill`.
fn thirty(fill: &str) -> Bytes {
    Bytes::from(fill.repeat(30))
}

/// The settings of a reconciler over a hot tier budgeted at 100 bytes (high water 80, low water
/// 70), with one item at a time so that the order of the calls is the order of the candidates.
fn small(config: &mut EntryReconcilerConfig) {
    config.base.hot_budget = 100;
    config.base.parallel = 1;
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

/// The usage object, as the proxies read it.
async fn published(rig: &Rig) -> Usage {
    let body = rig.hot.get(USAGE_KEY, None).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// A hot tier of one object `a` of thirty bytes over a budget of 10 (so that it is always the
/// candidate), as a reconciler sees it through a gate on the create of an entry and a fault
/// injector on each tier.
struct Parked {
    hot: Arc<FaultyBackend>,
    cold: Arc<FaultyBackend>,
    gate: Gate,
    reconciler: EntryReconciler,
}

impl Parked {
    async fn new(rig: &Rig, cold_copy: bool) -> Self {
        rig.activate().await;
        rig.hot.put("a", thirty("a")).await.unwrap();
        if cold_copy {
            rig.cold.put("a", thirty("a")).await.unwrap();
        }
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let gated = Arc::new(GateBackend::new(hot.clone()));
        let gate = gated.gate(Op::Put, ENTRIES_PREFIX, Phase::Pre);
        let reconciler = rig.over(gated, cold.clone(), "r1", |config| {
            small(config);
            config.base.hot_budget = 10;
        });
        Self {
            hot,
            cold,
            gate,
            reconciler,
        }
    }
}

/// Asserts that above the high water the oldest hot objects that the cold tier holds are deleted
/// down to the low water and no further: of three objects of thirty bytes in a budget of a hundred
/// the oldest goes, the others stay, the cold tier keeps its copy, no entry is left, and the usage
/// published afterwards is the sixty bytes that remain.
#[tokio::test]
async fn eviction_deletes_the_oldest_objects_the_cold_tier_holds_down_to_the_low_water() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", small);
    let step = r.step().await;
    assert_eq!(
        step,
        StepReport {
            evicted: 1,
            ..report(1)
        }
    );
    assert!(!rig.hot_has("a").await);
    assert!(rig.hot_has("b").await && rig.hot_has("c").await);
    assert!(rig.cold_has("a").await);
    assert!(rig.entry("a").await.is_none());
    assert_eq!(published(&rig).await.hot_bytes, 60);
}

/// Asserts that a hot tier at or under the high water is left alone: two objects of thirty bytes
/// and one of twenty (eighty bytes) evict nothing, and the usage published is eighty.
#[tokio::test]
async fn nothing_is_evicted_at_or_under_the_high_water() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b"]).await;
    rig.hot.delete("c").await.unwrap();
    rig.hot.put("c", Bytes::from("c".repeat(20))).await.unwrap();
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", small);
    assert_eq!(r.step().await, report(0));
    for key in ["a", "b", "c"] {
        assert!(rig.hot_has(key).await);
    }
    assert_eq!(published(&rig).await.hot_bytes, 80);
}

/// Asserts that a key with an entry is never evicted, whatever the entry says: held by a live
/// owner (a put and a delete) and an operation this release does not know. The oldest key is
/// skipped with its entry as it was and the next oldest goes.
#[tokio::test]
async fn eviction_never_touches_a_key_with_an_open_entry() {
    let shapes = [
        (EntryOp::Put, EntryState::Held, Some("p1")),
        (EntryOp::Del, EntryState::Held, Some("p1")),
        (EntryOp::Unknown, EntryState::Pending, None),
    ];
    for (op, st, owner) in shapes {
        let rig = Rig::new();
        rig.activate().await;
        rig.register("p1").await;
        crowded(&rig, &["a", "b", "c"]).await;
        rig.plant("a", op, st, owner).await;
        let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", small);
        let step = r.step().await;
        assert_eq!(step.evicted, 1, "{op:?}");
        assert!(rig.hot_has("a").await, "{op:?}");
        assert!(!rig.hot_has("b").await, "{op:?}");
        let shape = rig.shape("a").await;
        assert_eq!(shape, Some((op, st, owner.map(str::to_owned))));
    }
}

/// Asserts that an eviction with nothing evictable reports that it is still evicting and the loop
/// waits the floor, and not longer, for as long as that lasts: every key has an entry held by a
/// live owner, so nothing goes, and the idle waits as values stay at the floor without losing the
/// doubling they were at.
#[tokio::test]
async fn evicting_with_nothing_evictable_waits_at_the_floor() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    crowded(&rig, &["a", "b", "c"]).await;
    for key in ["a", "b", "c"] {
        rig.plant(key, EntryOp::Put, EntryState::Held, Some("p1"))
            .await;
    }
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", small);
    let step = r.step().await;
    assert_eq!((step.evicted, step.pinned, step.evicting), (0, 3, true));
    assert!(!step.progress());

    let secs = Duration::from_secs;
    let mut idle = Idle::new(secs(1), secs(10));
    assert_eq!(idle.after(&report(0)), Some(secs(1)));
    assert_eq!(idle.after(&report(0)), Some(secs(2)));
    assert_eq!(idle.after(&step), Some(secs(1)));
    assert_eq!(idle.after(&step), Some(secs(1)));
    assert_eq!(idle.after(&report(0)), Some(secs(4)));
}

/// Asserts that a hot object with no cold copy is not evicted but adopted: it gets a pending put
/// entry, the next step copies it and removes the entry, and the object that was evictable goes.
#[tokio::test]
async fn a_hot_object_without_a_cold_copy_is_adopted_and_not_evicted() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["b", "c"]).await;
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", small);
    let step = r.step().await;
    assert_eq!((step.strays_adopted, step.evicted), (1, 1));
    assert_eq!(r.strays_adopted(), 1);
    assert!(rig.hot_has("a").await);
    assert!(!rig.hot_has("b").await);
    assert_eq!(
        rig.shape("a").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
    let next = r.step().await;
    assert_eq!(next.changed, 1);
    assert!(rig.cold_has("a").await && rig.hot_has("a").await);
    assert!(rig.entry("a").await.is_none());
}

/// Asserts that a cold object of other bytes (a different size, or the same size with other bytes)
/// keeps the hot object: the key is reported as a mismatch, nothing is deleted or entered for it,
/// and the cold object is as it was.
#[tokio::test]
async fn a_cold_object_with_other_bytes_keeps_the_hot_object_and_is_reported() {
    for other in [Bytes::from("a".repeat(31)), thirty("z")] {
        let rig = Rig::new();
        rig.activate().await;
        crowded(&rig, &["b", "c"]).await;
        rig.cold.put("a", other.clone()).await.unwrap();
        let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", small);
        let step = r.step().await;
        assert_eq!(step.mismatched, ["a"]);
        assert_eq!(step.evicted, 1);
        assert!(rig.hot_has("a").await);
        assert_eq!(rig.cold.get("a", None).await.unwrap(), other);
        assert!(rig.entry("a").await.is_none());
    }
}

/// Asserts that the compare is skipped for a key this leader copied when both tags are still the
/// ones it recorded: a put copied and evicted in one step costs no read of the cold object's
/// bytes.
#[tokio::test]
async fn eviction_skips_the_compare_for_a_key_this_leader_copied_with_both_tags_unchanged() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["b", "c"]).await;
    land(&rig, "p1", "a").await;
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(rig.hot.clone(), cold.clone(), "r1", small);
    let step = r.step().await;
    assert_eq!(step.evicted, 1);
    assert!(!rig.hot_has("a").await && rig.cold_has("a").await);
    assert_eq!(cold.calls(Op::Get), 0);
}

/// Asserts that a key the leader did not copy is compared before it is evicted: both objects are
/// read, and the object goes.
#[tokio::test]
async fn eviction_compares_a_key_this_leader_did_not_copy() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(rig.hot.clone(), cold.clone(), "r1", small);
    assert_eq!(r.step().await.evicted, 1);
    assert_eq!(cold.calls(Op::Get), 1);
    assert!(!rig.hot_has("a").await);
}

/// Asserts that a record of tags does not outlive a change of either object, a new leader, or a
/// copy whose tag could not be read back: after each the key is compared before it is evicted.
/// The first step copies `a` (nothing is over the high water); after it two more objects make the
/// hot tier crowded, and a listing a day later finds it so.
#[tokio::test]
async fn eviction_compares_when_the_record_of_tags_is_gone_or_stale() {
    for case in ["cold object replaced", "new leader", "tag not read back"] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("a", thirty("a")).await.unwrap();
        rig.clock.advance(1000);
        land(&rig, "p1", "a").await;
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let gated = Arc::new(GateBackend::new(cold.clone()));
        let gate = gated.gate(Op::Put, "a", Phase::Pre);
        let parks = case == "tag not read back";
        let tier: Arc<dyn Backend> = if parks { gated } else { cold.clone() };
        let mut r = rig.over(rig.hot.clone(), tier, "r1", small);
        let mut step = Box::pin(r.step());
        if parks {
            park(&mut step, &gate).await;
            cold.fail_next_on(Op::Head, "a", Error::Unavailable("head".to_owned()));
            gate.release();
        }
        let first = step.await;
        assert!(first.failed.is_empty(), "{case}");
        assert!(rig.cold_has("a").await, "{case}");
        assert_eq!(cold.calls(Op::Get), 0, "{case}");

        for key in ["b", "c"] {
            rig.hot.put(key, thirty(key)).await.unwrap();
            rig.cold.put(key, thirty(key)).await.unwrap();
        }
        match case {
            "cold object replaced" => {
                rig.cold.delete("a").await.unwrap();
                rig.cold.put("a", thirty("z")).await.unwrap();
            }
            "new leader" => r.stop().await,
            _ => {}
        }
        rig.mono.advance(Duration::from_secs(24 * 3600));
        let second = r.step().await;
        assert!(cold.calls(Op::Get) > 0, "{case}");
        if case == "cold object replaced" {
            assert_eq!(second.mismatched, ["a"]);
            assert!(rig.hot_has("a").await);
        } else {
            assert!(!rig.hot_has("a").await, "{case}");
        }
    }
}

/// Asserts that an object that changes between the compare and the entry is kept and the entry of
/// the eviction removed: the hot object replaced by other bytes (another tag), the cold object
/// replaced, the hot object deleted, the cold object deleted.
#[tokio::test]
async fn an_object_that_changes_after_the_compare_is_kept_and_the_entry_removed() {
    for case in [
        "hot replaced",
        "cold replaced",
        "hot deleted",
        "cold deleted",
    ] {
        let rig = Rig::new();
        let mut parked = Parked::new(&rig, true).await;
        let mut step = Box::pin(parked.reconciler.step());
        park(&mut step, &parked.gate).await;
        assert_eq!(
            rig.shape("a").await,
            None,
            "the entry is made after the park"
        );
        match case {
            "hot replaced" => {
                rig.hot.delete("a").await.unwrap();
                rig.hot.put("a", thirty("z")).await.unwrap();
            }
            "cold replaced" => {
                rig.cold.delete("a").await.unwrap();
                rig.cold.put("a", thirty("z")).await.unwrap();
            }
            "hot deleted" => rig.hot.delete("a").await.unwrap(),
            _ => rig.cold.delete("a").await.unwrap(),
        }
        parked.gate.release();
        let done = step.await;
        assert_eq!((done.evicted, done.failed.len()), (0, 0), "{case}");
        assert!(rig.entry("a").await.is_none(), "{case}");
        assert_eq!(rig.hot_has("a").await, case != "hot deleted", "{case}");
        assert_eq!(rig.cold_has("a").await, case != "cold deleted", "{case}");
    }
}

/// The regression of the race that started the redesign: the evictor compares and parks; a delete
/// removes the key and a put of identical bytes (the same content tag) lands and is acknowledged
/// as a pending put; the evictor resumes, finds the entry, and deletes nothing. The new object is
/// in the hot tier, the cold tier has none yet, and the entry is the put's.
#[tokio::test]
async fn evict_vs_delete_then_identical_reput_keeps_new_object() {
    let rig = Rig::new();
    let mut parked = Parked::new(&rig, true).await;
    let mut step = Box::pin(parked.reconciler.step());
    park(&mut step, &parked.gate).await;
    let client = rig.client("p1");
    let Created::Taken(delete) = client.create("a", Take::Del).await.unwrap() else {
        panic!("the entry exists");
    };
    rig.cold.delete("a").await.unwrap();
    rig.hot.delete("a").await.unwrap();
    assert!(matches!(
        client.release(&delete).await.unwrap(),
        Outcome::Done(())
    ));
    let Created::Taken(put) = client.create("a", Take::Put).await.unwrap() else {
        panic!("the entry exists");
    };
    rig.hot.put("a", thirty("a")).await.unwrap();
    let landed = client.advance(&put, Step::LandPut).await.unwrap();
    assert!(matches!(landed, Outcome::Done(_)));
    parked.gate.release();
    let done = step.await;
    assert_eq!((done.evicted, done.failed.len()), (0, 0));
    assert_eq!(rig.hot.get("a", None).await.unwrap(), thirty("a"));
    assert!(!rig.cold_has("a").await);
    assert_eq!(
        rig.shape("a").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
}

/// Asserts that an eviction that meets an entry made by a put, a delete or a copy-back while it
/// was parked at its create leaves the entry as that client made it, and deletes nothing.
#[tokio::test]
async fn an_entry_made_meanwhile_stops_the_eviction_and_is_left_as_made() {
    for take in [Take::Put, Take::Del, Take::CopyBack] {
        let rig = Rig::new();
        let mut parked = Parked::new(&rig, true).await;
        let mut step = Box::pin(parked.reconciler.step());
        park(&mut step, &parked.gate).await;
        let made = rig.client("p1").create("a", take).await.unwrap();
        assert!(matches!(made, Created::Taken(_)));
        let before = rig.shape("a").await;
        parked.gate.release();
        let done = step.await;
        assert_eq!((done.evicted, done.failed.len()), (0, 0), "{take:?}");
        assert_eq!(rig.shape("a").await, before, "{take:?}");
        assert!(rig.hot_has("a").await && rig.cold_has("a").await);
    }
}

/// Asserts that a hot object with no cold copy is not adopted over an entry made while the
/// adoption was parked at its create: the entry stays the put's, and nothing is counted adopted.
#[tokio::test]
async fn an_entry_made_meanwhile_stops_the_adoption_of_an_unreplicated_object() {
    let rig = Rig::new();
    let mut parked = Parked::new(&rig, false).await;
    let mut step = Box::pin(parked.reconciler.step());
    park(&mut step, &parked.gate).await;
    let made = rig.client("p1").create("a", Take::Put).await.unwrap();
    assert!(matches!(made, Created::Taken(_)));
    parked.gate.release();
    let done = step.await;
    assert_eq!((done.strays_adopted, done.failed.len()), (0, 0));
    assert_eq!(
        rig.shape("a").await,
        Some((EntryOp::Put, EntryState::Held, Some("p1".to_owned())))
    );
}

/// Asserts what each answer of a store does to an eviction that is under its entry, after the
/// compare: a call that fails is the item's failure and the entry is removed all the same; a
/// delete that answers that the object is gone or is not the one named leaves it; a store that
/// cannot delete on a tag gets a plain delete; an entry that cannot be removed stays sealed with
/// the object gone, and the next step removes it.
#[tokio::test]
async fn every_answer_of_a_store_under_the_eviction_entry_is_judged() {
    let unavailable = || Error::Unavailable("store".to_owned());
    let missing = || Error::NotFound("a".to_owned());
    // The tier, the call, the prefix, the answer, the evictions, the failures and whether the hot
    // object is left.
    let cases = [
        (true, Op::Head, "a", missing(), 0, 0, true),
        (true, Op::Head, "a", unavailable(), 0, 1, true),
        (false, Op::Head, "a", missing(), 0, 0, true),
        (false, Op::Head, "a", unavailable(), 0, 1, true),
        (
            true,
            Op::DeleteIfMatch,
            "a",
            Error::Unsupported("tag".to_owned()),
            1,
            0,
            false,
        ),
        (true, Op::DeleteIfMatch, "a", missing(), 0, 0, true),
        (
            true,
            Op::DeleteIfMatch,
            "a",
            Error::PreconditionFailed("a".to_owned()),
            0,
            0,
            true,
        ),
        (true, Op::DeleteIfMatch, "a", unavailable(), 0, 1, true),
        (
            true,
            Op::DeleteIfMatch,
            ENTRIES_PREFIX,
            unavailable(),
            0,
            1,
            false,
        ),
    ];
    for (n, (on_hot, op, prefix, error, evicted, failed, left)) in cases.into_iter().enumerate() {
        let rig = Rig::new();
        let mut parked = Parked::new(&rig, true).await;
        let mut step = Box::pin(parked.reconciler.step());
        park(&mut step, &parked.gate).await;
        let faulty = if on_hot { &parked.hot } else { &parked.cold };
        faulty.fail_next_on(op, prefix, error);
        parked.gate.release();
        let done = step.await;
        assert_eq!(done.evicted, evicted, "case {n}");
        assert_eq!(done.failed.len(), failed, "case {n}");
        assert_eq!(rig.hot_has("a").await, left, "case {n}");
        if prefix == ENTRIES_PREFIX {
            assert_eq!(
                rig.shape("a").await,
                Some((EntryOp::Evict, EntryState::Sealed, None))
            );
            assert_eq!(parked.reconciler.step().await.changed, 1);
        }
        assert!(rig.entry("a").await.is_none(), "case {n}");
    }
}

/// Asserts that a delete of the hot object is refused when less than the margin of the lease is
/// left (at the margin and a millisecond under it): the object stays, the item fails, and the
/// entry of the eviction is removed.
#[tokio::test]
async fn an_eviction_needs_the_margin_of_the_lease() {
    for (left, allowed) in [(10_000, true), (10_001, false)] {
        let rig = Rig::new();
        let mut parked = Parked::new(&rig, true).await;
        let mut step = Box::pin(parked.reconciler.step());
        park(&mut step, &parked.gate).await;
        rig.mono.advance(Duration::from_millis(left));
        parked.gate.release();
        let done = step.await;
        assert_eq!(rig.hot_has("a").await, !allowed, "left {left}");
        assert_eq!(done.failed.len(), usize::from(!allowed), "left {left}");
        assert!(rig.entry("a").await.is_none(), "left {left}");
    }
}

/// Asserts that a call that fails before the entry (the read of the entry, the heads, the compare,
/// the create of the entry of an eviction, the create of the entry of an adoption) fails the item
/// and nothing else: the object stays, and the candidate is tried again at the next step, which
/// evicts it; a head that answers not found is an object that is gone.
#[tokio::test]
async fn a_call_that_fails_before_the_entry_fails_the_item_and_it_is_tried_again() {
    let unavailable = || Error::Unavailable("store".to_owned());
    // The tier, the call, the prefix, the answer, whether `a` has a cold copy and the failures.
    let cases = [
        (true, Op::Head, ENTRIES_PREFIX, unavailable(), true, 1),
        (true, Op::Head, "a", unavailable(), true, 1),
        (
            true,
            Op::Head,
            "a",
            Error::NotFound("a".to_owned()),
            true,
            0,
        ),
        (false, Op::Head, "a", unavailable(), true, 1),
        (true, Op::Get, "a", unavailable(), true, 1),
        (true, Op::Put, ENTRIES_PREFIX, unavailable(), true, 1),
        (true, Op::Put, ENTRIES_PREFIX, unavailable(), false, 1),
    ];
    for (n, (on_hot, op, prefix, error, cold_copy, failed)) in cases.into_iter().enumerate() {
        let rig = Rig::new();
        rig.activate().await;
        let cold_keys: &[&str] = if cold_copy {
            &["a", "b", "c"]
        } else {
            &["b", "c"]
        };
        crowded(&rig, cold_keys).await;
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let faulty = if on_hot { &hot } else { &cold };
        faulty.fail_next_on(op, prefix, error);
        let mut r = rig.over(hot.clone(), cold.clone(), "r1", small);
        let step = r.step().await;
        assert_eq!(step.failed.len(), failed, "case {n}");
        assert!(rig.hot_has("a").await, "case {n}");
        if failed > 0 {
            assert_eq!(step.evicted, 0, "case {n}");
            let next = r.step().await;
            assert_eq!(next.evicted, 1, "case {n}");
            assert!(next.failed.is_empty(), "case {n}");
        }
    }
}

/// Asserts that the candidates of a listing are the oldest objects, no more than the limit: with a
/// limit of one only the oldest of four objects is a candidate, so that the hot tier is left above
/// the low water after it goes, and the step says it is still evicting.
#[tokio::test]
async fn the_candidates_of_a_listing_are_the_oldest_up_to_the_limit() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c", "d"]).await;
    rig.hot.put("d", thirty("d")).await.unwrap();
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        small(config);
        config.candidate_limit = 1;
    });
    let step = r.step().await;
    assert_eq!((step.evicted, step.evicting), (1, true));
    assert!(!rig.hot_has("a").await);
    assert!(rig.hot_has("b").await && rig.hot_has("d").await);
    assert_eq!(published(&rig).await.hot_bytes, 90);
}

/// Asserts that a new leader starts from the figure the leader before it published and lists the
/// hot tier a few pages a step: while pages are left the step reports it, which is progress, and
/// the object still says the old figure; once the listing ends the figure is the bytes of the
/// objects that are not under a reserved prefix, published when its interval has run out.
#[tokio::test]
async fn a_new_leader_starts_from_the_published_figure_and_lists_a_few_pages_a_step() {
    let rig = Rig::new();
    rig.activate().await;
    let old = Usage {
        hot_bytes: 1234,
        copy_lag: 0,
        time: 1,
    };
    let body = Bytes::from(serde_json::to_vec(&old).unwrap());
    rig.hot.put(USAGE_KEY, body).await.unwrap();
    for n in 0..5 {
        rig.hot
            .put(&format!("k{n}"), bytes("0123456789"))
            .await
            .unwrap();
    }
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.base.page_size = 2;
        config.scan_pages = 1;
    });
    let first = r.step().await;
    assert!(first.scanning && first.progress());
    assert_eq!(published(&rig).await.hot_bytes, 1234);
    let mut steps = 1;
    let mut last = first;
    while last.scanning {
        last = r.step().await;
        steps += 1;
    }
    assert!(steps > 2, "{steps}");
    assert!(!last.progress());
    rig.mono.advance(Duration::from_secs(10));
    r.step().await;
    assert_eq!(published(&rig).await.hot_bytes, 50);
}

/// Asserts that a figure that is not known is not published and not acted on: with no object of
/// the leader before and a listing that has not ended, nothing is written to `_tiering/usage`,
/// however far over the budget the hot tier is.
#[tokio::test]
async fn an_unknown_figure_is_neither_published_nor_evicted_on() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        small(config);
        config.base.page_size = 2;
        config.scan_pages = 1;
    });
    let first = r.step().await;
    assert!(first.scanning);
    assert_eq!(first.evicted, 0);
    assert!(rig.hot.head(USAGE_KEY).await.is_err());
}

/// Asserts that the figure is published again when its interval has run out on the monotonic
/// clock and not before: at the interval less a millisecond it is not, at the interval it is.
#[tokio::test]
async fn the_usage_is_published_again_when_the_interval_has_run_out() {
    let rig = Rig::new();
    rig.activate().await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let mut r = rig.over(hot.clone(), rig.cold.clone(), "r1", |_| {});
    let writes = || {
        hot.call_log()
            .iter()
            .filter(|(op, key)| matches!(op, Op::Put | Op::PutIfMatch) && key == USAGE_KEY)
            .count()
    };
    r.step().await;
    r.step().await;
    assert_eq!(writes(), 1);
    rig.mono.advance(Duration::from_millis(9_999));
    r.step().await;
    assert_eq!(writes(), 1);
    rig.mono.advance(Duration::from_millis(1));
    r.step().await;
    assert_eq!(writes(), 2);
}

/// Asserts that between listings the figure moves by what the leader does: an object copied that
/// was written after the listing started adds its bytes, one that was written at the time the
/// listing started or before does not (the listing has it), and one that was copied before the
/// first listing is not counted twice.
#[tokio::test]
async fn between_listings_the_figure_adds_the_objects_copied_that_landed_after_the_listing() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("old", bytes("0123456789")).await.unwrap();
    land(&rig, "p1", "old").await;
    let mut r = rig.reconciler();
    r.step().await;
    assert_eq!(published(&rig).await.hot_bytes, 10);

    rig.hot.put("same-time", bytes("0123456789")).await.unwrap();
    land(&rig, "p1", "same-time").await;
    rig.clock.advance(1);
    rig.hot
        .put("new", Bytes::from("n".repeat(20)))
        .await
        .unwrap();
    land(&rig, "p1", "new").await;
    rig.mono.advance(Duration::from_secs(10));
    r.step().await;
    assert_eq!(published(&rig).await.hot_bytes, 30);
}

/// Asserts that a failed listing of the hot tier is a failure and not progress, that the listing
/// goes on at the next call, and that a failed read of the published figure is a failure too while
/// a missing or an unreadable one is not.
#[tokio::test]
async fn a_failed_listing_or_read_of_the_figure_is_reported_and_tried_again() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("0123456789")).await.unwrap();
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let mut r = rig.over(hot.clone(), rig.cold.clone(), "r1", |config| {
        config.usage_scan_interval = Duration::from_secs(100);
    });
    r.step().await;
    let error = Error::Unavailable("list".to_owned());
    hot.fail_next_on(Op::List, "", error.clone());
    let mut failed = StepReport::default();
    r.tend_usage(100_000, &mut failed).await;
    assert_eq!(failed.failed, [error]);
    assert!(!failed.scanning && !failed.progress());
    let mut again = StepReport::default();
    r.tend_usage(100_000, &mut again).await;
    assert!(again.failed.is_empty());
    assert_eq!(r.books.bytes, Some(10));

    for body in [None, Some(bytes("not json"))] {
        let rig = Rig::new();
        rig.activate().await;
        if let Some(body) = body {
            rig.hot.put(USAGE_KEY, body).await.unwrap();
        }
        let step = rig.reconciler().step().await;
        assert!(step.failed.is_empty());
    }
    let rig = Rig::new();
    rig.activate().await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let error = Error::Unavailable("get".to_owned());
    hot.fail_next_on(Op::Get, USAGE_KEY, error.clone());
    let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed, [error]);
}

/// Asserts that a figure that fails to be written is the step's failure and is written at the next
/// step, not an interval later.
#[tokio::test]
async fn a_failed_publication_is_tried_again_at_the_next_step() {
    let rig = Rig::new();
    rig.activate().await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let error = Error::Unavailable("put".to_owned());
    hot.fail_next_on(Op::Put, USAGE_KEY, error.clone());
    let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed, [error]);
    assert!(rig.hot.head(USAGE_KEY).await.is_err());
    assert!(r.step().await.failed.is_empty());
    assert_eq!(published(&rig).await.hot_bytes, 0);
}

/// Asserts that the records of tags keep the newest keys up to the limit (the oldest go first), that
/// a key recorded again is the newest, that a record matches only both of its tags, and that it can
/// be dropped one by one and all at once.
#[test]
fn the_records_of_tags_keep_the_newest_keys_up_to_the_limit() {
    let mut tags = Tags::default();
    tags.record("a", "ha", "ca", 2);
    tags.record("b", "hb", "cb", 2);
    tags.record("a", "ha2", "ca2", 2);
    tags.record("c", "hc", "cc", 2);
    assert!(!tags.matches("b", "hb", "cb"));
    assert!(tags.matches("a", "ha2", "ca2"));
    assert!(tags.matches("c", "hc", "cc"));
    assert!(!tags.matches("a", "ha", "ca2"));
    assert!(!tags.matches("a", "ha2", "ca"));
    tags.forget("a");
    assert!(!tags.matches("a", "ha2", "ca2"));
    tags.record("d", "hd", "cd", 0);
    assert!(tags.matches("d", "hd", "cd"));
    assert!(!tags.matches("c", "hc", "cc"));
    tags.clear();
    assert!(!tags.matches("d", "hd", "cd"));
}

/// Asserts the defaults of the settings of the usage and the eviction: the figure published every
/// ten seconds, a listing every day, a hundred pages of it a step, ten thousand candidates and a
/// hundred thousand remembered tags.
#[test]
fn the_usage_and_eviction_settings_default_to_the_values_of_the_design() {
    let config = Rig::new().config("r1");
    assert_eq!(config.usage_interval, Duration::from_secs(10));
    assert_eq!(config.usage_scan_interval, Duration::from_secs(24 * 3600));
    assert_eq!(config.scan_pages, 100);
    assert_eq!(config.candidate_limit, 10_000);
    assert_eq!(config.tag_record_limit, 100_000);
    assert_eq!(config.base.high_water, 0.8);
    assert_eq!(config.base.low_water, 0.7);
}
