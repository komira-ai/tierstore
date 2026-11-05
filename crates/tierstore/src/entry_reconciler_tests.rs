//! The entry reconciler: its gate on the format, the step of each state of an entry, the loop's
//! waits. The races between a delete and the reconciler, the stray sweep, and the usage and the
//! eviction are in the three modules below; every race is driven one store call at a time through
//! a gate, and no test reads a clock or waits on one.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use super::backoff::{Idle, retry_delay};
use super::{EntryReconciler, EntryReconcilerConfig, StepReport};
use crate::format::{Format, FormatPhase, format_body};
use crate::journal::{ENTRIES_PREFIX, FORMAT_KEY, LEASE_KEY, PROXIES_PREFIX, proxy_key};
use crate::{
    Backend, Created, Entries, Entry, EntryOp, EntryState, Error, FaultyBackend, Gate, GateBackend,
    MemoryBackend, Mono, Op, Outcome, Phase, ReconcilerConfig, RecordingSleeper, Seen, Sleeper,
    Step, Take, TestClock, encode_entry, entry_key,
};

#[path = "entry_reconciler_tests/copy_back.rs"]
mod copy_back;
#[path = "entry_reconciler_tests/evict.rs"]
mod evict;
#[path = "entry_reconciler_tests/races.rs"]
mod races;
#[path = "entry_reconciler_tests/sweep.rs"]
mod sweep;
#[path = "entry_reconciler_tests/usage.rs"]
mod usage;

/// A wait that never ends, for the renewal timer of the lease: no test sees a renewal.
struct Pending;

#[async_trait]
impl Sleeper for Pending {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// A hot tier and a cold tier in memory, the wall clock the hot tier stamps its objects with, the
/// monotonic clock of the reconciler, and the record of the loop's waits.
struct Rig {
    clock: Arc<TestClock>,
    mono: Arc<Mono>,
    hot: Arc<MemoryBackend>,
    cold: Arc<MemoryBackend>,
    waits: Arc<RecordingSleeper>,
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        Self {
            hot: Arc::new(MemoryBackend::with_clock(clock.clone())),
            cold: Arc::new(MemoryBackend::with_clock(clock.clone())),
            clock,
            mono: Arc::new(Mono::new()),
            waits: Arc::new(RecordingSleeper::new()),
        }
    }

    fn config(&self, holder: &str) -> EntryReconcilerConfig {
        let mut base = ReconcilerConfig::new(holder, 1 << 20, self.clock.clone());
        base.mono = self.mono.clone();
        base.sleeper = Arc::new(Pending);
        let mut config = EntryReconcilerConfig::new(base);
        config.waiter = self.waits.clone();
        config
    }

    /// A reconciler `holder` over the given tiers (a wrapper of the rig's) and `tweak`ed settings.
    fn over(
        &self,
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        holder: &str,
        tweak: impl FnOnce(&mut EntryReconcilerConfig),
    ) -> EntryReconciler {
        let mut config = self.config(holder);
        tweak(&mut config);
        EntryReconciler::new(hot, cold, config)
    }

    /// The reconciler `r1` over the rig's own tiers.
    fn reconciler(&self) -> EntryReconciler {
        self.over(self.hot.clone(), self.cold.clone(), "r1", |_| {})
    }

    async fn activate(&self) {
        let body = format_body(&Format {
            phase: FormatPhase::Active,
            epoch: None,
        });
        self.hot.put(FORMAT_KEY, body).await.unwrap();
    }

    /// The calls of the proxy `id`, as its entries.
    fn client(&self, id: &str) -> Entries {
        Entries::new(self.hot.clone(), id, self.clock.clone())
    }

    /// Registers the proxy `id`, as a heartbeat of it would have.
    async fn register(&self, id: &str) {
        let body = Bytes::from_static(b"{}");
        self.hot.put(&proxy_key(id), body).await.unwrap();
    }

    /// Writes the entry of `key` as it is, with a nonce.
    async fn plant(&self, key: &str, op: EntryOp, st: EntryState, owner: Option<&str>) {
        let entry = Entry {
            v: 1,
            key: key.to_owned(),
            op,
            st,
            owner: owner.map(str::to_owned),
            nonce: "planted".to_owned(),
            created_ms: 0,
        };
        self.hot
            .put(&entry_key(key), encode_entry(&entry))
            .await
            .unwrap();
    }

    async fn entry(&self, key: &str) -> Option<Seen> {
        self.client("reader").read(key).await.unwrap()
    }

    async fn shape(&self, key: &str) -> Option<(EntryOp, EntryState, Option<String>)> {
        let seen = self.entry(key).await?;
        let entry = seen.entry;
        Some((entry.op, entry.st, entry.owner))
    }

    async fn hot_has(&self, key: &str) -> bool {
        self.hot.get(key, None).await.is_ok()
    }

    async fn cold_has(&self, key: &str) -> bool {
        self.cold.get(key, None).await.is_ok()
    }
}

fn bytes(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// The entry of a put by the proxy `id` that landed: `(PUT, PENDING)`.
async fn land(rig: &Rig, id: &str, key: &str) {
    let client = rig.client(id);
    let Created::Taken(held) = client.create(key, Take::Put).await.unwrap() else {
        panic!("the entry exists");
    };
    let landed = client.advance(&held, Step::LandPut).await.unwrap();
    assert!(matches!(landed, Outcome::Done(_)));
}

/// A step that has run until the gate took its call.
async fn park(step: &mut (impl Future + Unpin), gate: &Gate) {
    tokio::select! {
        _ = &mut *step => panic!("the step finished instead of parking"),
        () = gate.parked() => {}
    }
}

fn report(changed: usize) -> StepReport {
    StepReport {
        active: true,
        held: true,
        changed,
        ..StepReport::default()
    }
}

/// Asserts that nothing is done until the format object says `active`: with no format object, with
/// `drain` and with `drained`, no lease is taken and no entry is read or changed; a format object
/// that cannot be read is a failure of the step.
#[tokio::test]
async fn nothing_is_done_until_the_format_is_active() {
    let rig = Rig::new();
    land(&rig, "p1", "k").await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    let mut r = rig.reconciler();
    assert_eq!(r.step().await, StepReport::default());
    for phase in [FormatPhase::Drain, FormatPhase::Drained] {
        let body = format_body(&Format { phase, epoch: None });
        rig.hot.delete(FORMAT_KEY).await.ok();
        rig.hot.put(FORMAT_KEY, body).await.unwrap();
        assert_eq!(r.step().await, StepReport::default());
    }
    assert!(!rig.cold_has("k").await);
    assert!(rig.hot.head(LEASE_KEY).await.is_err());
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );

    let faulty = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let error = Error::Unavailable("format".to_owned());
    faulty.fail_next_on(Op::Get, FORMAT_KEY, error.clone());
    let mut r = rig.over(faulty, rig.cold.clone(), "r1", |_| {});
    let failed = r.step().await;
    assert_eq!(failed.failed, [error]);
    assert!(!failed.active);
    assert!(!failed.held);
}

/// Asserts that a landed put is held by the reconciler, copied to the cold tier, sealed and
/// removed: the cold tier has the bytes, the hot tier keeps its object, no entry is left, and the
/// step reports one change and no failure.
#[tokio::test]
async fn a_landed_put_is_copied_to_the_cold_tier_and_its_entry_removed() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("value")).await.unwrap();
    land(&rig, "p1", "k").await;
    let step = rig.reconciler().step().await;
    assert_eq!(step, report(1));
    assert_eq!(rig.cold.get("k", None).await.unwrap(), bytes("value"));
    assert_eq!(rig.hot.get("k", None).await.unwrap(), bytes("value"));
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that a landed put whose hot object is gone (a put reaped before it landed, or a delete
/// that finished) copies nothing and its entry is removed, and that a put whose cold object has
/// the same bytes is removed with no mismatch, whether the comparison reads one range or several.
#[tokio::test]
async fn a_put_with_nothing_to_copy_or_a_cold_copy_already_there_is_released() {
    let rig = Rig::new();
    rig.activate().await;
    land(&rig, "p1", "gone").await;
    rig.hot.put("same", bytes("abcde")).await.unwrap();
    rig.cold.put("same", bytes("abcde")).await.unwrap();
    land(&rig, "p1", "same").await;
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.compare_range = 2;
    });
    let step = r.step().await;
    assert_eq!(step, report(2));
    assert!(!rig.cold_has("gone").await);
    assert!(rig.entry("gone").await.is_none());
    assert!(rig.entry("same").await.is_none());
}

/// Asserts that a put whose cold object differs in size or in bytes is reported by key as a
/// mismatch, the hot object is kept (and the cold one is not touched), and the entry is removed.
#[tokio::test]
async fn a_cold_object_that_differs_is_reported_and_the_hot_object_kept() {
    let rig = Rig::new();
    rig.activate().await;
    for (key, hot, cold) in [("size", "abc", "abcd"), ("bytes", "abc", "abd")] {
        rig.hot.put(key, bytes(hot)).await.unwrap();
        rig.cold.put(key, bytes(cold)).await.unwrap();
        land(&rig, "p1", key).await;
    }
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.compare_range = 2;
    });
    let mut step = r.step().await;
    step.mismatched.sort();
    assert_eq!(step.mismatched, ["bytes", "size"]);
    assert_eq!(step.changed, 2);
    assert!(step.failed.is_empty());
    assert_eq!(rig.hot.get("bytes", None).await.unwrap(), bytes("abc"));
    assert_eq!(rig.cold.get("size", None).await.unwrap(), bytes("abcd"));
    assert!(rig.entry("size").await.is_none() && rig.entry("bytes").await.is_none());
}

/// Asserts that a delete handed over (`(DEL, PENDING)`) is finished: the key is gone from both
/// tiers, also when one tier did not have it, and the entry is removed; and that a sealed delete
/// is finished the same way.
#[tokio::test]
async fn a_delete_handed_over_is_finished_on_both_tiers_and_its_entry_removed() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["both", "hot-only", "sealed"] {
        rig.hot.put(key, bytes("v")).await.unwrap();
    }
    rig.cold.put("both", bytes("v")).await.unwrap();
    rig.cold.put("sealed", bytes("v")).await.unwrap();
    rig.register("p1").await;
    let client = rig.client("p1");
    for key in ["both", "hot-only", "sealed"] {
        let Created::Taken(held) = client.create(key, Take::Del).await.unwrap() else {
            panic!("the entry exists");
        };
        let handed = client.advance(&held, Step::Handover).await.unwrap();
        let Outcome::Done(pending) = handed else {
            panic!("lost");
        };
        if key == "sealed" {
            let sealed = client.advance(&pending, Step::Seal).await.unwrap();
            assert!(matches!(sealed, Outcome::Done(_)));
        }
    }
    let step = rig.reconciler().step().await;
    assert_eq!(step, report(3));
    for key in ["both", "hot-only", "sealed"] {
        assert!(!rig.hot_has(key).await && !rig.cold_has(key).await);
        assert!(rig.entry(key).await.is_none());
    }
}

/// Asserts that a sealed put, a sealed copy-back and a pending eviction's sealed entry are removed
/// and that the objects are left as they were.
#[tokio::test]
async fn sealed_entries_are_removed_without_touching_the_objects() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("cb-sealed", bytes("v")).await.unwrap();
    rig.hot.put("evicted", bytes("v")).await.unwrap();
    let client = rig.client("p1");
    client.create("evicted", Take::Evict).await.unwrap();
    for (key, op) in [("put", EntryOp::Put), ("cb-sealed", EntryOp::Cb)] {
        rig.plant(key, op, EntryState::Sealed, None).await;
    }
    let step = rig.reconciler().step().await;
    assert_eq!(step, report(3));
    assert!(!rig.hot_has("cb-sealed").await && rig.cold_has("cb-sealed").await);
    assert!(rig.hot_has("evicted").await);
    for key in ["evicted", "put", "cb-sealed"] {
        assert!(rig.entry(key).await.is_none(), "{key}");
    }
}

/// Asserts that an entry in a shape no step of this release acts on is left as it is, without a
/// failure: a put held with no owner, a delete held by the reconciler itself, an eviction that is
/// pending, an operation this release does not know, and a state it does not know.
#[tokio::test]
async fn entries_in_shapes_this_release_does_not_act_on_are_left_alone() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant("ownerless", EntryOp::Put, EntryState::Held, None)
        .await;
    rig.plant("own-delete", EntryOp::Del, EntryState::Held, Some("r1"))
        .await;
    rig.plant("evict", EntryOp::Evict, EntryState::Pending, None)
        .await;
    for (key, op, st) in [
        ("future-op", "FUTURE", "HELD"),
        ("future-st", "PUT", "FUTURE"),
    ] {
        let body = format!(
            r#"{{"v":1,"key":"{key}","op":"{op}","st":"{st}","owner":"x","nonce":"n","created_ms":0}}"#
        );
        rig.hot
            .put(&entry_key(key), Bytes::from(body))
            .await
            .unwrap();
    }
    let step = rig.reconciler().step().await;
    assert_eq!(step, report(0));
    assert_eq!(rig.client("p1").list(100).await.unwrap().len(), 5);
}

/// Asserts that the reconciler's own put that a cut-off copy left held, `(PUT, HELD, me)`, is copied
/// again from the start: the cold tier has the bytes and the entry is removed.
#[tokio::test]
async fn a_put_the_reconciler_itself_left_held_is_copied_again() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant("k", EntryOp::Put, EntryState::Held, Some("r1"))
        .await;
    let step = rig.reconciler().step().await;
    assert_eq!(step, report(1));
    assert!(rig.cold_has("k").await && rig.entry("k").await.is_none());
}

/// Asserts that a put held by a live proxy is waited for and one held by a departed owner is
/// reaped: the registration is first seen at once, so the entry is pinned for the departure time
/// less a millisecond and reaped at the departure time itself (by the reconciler's own monotonic
/// clock, which the test advances), and the next step copies the put to the cold tier.
#[tokio::test]
async fn a_held_entry_is_waited_for_until_its_owner_has_departed_and_then_reaped() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register("p1").await;
    let client = rig.client("p1");
    client.create("k", Take::Put).await.unwrap();
    let mut r = rig.reconciler();
    let first = r.step().await;
    assert_eq!((first.pinned, first.changed), (1, 0));
    rig.mono.advance(Duration::from_millis(59_999));
    assert_eq!(r.step().await.pinned, 1);
    rig.mono.advance(Duration::from_millis(1));
    assert_eq!(r.step().await, report(1));
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
    assert_eq!(r.step().await, report(1));
    assert!(rig.cold_has("k").await && rig.entry("k").await.is_none());
}

/// Asserts that an entry whose owner has no registration is reaped at once, a put to
/// `(PUT, PENDING)` and a delete to `(DEL, PENDING)`, which the next step finishes.
#[tokio::test]
async fn a_held_entry_whose_owner_has_no_registration_is_reaped_at_once() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("p", bytes("v")).await.unwrap();
    rig.hot.put("d", bytes("v")).await.unwrap();
    rig.plant("p", EntryOp::Put, EntryState::Held, Some("gone"))
        .await;
    rig.plant("d", EntryOp::Del, EntryState::Held, Some("gone"))
        .await;
    let mut r = rig.reconciler();
    assert_eq!(r.step().await, report(2));
    assert_eq!(
        rig.shape("d").await,
        Some((EntryOp::Del, EntryState::Pending, None))
    );
    assert_eq!(r.step().await, report(2));
    assert!(rig.cold_has("p").await && !rig.hot_has("d").await);
    assert!(rig.entry("p").await.is_none() && rig.entry("d").await.is_none());
}

/// Asserts that reaping a put aborts the multipart uploads it left open for the key on both tiers,
/// across pages of one upload, and no upload of another key that has the key as a prefix; a store
/// that does not do multipart uploads is no failure, and one whose listing fails is.
#[tokio::test]
async fn reaping_a_put_aborts_the_uploads_of_its_key_and_no_others() {
    let rig = Rig::new();
    rig.activate().await;
    for tier in [&rig.hot, &rig.cold] {
        tier.create_upload("k").await.unwrap();
        tier.create_upload("k").await.unwrap();
        tier.create_upload("k2").await.unwrap();
    }
    rig.plant("k", EntryOp::Put, EntryState::Held, Some("gone"))
        .await;
    let faulty = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(rig.hot.clone(), faulty.clone(), "r1", |config| {
        config.base.page_size = 1;
    });
    assert_eq!(r.step().await, report(1));
    for tier in [&rig.hot, &rig.cold] {
        let open = tier.list_uploads("k", None, 10).await.unwrap();
        let keys: Vec<&str> = open.iter().map(|upload| upload.key.as_str()).collect();
        assert_eq!(keys, ["k2"]);
    }

    let cases = [
        Error::Unsupported("uploads".to_owned()),
        Error::Unavailable("uploads".to_owned()),
    ];
    for (n, error) in cases.into_iter().enumerate() {
        let key = format!("held-{n}");
        rig.plant(&key, EntryOp::Put, EntryState::Held, Some("gone"))
            .await;
        faulty.fail_next_with(Op::ListUploads, error.clone());
        let step = r.step().await;
        let failed = usize::from(!matches!(error, Error::Unsupported(_)));
        assert_eq!(step.failed.len(), failed, "{error}");
    }
}

/// Asserts that a reap whose upload cleanup fails is done again from the start by the next step:
/// the entry is still held by the departed owner after the failure, and once the retry delay has
/// passed the uploads on both tiers are aborted and the entry is reaped.
#[tokio::test]
async fn a_reap_whose_upload_cleanup_failed_aborts_the_uploads_on_the_retry() {
    let rig = Rig::new();
    rig.activate().await;
    for tier in [&rig.hot, &rig.cold] {
        tier.create_upload("k").await.unwrap();
    }
    rig.plant("k", EntryOp::Put, EntryState::Held, Some("gone"))
        .await;
    let faulty = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(rig.hot.clone(), faulty.clone(), "r1", |_| {});
    faulty.fail_next_with(Op::ListUploads, Error::Unavailable("uploads".to_owned()));
    assert_eq!(r.step().await.failed.len(), 1);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Put, EntryState::Held, Some("gone".to_owned())))
    );
    rig.mono.advance(Duration::from_secs(1));
    assert_eq!(r.step().await, report(1));
    for tier in [&rig.hot, &rig.cold] {
        assert!(tier.list_uploads("k", None, 10).await.unwrap().is_empty());
    }
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
}

/// Asserts that every store call of a copy that fails fails the item and not the step: the failure
/// is reported once, the entry stays held by the reconciler, and nothing reached the cold tier.
#[tokio::test]
async fn a_failing_store_call_fails_the_item_and_leaves_the_entry_held() {
    // The tier that fails, the call, and whether the cold tier holds the bytes already (so that
    // the comparison, not the copy, is what is under test).
    let cases = [
        (false, Op::Head, false),
        (false, Op::Get, false),
        (true, Op::Head, false),
        (true, Op::Put, false),
        (false, Op::Get, true),
        (true, Op::Get, true),
    ];
    for (n, (on_cold, op, precold)) in cases.into_iter().enumerate() {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("abcd")).await.unwrap();
        if precold {
            rig.cold.put("k", bytes("abcd")).await.unwrap();
        }
        land(&rig, "p1", "k").await;
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let error = Error::Unavailable(format!("case {n}"));
        let faulty = if on_cold { &cold } else { &hot };
        faulty.fail_next_on(op, "k", error.clone());
        let mut r = rig.over(hot.clone(), cold.clone(), "r1", |_| {});
        let step = r.step().await;
        assert_eq!(step.failed, [error], "case {n}");
        assert_eq!(step.changed, 0, "case {n}");
        let shape = rig.shape("k").await;
        assert_eq!(
            shape,
            Some((EntryOp::Put, EntryState::Held, Some("r1".to_owned())))
        );
        assert_eq!(rig.cold_has("k").await, precold, "case {n}");
    }
}

/// Asserts that a hot object that vanishes between the look at it and the read of it is nothing to
/// copy: nothing reaches the cold tier and the entry is removed.
#[tokio::test]
async fn a_hot_object_that_vanishes_before_the_copy_reads_it_is_nothing_to_copy() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    land(&rig, "p1", "k").await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Get, "k", Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.hot.delete("k").await.unwrap();
    gate.release();
    assert_eq!(step.await, report(1));
    assert!(!rig.cold_has("k").await && rig.entry("k").await.is_none());
}

/// Asserts that a cold delete that fails fails the item, the entry stays sealed for the next step,
/// and the next step, after the item's delay, finishes it.
#[tokio::test]
async fn a_delete_whose_cold_call_fails_is_finished_by_a_later_step() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.cold.put("k", bytes("v")).await.unwrap();
    let client = rig.client("p1");
    rig.register("p1").await;
    let Created::Taken(held) = client.create("k", Take::Del).await.unwrap() else {
        panic!("exists");
    };
    client.advance(&held, Step::Handover).await.unwrap();
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    cold.fail_next_with(Op::Delete, Error::Unavailable("cold".to_owned()));
    let mut r = rig.over(rig.hot.clone(), cold, "r1", |_| {});
    let first = r.step().await;
    assert_eq!(first.failed.len(), 1);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Del, EntryState::Sealed, None))
    );
    assert!(rig.hot_has("k").await);
    assert_eq!(r.step().await.backed_off, 1);
    rig.mono.advance(Duration::from_secs(1));
    assert_eq!(r.step().await, report(1));
    assert!(!rig.hot_has("k").await && !rig.cold_has("k").await);
}

/// Asserts that a delete whose call answers that the object is not there is finished all the same:
/// the entry is removed and nothing fails.
#[tokio::test]
async fn a_delete_of_an_object_that_is_already_gone_is_finished() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant("k", EntryOp::Del, EntryState::Sealed, None).await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let gone = || Error::NotFound("k".to_owned());
    hot.fail_next_on(Op::Delete, "k", gone());
    cold.fail_next_with(Op::Delete, gone());
    let mut r = rig.over(hot, cold, "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that an item that fails is not tried again before its delay: the delay is one second
/// after the first failure and two after the second, to the millisecond, and a success clears it;
/// that an entry removed meanwhile is forgotten; and that an entry that does not decode fails its
/// item and a failed read of an entry fails it too.
#[tokio::test]
async fn a_failing_item_waits_its_own_delay_and_the_delay_doubles() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    land(&rig, "p1", "k").await;
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let unavailable = || Error::Unavailable("cold".to_owned());
    cold.fail_next_on(Op::Head, "k", unavailable());
    cold.fail_next_on(Op::Head, "k", unavailable());
    let mut r = rig.over(rig.hot.clone(), cold.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed.len(), 1);
    assert_eq!(r.step().await.backed_off, 1);
    rig.mono.advance(Duration::from_millis(999));
    assert_eq!(r.step().await.backed_off, 1);
    rig.mono.advance(Duration::from_millis(1));
    assert_eq!(r.step().await.failed.len(), 1);
    rig.mono.advance(Duration::from_millis(1_999));
    assert_eq!(r.step().await.backed_off, 1);
    rig.mono.advance(Duration::from_millis(1));
    assert_eq!(r.step().await, report(1));
    assert!(rig.cold_has("k").await);

    rig.hot
        .put(&format!("{ENTRIES_PREFIX}garbage"), bytes("{"))
        .await
        .unwrap();
    let corrupt = r.step().await;
    assert!(matches!(corrupt.failed.as_slice(), [Error::Other(text)] if text.contains("corrupt")));
    assert_eq!(r.step().await.backed_off, 1);
    rig.hot
        .delete(&format!("{ENTRIES_PREFIX}garbage"))
        .await
        .unwrap();
    assert_eq!(r.step().await.backed_off, 0);

    r.stop().await;
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    rig.plant("again", EntryOp::Cb, EntryState::Pending, None)
        .await;
    hot.fail_next_on(Op::Head, ENTRIES_PREFIX, unavailable());
    let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
    assert_eq!(r.step().await.failed.len(), 1);
}

/// Asserts that an entry removed between the listing and the read of it is skipped.
#[tokio::test]
async fn an_entry_removed_between_the_listing_and_the_read_is_skipped() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant("k", EntryOp::Cb, EntryState::Pending, None).await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Head, ENTRIES_PREFIX, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.hot.delete(&entry_key("k")).await.unwrap();
    gate.release();
    assert_eq!(step.await, report(0));
}

/// Asserts that a failed listing of the registrations or of the entries ends the step with the
/// failure, with nothing done.
#[tokio::test]
async fn a_failed_listing_ends_the_step_with_its_failure() {
    for prefix in [PROXIES_PREFIX, ENTRIES_PREFIX] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        land(&rig, "p1", "k").await;
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let error = Error::Unavailable(prefix.to_owned());
        hot.fail_next_on(Op::List, prefix, error.clone());
        let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
        let step = r.step().await;
        assert_eq!((step.held, step.changed), (true, 0));
        assert_eq!(step.failed, [error]);
        assert!(!rig.cold_has("k").await);
    }
}

/// Asserts that puts and deletes are worked before copy-backs: with one item at a time, every
/// store call that changes the entry of a delete or a put comes before any that changes the entry
/// of a copy-back.
#[tokio::test]
async fn puts_and_deletes_are_worked_before_copy_backs() {
    let rig = Rig::new();
    rig.activate().await;
    let keys = ["a", "b", "c", "d", "e", "f"];
    for key in keys {
        let op = if key < "d" { EntryOp::Cb } else { EntryOp::Del };
        rig.plant(key, op, EntryState::Pending, None).await;
    }
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let mut r = rig.over(hot.clone(), rig.cold.clone(), "r1", |config| {
        config.base.parallel = 1;
    });
    assert_eq!(r.step().await, report(6));
    let log = hot.call_log();
    let at = |key: &str| {
        let path = entry_key(key);
        log.iter()
            .position(|(op, k)| *op == Op::PutIfMatch && *k == path)
            .unwrap()
    };
    let last_delete = keys[3..].iter().map(|key| at(key)).max().unwrap();
    let first_copy_back = keys[..3].iter().map(|key| at(key)).min().unwrap();
    assert!(last_delete < first_copy_back);
}

/// Asserts that a reconciler that does not hold the lease does nothing: another holds it, so the
/// step reports `held` as false and the entries are as they were.
#[tokio::test]
async fn a_reconciler_that_does_not_hold_the_lease_does_nothing() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    land(&rig, "p1", "k").await;
    let mut leader = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |_| {});
    let mut other = rig.over(rig.hot.clone(), rig.cold.clone(), "r2", |_| {});
    assert!(leader.step().await.held);
    land(&rig, "p1", "k2").await;
    rig.hot.put("k2", bytes("v")).await.unwrap();
    let step = other.step().await;
    assert_eq!(
        step,
        StepReport {
            active: true,
            ..StepReport::default()
        }
    );
    assert!(!rig.cold_has("k2").await);
    other.stop().await;
    assert!(leader.step().await.held);
}

/// Asserts the waits of the loop by the sleeper's record: while the loop has nothing to do they are
/// 1, 2, 4, 8 seconds and then 10 for as long as it idles; the loop gives the lease up when it is
/// halted in a wait.
#[tokio::test]
async fn the_loop_waits_one_two_four_eight_and_then_the_ceiling_when_idle() {
    let rig = Rig::new();
    rig.activate().await;
    let mut r = rig.reconciler();
    let halt = Notify::new();
    let watcher = async {
        while rig.waits.asked().len() < 6 {
            tokio::task::yield_now().await;
        }
        halt.notify_one();
    };
    tokio::join!(r.run(&halt), watcher);
    let secs = |n| Duration::from_secs(n);
    assert_eq!(
        rig.waits.asked()[..6],
        [secs(1), secs(2), secs(4), secs(8), secs(10), secs(10)]
    );
    assert!(rig.hot.head(LEASE_KEY).await.is_err());
}

/// Asserts that the loop works through the entries before its first wait, and that the first wait
/// after progress is the floor: the cold tier holds every key when the first wait is asked.
#[tokio::test]
async fn the_loop_copies_before_it_first_waits() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["a", "b", "c"] {
        rig.hot.put(key, bytes("v")).await.unwrap();
        land(&rig, "p1", key).await;
    }
    let mut r = rig.reconciler();
    let halt = Notify::new();
    let watcher = async {
        while rig.waits.asked().is_empty() {
            tokio::task::yield_now().await;
        }
        halt.notify_one();
    };
    tokio::join!(r.run(&halt), watcher);
    for key in ["a", "b", "c"] {
        assert!(rig.cold_has(key).await);
    }
    assert_eq!(rig.waits.asked()[0], Duration::from_secs(1));
}

/// Asserts that a halt cuts a step that is running and the lease is given up: the step is parked
/// reading an entry when the halt comes.
#[tokio::test]
async fn a_halt_cuts_a_step_that_is_running_and_gives_the_lease_up() {
    let rig = Rig::new();
    rig.activate().await;
    rig.plant("k", EntryOp::Cb, EntryState::Pending, None).await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Head, ENTRIES_PREFIX, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let halt = Notify::new();
    let watcher = async {
        gate.parked().await;
        assert!(rig.hot.head(LEASE_KEY).await.is_ok());
        halt.notify_one();
    };
    tokio::join!(r.run(&halt), watcher);
    assert!(rig.hot.head(LEASE_KEY).await.is_err());
    assert!(rig.entry("k").await.is_some());
}

/// Asserts the waits as values: the idle wait doubles from the floor to the ceiling and starts again
/// at the floor after progress, with no wait at all while a step makes progress; a step with a live
/// held entry, one that is not the leader and one of an inactive format wait the floor and leave
/// the doubling where it was.
#[test]
fn the_idle_waits_double_to_the_ceiling_and_restart_after_progress() {
    let secs = Duration::from_secs;
    let mut idle = Idle::new(secs(1), secs(10));
    let quiet = report(0);
    let waits: Vec<_> = (0..6).map(|_| idle.after(&quiet).unwrap()).collect();
    assert_eq!(
        waits,
        [secs(1), secs(2), secs(4), secs(8), secs(10), secs(10)]
    );
    assert_eq!(idle.after(&report(1)), None);
    assert_eq!(idle.after(&quiet), Some(secs(1)));
    assert_eq!(idle.after(&quiet), Some(secs(2)));

    let pinned = StepReport {
        pinned: 1,
        ..report(0)
    };
    let adopting = StepReport {
        strays_adopted: 1,
        ..report(0)
    };
    assert_eq!(idle.after(&pinned), Some(secs(1)));
    assert_eq!(idle.after(&pinned), Some(secs(1)));
    assert_eq!(idle.after(&adopting), None);
    let follower = StepReport {
        active: true,
        ..StepReport::default()
    };
    assert_eq!(idle.after(&follower), Some(secs(1)));
    assert_eq!(idle.after(&StepReport::default()), Some(secs(1)));
}

/// Asserts the delay of a failing item: one second, doubling to a minute, and a minute from then
/// on, however many times it failed.
#[test]
fn the_delay_of_a_failing_item_doubles_to_the_longest() {
    let secs = Duration::from_secs;
    let delays: Vec<_> = (1..=8).map(|n| retry_delay(secs(1), secs(60), n)).collect();
    let expected = [1, 2, 4, 8, 16, 32, 60, 60].map(secs);
    assert_eq!(delays, expected);
    assert_eq!(retry_delay(secs(1), secs(60), 0), secs(1));
    assert_eq!(retry_delay(secs(1), secs(60), u32::MAX), secs(60));
}

/// Asserts the defaults of the settings: the departure, the waits of the loop, the retry delays,
/// the comparison range, the stray grace, interval and rate.
#[test]
fn the_settings_default_to_the_values_the_design_names() {
    let config = Rig::new().config("r1");
    let secs = Duration::from_secs;
    assert_eq!(config.departure, secs(60));
    assert_eq!((config.idle_min, config.idle_max), (secs(1), secs(10)));
    assert_eq!((config.retry_min, config.retry_max), (secs(1), secs(60)));
    assert_eq!(config.compare_range, 1 << 20);
    assert_eq!(config.stray_grace, secs(600));
    assert_eq!(config.stray_interval, secs(24 * 3600));
    assert_eq!(config.stray_rate, 100);
    assert_eq!(config.abandoned_upload_after, secs(0));
    assert_eq!(config.base.parallel, 16);
}
