//! The puts of a proxy whose format is active: where each kind of put lands, what it leaves in the
//! entry, and what it answers when a tier or an entry is in the way.
//!
//! Every test makes the proxy live by moving its `live_until` on by hand and never waits: no
//! assertion is about time.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;

use crate::Backend;
use crate::entry::{Entries, EntryOp, EntryState, Seen, Take, entry_key};
use crate::journal::{FORMAT_KEY, USAGE_KEY, Usage};
use crate::{
    Error, FaultyBackend, GateBackend, MemoryBackend, Mono, Op, PartBody, Proxy, ProxyConfig,
    TestClock,
};
use futures::StreamExt;

/// The format object of a store whose writes are made on entries.
pub(super) const ACTIVE_FORMAT: &str = r#"{"epoch":2,"phase":"active"}"#;
/// The hot tier's budget, in bytes.
pub(super) const BUDGET: u64 = 1000;

pub(super) fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// A body of the chunks `parts`.
pub(super) fn chunks(parts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts.iter().map(|part| Ok(body(part))).collect();
    stream::iter(items).boxed()
}

/// A proxy over a hot tier behind a gate and a fault injector and a cold tier behind a fault
/// injector, whose format object is active.
pub(super) struct Rig {
    pub(super) clock: Arc<TestClock>,
    pub(super) mono: Arc<Mono>,
    pub(super) hot_mem: Arc<MemoryBackend>,
    pub(super) cold_mem: Arc<MemoryBackend>,
    pub(super) hot_faults: Arc<FaultyBackend>,
    pub(super) hot: Arc<GateBackend>,
    pub(super) cold: Arc<FaultyBackend>,
    pub(super) proxy: Arc<Proxy>,
}

impl Rig {
    /// A live proxy `p1` whose format says `format` (none if empty).
    pub(super) async fn with_format(format: &str) -> Self {
        Self::build(format, 3).await
    }

    /// As [`Rig::with_format`], with `entry_rounds` looks at an entry for a put.
    pub(super) async fn build(format: &str, entry_rounds: u32) -> Self {
        Self::build_waiting(format, entry_rounds, crate::DeleteWait::new()).await
    }

    /// As [`Rig::build`], with the wait of a delete for the release of an entry.
    pub(super) async fn build_waiting(
        format: &str,
        entry_rounds: u32,
        delete_wait: crate::DeleteWait,
    ) -> Self {
        let clock = Arc::new(TestClock::new());
        let mono = Arc::new(Mono::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        if !format.is_empty() {
            hot_mem.put(FORMAT_KEY, body(format)).await.unwrap();
        }
        let hot_faults = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let hot = Arc::new(GateBackend::new(hot_faults.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let mut config = ProxyConfig::new("p1", BUDGET, clock.clone());
        config.mono = mono.clone();
        config.usage_refresh = Duration::ZERO;
        config.format_refresh = Duration::ZERO;
        config.entry_rounds = entry_rounds;
        config.delete_wait = delete_wait;
        let proxy = Proxy::open(hot.clone(), cold.clone(), config)
            .await
            .unwrap();
        let rig = Self {
            clock,
            mono,
            hot_mem,
            cold_mem,
            hot_faults,
            hot,
            cold,
            proxy: Arc::new(proxy),
        };
        rig.live();
        rig
    }

    pub(super) async fn active() -> Self {
        Self::with_format(ACTIVE_FORMAT).await
    }

    /// Moves `live_until` of the proxy to ten minutes from now.
    pub(super) fn live(&self) {
        let until = crate::Clock::now(&*self.mono) + 600_000;
        self.proxy.inner.heartbeat.deadline.extend_to(until);
    }

    /// Moves the monotonic clock past `live_until`.
    pub(super) fn stop_being_live(&self) {
        self.mono.advance(Duration::from_secs(3600));
    }

    /// Says the hot tier is full.
    pub(super) async fn fill_hot(&self) {
        let usage = Usage {
            hot_bytes: BUDGET,
            copy_lag: 0,
            time: 0,
        };
        let json = serde_json::to_vec(&usage).unwrap();
        self.hot_mem
            .put(USAGE_KEY, Bytes::from(json))
            .await
            .unwrap();
    }

    /// The calls on entries as another actor.
    pub(super) fn other(&self) -> Entries {
        Entries::new(self.hot_mem.clone(), "other", self.clock.clone())
    }

    /// The entry of `key`, read from the hot tier.
    pub(super) async fn entry(&self, key: &str) -> Option<Seen> {
        self.other().read(key).await.unwrap()
    }

    /// The shape of the entry of `key`: operation, state and owner.
    pub(super) async fn shape(&self, key: &str) -> Option<(EntryOp, EntryState, Option<String>)> {
        let seen = self.entry(key).await?;
        Some((seen.entry.op, seen.entry.st, seen.entry.owner))
    }

    pub(super) async fn hot_has(&self, key: &str) -> bool {
        self.hot_mem.head(key).await.is_ok()
    }

    pub(super) async fn cold_has(&self, key: &str) -> bool {
        self.cold_mem.head(key).await.is_ok()
    }
}

pub(super) fn pending() -> Option<(EntryOp, EntryState, Option<String>)> {
    Some((EntryOp::Put, EntryState::Pending, None))
}

pub(super) fn held_by(owner: &str) -> Option<(EntryOp, EntryState, Option<String>)> {
    Some((EntryOp::Put, EntryState::Held, Some(owner.to_owned())))
}

/// Asserts that a put in the active format lands in the hot tier and leaves its entry
/// `(PUT, PENDING)` with no owner, and that nothing is journaled: the entry is what the
/// reconciler finds. Also that the cold tier has nothing yet.
#[tokio::test]
async fn a_put_lands_in_hot_and_leaves_a_pending_entry() {
    let rig = Rig::active().await;
    rig.proxy.put("k", body("v")).await.unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, pending());
    let journal = rig.hot_mem.list("_journal/").await.unwrap();
    assert!(journal.is_empty(), "{journal:?}");
}

/// Asserts that a streamed put lands in the hot tier whole and leaves a `(PUT, PENDING)` entry.
#[tokio::test]
async fn a_streamed_put_lands_in_hot_and_leaves_a_pending_entry() {
    let rig = Rig::active().await;
    rig.proxy
        .put_stream("k", Some(3), chunks(&["a", "bc"]))
        .await
        .unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("abc"));
    assert_eq!(rig.shape("k").await, pending());
}

/// Asserts that a presigned put (a hot-only streamed put) lands in the hot tier and leaves a
/// `(PUT, PENDING)` entry.
#[tokio::test]
async fn a_presigned_put_lands_in_hot_and_leaves_a_pending_entry() {
    let rig = Rig::active().await;
    let put = rig.proxy.put_stream_hot_only("k", Some(1), chunks(&["v"]));
    put.await.unwrap();
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("v"));
    assert_eq!(rig.shape("k").await, pending());
}

/// Asserts that while the hot tier is full a held put and a streamed put land in the cold tier,
/// create-only, with nothing in the hot tier, and leave `(PUT, PENDING)` entries.
#[tokio::test]
async fn a_put_at_full_lands_in_cold_and_leaves_a_pending_entry() {
    let rig = Rig::active().await;
    rig.fill_hot().await;
    rig.proxy.put("held", body("v")).await.unwrap();
    let streamed = rig.proxy.put_stream("streamed", Some(1), chunks(&["w"]));
    streamed.await.unwrap();
    for (key, text) in [("held", "v"), ("streamed", "w")] {
        assert_eq!(rig.cold_mem.get(key, None).await.unwrap(), body(text));
        assert!(!rig.hot_has(key).await);
        assert_eq!(rig.shape(key).await, pending());
    }
}

/// Asserts that a presigned put while the hot tier is full is refused with `Full`, which the S3
/// front answers 503, and leaves no object and no entry.
#[tokio::test]
async fn a_presigned_put_at_full_is_refused_and_leaves_nothing() {
    let rig = Rig::active().await;
    rig.fill_hot().await;
    let put = rig.proxy.put_stream_hot_only("k", Some(1), chunks(&["v"]));
    assert_eq!(put.await, Err(Error::Full("k".to_owned())));
    assert!(!rig.hot_has("k").await && !rig.cold_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a held put and a streamed put whose hot write is answered `Full` before it took a
/// byte land in the cold tier instead and leave `(PUT, PENDING)` entries, and that a presigned put
/// in the same case is refused with `Full` and leaves no entry.
#[tokio::test]
async fn a_hot_tier_that_answers_full_sends_a_put_to_cold_and_refuses_a_presigned_one() {
    let rig = Rig::active().await;
    let full = || Error::Full("k".to_owned());
    rig.hot_faults.fail_next_on(Op::Put, "held", full());
    rig.proxy.put("held", body("v")).await.unwrap();
    rig.hot_faults.fail_next_on(Op::Put, "streamed", full());
    let streamed = rig.proxy.put_stream("streamed", Some(1), chunks(&["w"]));
    streamed.await.unwrap();
    for (key, text) in [("held", "v"), ("streamed", "w")] {
        assert_eq!(rig.cold_mem.get(key, None).await.unwrap(), body(text));
        assert!(!rig.hot_has(key).await);
        assert_eq!(rig.shape(key).await, pending());
    }
    rig.hot_faults.fail_next_on(Op::Put, "presigned", full());
    let put = rig
        .proxy
        .put_stream_hot_only("presigned", Some(1), chunks(&["x"]));
    assert_eq!(put.await, Err(Error::Full("presigned".to_owned())));
    assert_eq!(rig.shape("presigned").await, None);
    assert!(!rig.cold_has("presigned").await);
}

/// Asserts that the completion of a multipart upload makes the object in the cold tier and leaves
/// a `(PUT, PENDING)` entry, with nothing in the hot tier.
#[tokio::test]
async fn a_multipart_completion_lands_in_cold_and_leaves_a_pending_entry() {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let tag = rig
        .proxy
        .upload_part("k", &id, 1, chunks(&["ab"]))
        .await
        .unwrap();
    rig.proxy
        .complete_upload("k", &id, &[(1, tag)])
        .await
        .unwrap();
    assert_eq!(rig.cold_mem.get("k", None).await.unwrap(), body("ab"));
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, pending());
}

/// Asserts that a completion the cold tier refuses (a part it does not know) is answered with the
/// cold tier's error and removes its entry, and that a completion the cold tier did not answer is
/// answered `Unavailable` and leaves the entry pending, for the reconciler to find the object or
/// none.
#[tokio::test]
async fn a_multipart_completion_that_fails_removes_its_entry_and_one_without_an_answer_hands_it_over()
 {
    let rig = Rig::active().await;
    let id = rig.proxy.create_upload("k").await.unwrap();
    let tag = rig
        .proxy
        .upload_part("k", &id, 1, chunks(&["ab"]))
        .await
        .unwrap();
    let invalid = Error::InvalidPart("1".to_owned());
    rig.cold.fail_next_with(Op::CompleteUpload, invalid.clone());
    let done = rig
        .proxy
        .complete_upload("k", &id, &[(1, tag.clone())])
        .await;
    assert_eq!(done, Err(invalid));
    assert_eq!(rig.shape("k").await, None);
    rig.cold.fail_next(Op::CompleteUpload, 1);
    let lost = rig.proxy.complete_upload("k", &id, &[(1, tag)]).await;
    assert!(matches!(lost, Err(Error::Unavailable(_))), "{lost:?}");
    assert_eq!(rig.shape("k").await, pending());
}

/// Asserts that a put of a key the hot tier has is refused as existing before any entry is made:
/// the hot tier saw no write at an entry, and the object is as it was.
#[tokio::test]
async fn a_put_of_a_key_the_hot_tier_has_is_refused_and_makes_no_entry() {
    let rig = Rig::active().await;
    rig.hot_mem.put("k", body("old")).await.unwrap();
    let put = rig.proxy.put("k", body("new")).await;
    assert_eq!(put, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.hot_faults.calls_on(Op::Put, &entry_key("k")), 0);
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("old"));
}

/// Asserts that a second put of the very bytes of the first is refused as existing and changes
/// nothing: the stored bytes, the entry and the cold tier are as the first put left them.
#[tokio::test]
async fn a_put_of_identical_bytes_again_is_refused_and_changes_nothing() {
    let rig = Rig::active().await;
    rig.proxy.put("k", body("same")).await.unwrap();
    let before = rig.entry("k").await;
    let again = rig.proxy.put("k", body("same")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.entry("k").await, before);
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("same"));
    assert!(!rig.cold_has("k").await);
}

/// Asserts that a put of a key only the cold tier has (it was evicted from the hot tier) is
/// refused as existing, and that the entry it took is removed.
#[tokio::test]
async fn a_put_of_a_key_the_cold_tier_has_is_refused_and_removes_its_entry() {
    let rig = Rig::active().await;
    rig.cold_mem.put("k", body("old")).await.unwrap();
    let put = rig.proxy.put("k", body("new")).await;
    assert_eq!(put, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.shape("k").await, None);
    assert!(!rig.hot_has("k").await);
}

/// Asserts that while the cold tier is down a put of a new key is answered `Unavailable`, whether
/// the cold tier says so or fails in another way, and leaves no object and no entry.
#[tokio::test]
async fn a_put_of_a_new_key_while_the_cold_tier_is_down_is_unavailable_and_leaves_nothing() {
    let rig = Rig::active().await;
    rig.cold.set_down(true);
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    rig.cold.set_down(false);
    rig.cold
        .fail_next_with(Op::Head, Error::Other("broken".to_owned()));
    let other = rig.proxy.put("k", body("v")).await;
    let Err(Error::Unavailable(text)) = other else {
        panic!("{other:?}");
    };
    assert!(text.contains("broken"), "{text}");
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
    rig.proxy.put("k", body("v")).await.unwrap();
}

/// Asserts that a put that meets the entry of a put in flight, or of a delete, is answered
/// `Unavailable` and changes neither the entry nor the tiers.
#[tokio::test]
async fn a_put_that_meets_an_entry_in_flight_is_unavailable_and_changes_nothing() {
    let rig = Rig::active().await;
    for (key, take) in [("put", Take::Put), ("del", Take::Del)] {
        rig.other().create(key, take).await.unwrap();
        let before = rig.entry(key).await;
        let put = rig.proxy.put(key, body("v")).await;
        assert!(matches!(put, Err(Error::Unavailable(_))), "{key}: {put:?}");
        assert_eq!(rig.entry(key).await, before, "{key}");
        assert!(!rig.hot_has(key).await && !rig.cold_has(key).await, "{key}");
    }
}

/// Asserts that a put that meets a settled entry (a pending put, a pending copy-back, a sealed
/// eviction) of a key that no tier holds is answered `Unavailable`, and that one that meets a
/// sealed entry of a key the cold tier holds is refused as existing.
#[tokio::test]
async fn a_put_that_meets_a_settled_entry_is_answered_from_the_tiers() {
    let rig = Rig::active().await;
    for (key, take) in [
        ("stray", Take::Stray),
        ("back", Take::CopyBack),
        ("evict", Take::Evict),
    ] {
        rig.other().create(key, take).await.unwrap();
        let put = rig.proxy.put(key, body("v")).await;
        assert!(matches!(put, Err(Error::Unavailable(_))), "{key}: {put:?}");
    }
    rig.cold_mem.put("evict", body("old")).await.unwrap();
    let put = rig.proxy.put("evict", body("v")).await;
    assert_eq!(put, Err(Error::AlreadyExists("evict".to_owned())));
    assert!(!rig.hot_has("evict").await);
}

/// Asserts that a proxy that is not live refuses a put with `Unavailable` before it reads or
/// writes anything of the key.
#[tokio::test]
async fn a_put_of_a_proxy_that_is_not_live_is_unavailable_and_touches_nothing() {
    let rig = Rig::active().await;
    rig.stop_being_live();
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    let streamed = rig.proxy.put_stream("s", None, chunks(&["v"])).await;
    assert!(
        matches!(streamed, Err(Error::Unavailable(_))),
        "{streamed:?}"
    );
    for key in ["k", "s"] {
        assert_eq!(rig.hot_faults.calls_on(Op::Head, key), 0);
        assert_eq!(rig.cold.calls_on(Op::Head, key), 0);
        assert_eq!(rig.hot_faults.calls_on(Op::Put, &entry_key(key)), 0);
    }
}

/// Asserts that a put is refused with `Unavailable` and the text that says writes wait for the
/// active format, and makes no entry and writes no object, while the format object is missing,
/// says `active` with no epoch or with epoch 1, or is read as another phase; and that it goes to an
/// entry once the object says epoch 2 (the proxy reads the object at every write here).
#[tokio::test]
async fn a_put_is_made_on_an_entry_only_once_the_format_is_active_with_epoch_2() {
    let rig = Rig::with_format("").await;
    let formats = [
        None,
        Some(r#"{"phase":"active"}"#),
        Some(r#"{"epoch":1,"phase":"active"}"#),
        Some(r#"{"epoch":2,"phase":"someday"}"#),
    ];
    for (n, format) in formats.into_iter().enumerate() {
        let _ = rig.hot_mem.delete(FORMAT_KEY).await;
        if let Some(json) = format {
            rig.hot_mem.put(FORMAT_KEY, body(json)).await.unwrap();
        }
        let key = format!("waiting-{n}");
        let put = rig.proxy.put(&key, body("v")).await;
        let Err(Error::Unavailable(text)) = put else {
            panic!("{format:?}: {put:?}");
        };
        let wait = "writes wait until the format object says active";
        assert!(text.contains(wait), "{format:?}: {text}");
        assert_eq!(rig.shape(&key).await, None, "{format:?}");
        assert!(!rig.hot_has(&key).await, "{format:?}");
    }
    let _ = rig.hot_mem.delete(FORMAT_KEY).await;
    rig.hot_mem
        .put(FORMAT_KEY, body(ACTIVE_FORMAT))
        .await
        .unwrap();
    rig.proxy.put("entered", body("v")).await.unwrap();
    assert_eq!(rig.shape("entered").await, pending());
}

/// Asserts that a put whose entry cannot be created (the hot tier did not answer) is answered
/// `Unavailable` and leaves no object.
#[tokio::test]
async fn a_put_whose_entry_cannot_be_created_is_unavailable_and_leaves_no_object() {
    let rig = Rig::active().await;
    let unavailable = Error::Unavailable("down".to_owned());
    rig.hot_faults
        .fail_next_on(Op::Put, "_entries/", unavailable);
    let put = rig.proxy.put("k", body("v")).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a put whose acknowledging write of the entry fails, whether the store says
/// unavailable (and the entry is read back unchanged) or another error, is answered
/// `Unavailable`, and that the entry is left held by the put for the reconciler.
#[tokio::test]
async fn a_put_whose_acknowledgement_cannot_be_written_is_unavailable_and_leaves_the_entry() {
    let rig = Rig::active().await;
    let unavailable = Error::Unavailable("down".to_owned());
    rig.hot_faults
        .fail_next_on(Op::PutIfMatch, "_entries/", unavailable);
    let first = rig.proxy.put("a", body("v")).await;
    assert!(matches!(first, Err(Error::Unavailable(_))), "{first:?}");
    assert_eq!(rig.shape("a").await, held_by("p1"));
    let refused = Error::Full("x".to_owned());
    rig.hot_faults
        .fail_next_on(Op::PutIfMatch, "_entries/", refused);
    let Err(Error::Unavailable(text)) = rig.proxy.put("b", body("v")).await else {
        panic!("not unavailable");
    };
    assert!(text.contains("could not be written"), "{text}");
    assert_eq!(rig.shape("b").await, held_by("p1"));
}

/// Asserts that a streamed body that ends with an error leaves no object and removes the entry,
/// and answers with the body's error.
#[tokio::test]
async fn a_streamed_body_that_fails_leaves_no_object_and_removes_its_entry() {
    let rig = Rig::active().await;
    let items: Vec<Result<Bytes, Error>> =
        vec![Ok(body("a")), Err(Error::Other("reset".to_owned()))];
    let put = rig
        .proxy
        .put_stream("k", None, stream::iter(items).boxed())
        .await;
    assert_eq!(put, Err(Error::Other("reset".to_owned())));
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a streamed put whose landing the store did not answer is answered `Unavailable`
/// and leaves the entry pending, for the reconciler to find the object or none.
#[tokio::test]
async fn a_streamed_put_whose_landing_gets_no_answer_hands_its_entry_over() {
    let rig = Rig::active().await;
    let unavailable = Error::Unavailable("down".to_owned());
    rig.hot_faults.fail_next_on(Op::Put, "k", unavailable);
    let put = rig.proxy.put_stream("k", None, chunks(&["v"])).await;
    assert!(matches!(put, Err(Error::Unavailable(_))), "{put:?}");
    assert_eq!(rig.shape("k").await, pending());
    assert!(!rig.hot_has("k").await);
}

/// Asserts that a put whose first look at the hot tier fails is answered with that failure and
/// makes no entry.
#[tokio::test]
async fn a_put_whose_first_look_at_the_hot_tier_fails_makes_no_entry() {
    let rig = Rig::active().await;
    let down = Error::Unavailable("down".to_owned());
    rig.hot_faults.fail_next_on(Op::Head, "k", down.clone());
    assert_eq!(rig.proxy.put("k", body("v")).await, Err(down));
    assert_eq!(rig.hot_faults.calls_on(Op::Put, &entry_key("k")), 0);
    assert_eq!(rig.shape("k").await, None);
}

/// Asserts that a put whose acknowledging write of the entry was not answered, and whose entry
/// could not be read back either, is answered with the write's own `Unavailable` and leaves the
/// entry held.
#[tokio::test]
async fn a_put_whose_acknowledgement_and_read_back_both_fail_is_unavailable_with_its_own_error() {
    let rig = Rig::active().await;
    let unanswered = Error::Unavailable("no answer".to_owned());
    rig.hot_faults
        .fail_next_on(Op::PutIfMatch, "_entries/", unanswered.clone());
    let unreadable = Error::Unavailable("no read".to_owned());
    rig.hot_faults
        .fail_next_on(Op::Head, &entry_key("k"), unreadable);
    assert_eq!(rig.proxy.put("k", body("v")).await, Err(unanswered));
    assert_eq!(rig.shape("k").await, held_by("p1"));
}
