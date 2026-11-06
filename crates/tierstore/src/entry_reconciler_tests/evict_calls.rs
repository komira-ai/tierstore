//! The store calls of one eviction, counted tier by tier and key by key, and the races of the two
//! looks the eviction no longer makes by themselves: the look for an entry before the head of a
//! key whose tags this leader recorded (there is no compare for an entry to spoil, and the create
//! of the eviction's entry is create-only), and the head of the hot object under the entry (the
//! delete on a tag is that look, in one call). Each race is driven one store call at a time
//! through a gate, and no test reads a clock or waits on one.

use super::evict::{crowded, small, thirty, took};
use super::*;
use crate::journal::LEASE_KEY;

/// The calls of the log that name `key` or its entry, in order: the kind and `"object"` or
/// `"entry"`.
fn on_key(log: Vec<(Op, String)>, key: &str) -> Vec<(Op, &'static str)> {
    let entry = entry_key(key);
    let named = |(op, path): (Op, String)| {
        if path == key {
            Some((op, "object"))
        } else if path == entry {
            Some((op, "entry"))
        } else {
            None
        }
    };
    log.into_iter().filter_map(named).collect()
}

/// Has `r` copy `a` to the cold tier in its first step, so that it has the tags of both objects,
/// then crowds the hot tier over the high water with `b` and `c`, which the cold tier holds too,
/// and makes a listing due: the next step of `r` evicts `a`.
async fn copied_then_crowded(rig: &Rig, r: &mut EntryReconciler) {
    rig.activate().await;
    rig.hot.put("a", thirty("a")).await.unwrap();
    rig.clock.advance(1000);
    land(rig, "p1", "a").await;
    assert!(r.step().await.failed.is_empty());
    assert!(rig.cold_has("a").await);
    for key in ["b", "c"] {
        rig.hot.put(key, thirty(key)).await.unwrap();
        rig.cold.put(key, thirty(key)).await.unwrap();
        rig.clock.advance(1000);
    }
    rig.mono.advance(Duration::from_secs(24 * 3600));
}

/// The two tiers as faulty wrappers over the rig's, and a reconciler over them.
fn counted(rig: &Rig) -> (Arc<FaultyBackend>, Arc<FaultyBackend>, EntryReconciler) {
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let r = rig.over(hot.clone(), cold.clone(), "r1", small);
    (hot, cold, r)
}

/// Asserts what an eviction of a key this leader copied asks of each tier, in order: the hot tier
/// is headed once, the entry is created without being read first, the hot object is deleted on
/// its tag (that call is the second look at it), and the entry is removed; the cold tier is
/// headed before and under the entry and its bytes are not read.
#[tokio::test]
async fn a_key_this_leader_copied_is_evicted_by_two_heads_one_create_and_two_deletes() {
    let rig = Rig::new();
    let (hot, cold, mut r) = counted(&rig);
    copied_then_crowded(&rig, &mut r).await;
    let (hot_before, cold_before) = (hot.call_log().len(), cold.call_log().len());
    let step = r.step().await;
    assert_eq!((step.evicted, step.failed.len()), (1, 0));
    assert!(!rig.hot_has("a").await);
    assert!(rig.cold_has("a").await);
    let hot_calls = on_key(hot.call_log().split_off(hot_before), "a");
    let want = [
        (Op::Head, "object"),
        (Op::Put, "entry"),
        (Op::DeleteIfMatch, "object"),
        (Op::DeleteIfMatch, "entry"),
    ];
    assert_eq!(hot_calls, want);
    let cold_calls = on_key(cold.call_log().split_off(cold_before), "a");
    assert_eq!(cold_calls, [(Op::Head, "object"), (Op::Head, "object")]);
}

/// Asserts what an eviction of a key that must be compared asks of each tier, in order: the hot
/// tier is headed once, the entry is looked up once before any bytes are read, then both objects
/// are read once, the entry is created, the hot object is deleted on its tag and the entry is
/// removed; the cold tier is headed, read, and headed under the entry.
#[tokio::test]
async fn a_key_that_is_compared_is_looked_up_for_an_entry_once_before_its_bytes_are_read() {
    let rig = Rig::new();
    rig.activate().await;
    crowded(&rig, &["a", "b", "c"]).await;
    let (hot, cold, mut r) = counted(&rig);
    let step = r.step().await;
    assert_eq!((step.evicted, step.failed.len()), (1, 0));
    let hot_calls = on_key(hot.call_log(), "a");
    let want = [
        (Op::Head, "object"),
        (Op::Get, "entry"),
        (Op::Get, "object"),
        (Op::Put, "entry"),
        (Op::DeleteIfMatch, "object"),
        (Op::DeleteIfMatch, "entry"),
    ];
    assert_eq!(hot_calls, want);
    let cold_calls = on_key(cold.call_log(), "a");
    let want = [
        (Op::Head, "object"),
        (Op::Get, "object"),
        (Op::Head, "object"),
    ];
    assert_eq!(cold_calls, want);
}

/// Asserts that a hot store that cannot delete on a tag is asked for a head of the object under
/// the entry and then a plain delete: the head is the second look, as it was before the delete on
/// a tag was the look.
#[tokio::test]
async fn a_store_that_cannot_delete_on_a_tag_gets_a_head_and_a_plain_delete() {
    let rig = Rig::new();
    let (hot, _cold, mut r) = counted(&rig);
    copied_then_crowded(&rig, &mut r).await;
    hot.fail_next_on(Op::DeleteIfMatch, "a", Error::Unsupported("tag".to_owned()));
    let hot_before = hot.call_log().len();
    let step = r.step().await;
    assert_eq!((step.evicted, step.failed.len()), (1, 0));
    assert!(!rig.hot_has("a").await);
    let want = [
        (Op::Head, "object"),
        (Op::Put, "entry"),
        (Op::DeleteIfMatch, "object"),
        (Op::Head, "object"),
        (Op::Delete, "object"),
        (Op::DeleteIfMatch, "entry"),
    ];
    assert_eq!(on_key(hot.call_log().split_off(hot_before), "a"), want);
}

/// Asserts that a put that takes the entry of a key whose tags this leader recorded, after the
/// leader headed both objects and before it created its entry, is not undone: the create finds the
/// put's entry, nothing is deleted, no bytes are read or compared (so nothing is reported as a
/// mismatch between the new bytes and the cold object's old ones), and the entry is the put's.
#[tokio::test]
async fn a_put_that_lands_after_the_heads_of_a_key_with_recorded_tags_is_not_evicted() {
    let rig = Rig::new();
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let gated = Arc::new(GateBackend::new(hot));
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(gated.clone(), cold.clone(), "r1", small);
    copied_then_crowded(&rig, &mut r).await;
    let gate = gated.gate(Op::Put, ENTRIES_PREFIX, Phase::Pre);
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    assert_eq!(rig.shape("a").await, None);
    let client = rig.client("p1");
    let put = took(client.create("a", Take::Put).await.unwrap());
    rig.hot.delete("a").await.unwrap();
    rig.hot.put("a", thirty("z")).await.unwrap();
    let landed = client.advance(&put, Step::LandPut).await.unwrap();
    assert!(matches!(landed, Outcome::Done(_)));
    gate.release();
    let done = step.await;
    // `b`, the next oldest, goes.
    assert_eq!((done.evicted, done.failed.len()), (1, 0));
    assert!(!rig.hot_has("b").await);
    assert!(done.mismatched.is_empty());
    assert_eq!(rig.hot.get("a", None).await.unwrap(), thirty("z"));
    assert_eq!(rig.cold.get("a", None).await.unwrap(), thirty("a"));
    assert_eq!(cold.calls_on(Op::Get, "a"), 0);
    assert_eq!(
        rig.shape("a").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
}

/// Asserts that a key with an open entry whose hot object is not the cold one (a put landed over
/// it and its owner is still at work) is neither read nor reported: the entry is looked up before
/// the bytes of a key that is compared, so no mismatch is counted for the put in flight, and the
/// entry is as its owner left it.
#[tokio::test]
async fn a_key_with_an_open_entry_over_other_bytes_is_neither_compared_nor_reported() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    rig.hot.put("a", thirty("z")).await.unwrap();
    rig.cold.put("a", thirty("a")).await.unwrap();
    for key in ["b", "c"] {
        rig.clock.advance(1000);
        rig.hot.put(key, thirty(key)).await.unwrap();
        rig.cold.put(key, thirty(key)).await.unwrap();
    }
    rig.plant("a", EntryOp::Put, EntryState::Held, Some("p1"))
        .await;
    let (_hot, cold, mut r) = counted(&rig);
    let step = r.step().await;
    assert_eq!((step.evicted, step.failed.len()), (1, 0));
    assert!(step.mismatched.is_empty());
    assert_eq!(cold.calls_on(Op::Get, "a"), 0);
    assert_eq!(rig.hot.get("a", None).await.unwrap(), thirty("z"));
    let held = Some((EntryOp::Put, EntryState::Held, Some("p1".to_owned())));
    assert_eq!(rig.shape("a").await, held);
}

/// Asserts that a hot object that is replaced or deleted after the eviction has the entry and has
/// looked at the cold object again, right before its delete, is kept (replaced) or already gone
/// (deleted), whether the store deletes on a tag or is given a head and a plain delete: the
/// replacement is not deleted, the cold object stays, and the entry of the eviction is removed.
#[tokio::test]
async fn an_object_changed_just_before_the_hot_delete_is_kept_whatever_the_store_can_do() {
    for (on_a_tag, replaced) in [(true, true), (true, false), (false, true), (false, false)] {
        let rig = Rig::new();
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let gated = Arc::new(GateBackend::new(hot.clone()));
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let mut r = rig.over(gated.clone(), cold, "r1", small);
        copied_then_crowded(&rig, &mut r).await;
        if !on_a_tag {
            hot.fail_next_on(Op::DeleteIfMatch, "a", Error::Unsupported("tag".to_owned()));
        }
        let gate = gated.gate(Op::DeleteIfMatch, "a", Phase::Pre);
        let mut step = Box::pin(r.step());
        park(&mut step, &gate).await;
        let case = format!("on a tag {on_a_tag}, replaced {replaced}");
        assert_eq!(
            rig.shape("a").await,
            Some((EntryOp::Evict, EntryState::Sealed, None)),
            "{case}"
        );
        rig.hot.delete("a").await.unwrap();
        if replaced {
            rig.hot.put("a", thirty("z")).await.unwrap();
        }
        gate.release();
        let done = step.await;
        // `b`, the next oldest, goes.
        assert_eq!((done.evicted, done.failed.len()), (1, 0), "{case}");
        assert!(!rig.hot_has("b").await, "{case}");
        assert_eq!(rig.hot_has("a").await, replaced, "{case}");
        assert!(rig.cold_has("a").await, "{case}");
        assert!(rig.entry("a").await.is_none(), "{case}");
    }
}

/// Asserts that the lease is checked again after the head that a store without a delete on a tag
/// is given: a lease that has run short during the head is renewed with one write and the object
/// is deleted, and when the renewal fails the object stays, the item fails, and the entry is
/// removed.
#[tokio::test]
async fn a_lease_that_runs_short_during_the_head_before_a_plain_delete_is_renewed_or_stops_it() {
    for renewal_fails in [false, true] {
        let rig = Rig::new();
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let gated = Arc::new(GateBackend::new(hot.clone()));
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let mut r = rig.over(gated.clone(), cold, "r1", small);
        copied_then_crowded(&rig, &mut r).await;
        hot.fail_next_on(Op::DeleteIfMatch, "a", Error::Unsupported("tag".to_owned()));
        let gate = gated.gate(Op::DeleteIfMatch, "a", Phase::Pre);
        let mut step = Box::pin(r.step());
        park(&mut step, &gate).await;
        rig.mono.advance(Duration::from_millis(10_001));
        if renewal_fails {
            let error = Error::Unavailable("lease".to_owned());
            hot.fail_next_on(Op::PutIfMatch, LEASE_KEY, error);
        }
        let writes = hot.calls_on(Op::PutIfMatch, LEASE_KEY);
        gate.release();
        let done = step.await;
        let case = format!("renewal fails {renewal_fails}");
        assert_eq!(rig.hot_has("a").await, renewal_fails, "{case}");
        assert_eq!(done.failed.len(), usize::from(renewal_fails), "{case}");
        assert_eq!(
            hot.calls_on(Op::PutIfMatch, LEASE_KEY),
            writes + 1,
            "{case}"
        );
        assert!(rig.entry("a").await.is_none(), "{case}");
    }
}
