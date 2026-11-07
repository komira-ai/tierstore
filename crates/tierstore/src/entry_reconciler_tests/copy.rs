//! The store calls of the copy of one put to the cold tier: how many each tier is asked for, and
//! what is done when the create-only write is refused or its outcome is not known.

use super::*;

/// The calls on `key` that `tier` took, in order.
fn calls(tier: &FaultyBackend, key: &str) -> Vec<Op> {
    let log = tier.call_log();
    log.into_iter()
        .filter(|(_, called)| called == key)
        .map(|(op, _)| op)
        .collect()
}

/// A rig with one landed put of `key` holding `value` in the hot tier, and the two tiers wrapped
/// to count their calls (the cold tier is `cold`, which wraps the store of the test).
async fn landed(
    value: &'static str,
    cold: FaultyBackend,
) -> (Rig, Arc<FaultyBackend>, Arc<FaultyBackend>) {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes(value)).await.unwrap();
    land(&rig, "p1", "k").await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    (rig, hot, Arc::new(cold))
}

/// Asserts that the copy of a small object asks the hot tier for one read of the object (no HEAD of
/// it) and the cold tier for one write (no HEAD before or after it), when the store's answer to
/// the write has the tag, and that the tags of the two objects are remembered for the eviction.
#[tokio::test]
async fn a_copy_is_one_read_of_the_hot_object_and_one_create_only_write_to_the_cold_tier() {
    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) = landed("value", FaultyBackend::new(cold.clone())).await;
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&hot, "k"), [Op::Get]);
    assert_eq!(calls(&cold_calls, "k"), [Op::Put]);
    assert_eq!(cold.get("k", None).await.unwrap(), bytes("value"));
    assert!(rig.entry("k").await.is_none());
    let hot_tag = rig.hot.head("k").await.unwrap().tag;
    let cold_tag = cold.head("k").await.unwrap().tag;
    assert!(r.tags().matches("k", &hot_tag, &cold_tag));
}

/// Asserts that a store whose answer to the write has no tag is asked for the tag by one HEAD after
/// the write, and not before it, and that the tags are remembered.
#[tokio::test]
async fn a_write_answered_without_a_tag_is_followed_by_one_head_for_the_tag() {
    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) =
        landed("value", FaultyBackend::new(cold.clone()).without_answers()).await;
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&cold_calls, "k"), [Op::Put, Op::Head]);
    let hot_tag = rig.hot.head("k").await.unwrap().tag;
    let cold_tag = cold.head("k").await.unwrap().tag;
    assert!(r.tags().matches("k", &hot_tag, &cold_tag));
}

/// Asserts that a copy whose write is refused because the cold tier has the key reads the cold
/// object with one HEAD and compares: the same bytes are adopted (the entry is removed, the cold
/// object is as it was), different bytes are reported as a mismatch and neither object is touched.
#[tokio::test]
async fn a_refused_write_is_resolved_by_a_head_and_a_comparison_of_the_bytes() {
    for (value, cold_value, same) in [("abc", "abc", true), ("abc", "abd", false)] {
        let cold = Arc::new(MemoryBackend::new());
        cold.put("k", bytes(cold_value)).await.unwrap();
        let (rig, hot, cold_calls) = landed(value, FaultyBackend::new(cold.clone())).await;
        let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
        let step = r.step().await;
        assert_eq!(step.changed, 1, "{cold_value}");
        assert!(step.failed.is_empty(), "{cold_value}");
        let mismatched: &[String] = if same { &[] } else { &["k".to_owned()] };
        assert_eq!(step.mismatched, mismatched, "{cold_value}");
        assert_eq!(
            calls(&cold_calls, "k"),
            [Op::Put, Op::Head, Op::Get],
            "{cold_value}"
        );
        assert_eq!(cold.get("k", None).await.unwrap(), bytes(cold_value));
        assert_eq!(rig.hot.get("k", None).await.unwrap(), bytes(value));
        assert!(rig.entry("k").await.is_none(), "{cold_value}");
    }
}

/// Asserts that a write whose outcome is not known (the connection was reset after the object
/// landed) is resolved by a HEAD of the cold tier: the object is there with the same bytes, so the
/// copy is adopted; the object is not there, so the failure of the write is the failure of the item
/// and the entry stays held for the next step.
#[tokio::test]
async fn a_write_with_an_unknown_outcome_is_resolved_by_a_head() {
    let error = Error::Unavailable("reset".to_owned());
    let cold = Arc::new(MemoryBackend::new());
    cold.put("k", bytes("value")).await.unwrap();
    let (rig, hot, cold_calls) = landed("value", FaultyBackend::new(cold.clone())).await;
    cold_calls.fail_next_on(Op::Put, "k", error.clone());
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&cold_calls, "k"), [Op::Put, Op::Head, Op::Get]);
    assert!(rig.entry("k").await.is_none());

    for refused in [error, Error::AlreadyExists("k".to_owned())] {
        let cold = Arc::new(MemoryBackend::new());
        let (rig, hot, cold_calls) = landed("value", FaultyBackend::new(cold.clone())).await;
        cold_calls.fail_next_on(Op::Put, "k", refused.clone());
        let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
        let step = r.step().await;
        assert_eq!(step.failed, [refused]);
        assert_eq!(step.changed, 0);
        assert_eq!(calls(&cold_calls, "k"), [Op::Put, Op::Head]);
        assert!(!rig.cold_has("k").await);
        assert_eq!(
            rig.shape("k").await,
            Some((EntryOp::Put, EntryState::Held, Some("r1".to_owned())))
        );
    }
}

/// Asserts that a failure of the write that cannot mean the object landed (the tier has no room)
/// is the failure of the item with no HEAD, and that a hot object that is gone costs the hot tier
/// one read and the cold tier nothing.
#[tokio::test]
async fn a_full_cold_tier_fails_the_item_without_a_head_and_a_gone_hot_object_costs_no_cold_call() {
    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) = landed("value", FaultyBackend::new(cold)).await;
    let full = Error::Full("cold".to_owned());
    cold_calls.fail_next_on(Op::Put, "k", full.clone());
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed, [full]);
    assert_eq!(calls(&cold_calls, "k"), [Op::Put]);

    let (rig, hot, cold_calls) =
        landed("value", FaultyBackend::new(Arc::new(MemoryBackend::new()))).await;
    rig.hot.delete("k").await.unwrap();
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&hot, "k"), [Op::Get]);
    assert!(cold_calls.call_log().iter().all(|(_, key)| key != "k"));
}

/// Asserts that a copy of an object longer than the cold backend writes in one request asks the
/// cold tier with a HEAD before the write, so that a retry of a copy that landed sends no part: an
/// object that is there with the same bytes is adopted by a comparison with no write, one with
/// other bytes is a mismatch with no write, none is written after the HEAD, and a HEAD that fails
/// fails the item before any write. An object of the limit or less is written with no HEAD first.
#[tokio::test]
async fn a_copy_above_the_single_put_limit_looks_at_the_cold_tier_first_and_one_below_does_not() {
    for (cold_value, same) in [("value", true), ("valuf", false)] {
        let cold = Arc::new(MemoryBackend::new());
        cold.put("k", bytes(cold_value)).await.unwrap();
        let (rig, hot, cold_calls) = landed(
            "value",
            FaultyBackend::new(cold.clone()).with_single_put_max(4),
        )
        .await;
        let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
        let step = r.step().await;
        let mismatched: &[String] = if same { &[] } else { &["k".to_owned()] };
        assert_eq!(step.mismatched, mismatched, "{cold_value}");
        assert_eq!(calls(&cold_calls, "k"), [Op::Head, Op::Get], "{cold_value}");
        assert_eq!(cold.get("k", None).await.unwrap(), bytes(cold_value));
    }

    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) = landed(
        "value",
        FaultyBackend::new(cold.clone()).with_single_put_max(4),
    )
    .await;
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&cold_calls, "k"), [Op::Head, Op::Put]);
    assert_eq!(cold.get("k", None).await.unwrap(), bytes("value"));
    assert!(rig.entry("k").await.is_none());

    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) = landed(
        "value",
        FaultyBackend::new(cold.clone()).with_single_put_max(4),
    )
    .await;
    let unread = Error::Unavailable("cold".to_owned());
    cold_calls.fail_next_on(Op::Head, "k", unread.clone());
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed, [unread]);
    assert_eq!(calls(&cold_calls, "k"), [Op::Head]);
    assert!(!rig.cold_has("k").await);

    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) = landed(
        "value",
        FaultyBackend::new(cold.clone()).with_single_put_max(5),
    )
    .await;
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&cold_calls, "k"), [Op::Put]);
}
