//! The store calls of the copy of one put to the cold tier: how many each tier is asked for, and
//! what is done when the create-only write is refused or its outcome is not known.

use super::*;
use crate::{Listed, PutAnswer};

/// The cold tier of a store whose answer to a write has no tag: everything but the write is the
/// memory backend's.
pub(super) struct Untagged(pub(super) Arc<MemoryBackend>);

#[async_trait]
impl Backend for Untagged {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.0.put(key, body).await
    }

    async fn put_stream_answered(
        &self,
        key: &str,
        len: Option<u64>,
        body: crate::PartBody<'_>,
        _hot_only: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        self.0.put_stream(key, len, body).await?;
        Ok(None)
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.0.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<crate::ByteRange>) -> Result<Bytes, Error> {
        self.0.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<crate::Meta, Error> {
        self.0.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.0.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.0.list(prefix).await
    }
}

/// The calls on `key` that `tier` took, in order.
fn calls(tier: &FaultyBackend, key: &str) -> Vec<Op> {
    let log = tier.call_log();
    log.into_iter()
        .filter(|(_, called)| called == key)
        .map(|(op, _)| op)
        .collect()
}

/// A rig with one landed put of `key` holding `value` in the hot tier, and the two tiers wrapped
/// to count their calls (the cold tier over `cold`).
async fn landed(
    value: &'static str,
    cold: Arc<dyn Backend>,
) -> (Rig, Arc<FaultyBackend>, Arc<FaultyBackend>) {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes(value)).await.unwrap();
    land(&rig, "p1", "k").await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    (rig, hot, Arc::new(FaultyBackend::new(cold)))
}

/// Asserts that the copy of a small object asks the hot tier for one read of the object (no HEAD of
/// it) and the cold tier for one write (no HEAD before or after it), when the store's answer to
/// the write has the tag, and that the tags of the two objects are remembered for the eviction.
#[tokio::test]
async fn a_copy_is_one_read_of_the_hot_object_and_one_create_only_write_to_the_cold_tier() {
    let cold = Arc::new(MemoryBackend::new());
    let (rig, hot, cold_calls) = landed("value", cold.clone()).await;
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
    let (rig, hot, cold_calls) = landed("value", Arc::new(Untagged(cold.clone()))).await;
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
        let (rig, hot, cold_calls) = landed(value, cold.clone()).await;
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
    let (rig, hot, cold_calls) = landed("value", cold.clone()).await;
    cold_calls.fail_next_on(Op::Put, "k", error.clone());
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&cold_calls, "k"), [Op::Put, Op::Head, Op::Get]);
    assert!(rig.entry("k").await.is_none());

    for refused in [error, Error::AlreadyExists("k".to_owned())] {
        let cold = Arc::new(MemoryBackend::new());
        let (rig, hot, cold_calls) = landed("value", cold.clone()).await;
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
    let (rig, hot, cold_calls) = landed("value", cold).await;
    let full = Error::Full("cold".to_owned());
    cold_calls.fail_next_on(Op::Put, "k", full.clone());
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed, [full]);
    assert_eq!(calls(&cold_calls, "k"), [Op::Put]);

    let (rig, hot, cold_calls) = landed("value", Arc::new(MemoryBackend::new())).await;
    rig.hot.delete("k").await.unwrap();
    let mut r = rig.over(hot.clone(), cold_calls.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert_eq!(calls(&hot, "k"), [Op::Get]);
    assert!(cold_calls.call_log().iter().all(|(_, key)| key != "k"));
}
