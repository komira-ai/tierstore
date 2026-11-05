//! The copy-back: what the reconciler does with a `(CB, PENDING)` entry that a read of a cold
//! object asked for. No assertion is about time: the age of an entry moves by the monotonic clock
//! of the rig, and the races are held by gates.

use super::*;

/// Asserts that a pending copy-back is sealed before a byte is written, that the cold object is
/// then streamed into the hot tier under the key, and that the entry is removed afterwards and the
/// cold object left where it is: the hot write is parked, and the entry is `(CB, SEALED)` then.
#[tokio::test]
async fn a_pending_copy_back_is_sealed_then_the_cold_object_is_streamed_into_the_hot_tier() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("cold bytes")).await.unwrap();
    rig.client("p1").create("k", Take::CopyBack).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Cb, EntryState::Sealed, None))
    );
    assert!(!rig.hot_has("k").await);
    gate.release();
    assert_eq!(step.await, report(1));
    assert_eq!(rig.hot.get("k", None).await.unwrap(), bytes("cold bytes"));
    assert_eq!(rig.cold.get("k", None).await.unwrap(), bytes("cold bytes"));
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that a delete that amends a pending copy-back while the reconciler seals it stops the
/// copy: nothing is written into the hot tier, the entry is the delete's, and after the delete has
/// removed the key from both tiers and handed the entry over, the reconciler finishes it and the
/// key is in neither tier. A copy-back never brings back a key that a delete took.
#[tokio::test]
async fn a_delete_that_amends_a_copy_back_before_the_seal_leaves_no_copy_to_resurrect_the_key() {
    let rig = Rig::new();
    rig.activate().await;
    rig.register("p1").await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    let client = rig.client("p1");
    client.create("k", Take::CopyBack).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::PutIfMatch, ENTRIES_PREFIX, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let pending = client.read("k").await.unwrap().unwrap();
    let amended = client.advance(&pending, Step::Amend).await.unwrap();
    assert!(matches!(amended, Outcome::Done(_)));
    let amended = client.read("k").await.unwrap().unwrap();
    gate.release();
    assert_eq!(step.await, report(1));
    assert!(!rig.hot_has("k").await);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Del, EntryState::Held, Some("p1".to_owned())))
    );

    rig.cold.delete("k").await.unwrap();
    let handed = client.advance(&amended, Step::Handover).await.unwrap();
    assert!(matches!(handed, Outcome::Done(_)));
    assert_eq!(r.step().await, report(1));
    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that a delete cannot amend a copy-back that is sealed: the step does not apply, and the
/// entry stays the reconciler's until the copy has landed and the entry is removed.
#[tokio::test]
async fn a_delete_cannot_amend_a_copy_back_that_is_sealed() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    let client = rig.client("p1");
    client.create("k", Take::CopyBack).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let sealed = client.read("k").await.unwrap().unwrap();
    let amend = client.advance(&sealed, Step::Amend).await.unwrap();
    assert_eq!(amend, Outcome::Refused);
    gate.release();
    assert_eq!(step.await, report(1));
    assert!(rig.hot_has("k").await);
}

/// Asserts that a copy-back with nothing to copy is removed without a failure and without a
/// change of either tier: a key the hot tier has (its object stays as it is), a key the cold tier
/// does not have, and a hot tier that answers full.
#[tokio::test]
async fn a_copy_back_with_nothing_to_copy_is_removed_and_changes_no_object() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("in-hot", bytes("cold")).await.unwrap();
    rig.hot.put("in-hot", bytes("hot")).await.unwrap();
    rig.cold.put("full", bytes("v")).await.unwrap();
    for key in ["in-hot", "gone", "full"] {
        rig.client("p1").create(key, Take::CopyBack).await.unwrap();
    }
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    hot.fail_next_on(Op::Put, "full", Error::Full("full".to_owned()));
    let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(3));
    assert_eq!(rig.hot.get("in-hot", None).await.unwrap(), bytes("hot"));
    assert!(!rig.hot_has("gone").await);
    assert!(!rig.hot_has("full").await);
    for key in ["in-hot", "gone", "full"] {
        assert!(rig.entry(key).await.is_none(), "{key}");
    }
}

/// Asserts that a copy-back whose copy fails (the head of the hot tier, the read of the cold tier,
/// the hot write) fails the item and leaves the entry sealed; that the next step, after the item's
/// delay, removes it and copies nothing, so a later read asks again.
#[tokio::test]
async fn a_failed_copy_back_leaves_the_entry_sealed_and_the_next_step_removes_it_without_a_copy() {
    for failing in [Op::Head, Op::Get, Op::Put] {
        let rig = Rig::new();
        rig.activate().await;
        rig.cold.put("k", bytes("v")).await.unwrap();
        rig.client("p1").create("k", Take::CopyBack).await.unwrap();
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
        let error = Error::Unavailable("down".to_owned());
        match failing {
            Op::Head => hot.fail_next_on(Op::Head, "k", error.clone()),
            Op::Get => cold.fail_next_on(Op::Get, "k", error.clone()),
            _ => hot.fail_next_on(Op::Put, "k", error.clone()),
        }
        let mut r = rig.over(hot, cold, "r1", |_| {});
        let first = r.step().await;
        assert_eq!(first.failed, [error], "{failing:?}");
        assert_eq!(
            rig.shape("k").await,
            Some((EntryOp::Cb, EntryState::Sealed, None))
        );
        rig.mono.advance(Duration::from_secs(1));
        assert_eq!(r.step().await, report(1));
        assert!(rig.entry("k").await.is_none());
        assert!(!rig.hot_has("k").await, "{failing:?}");
    }
}

/// Asserts that a copy-back is dropped, sealed and removed with no copy, once it has been seen for
/// the copy-back ttl (600 seconds by default) on the monotonic clock, and not before: the first
/// step's seal is made to fail, so the entry stays pending, and the second step is made at 599,999
/// and at 600,000 milliseconds after the first.
#[tokio::test]
async fn a_copy_back_seen_for_the_ttl_is_dropped_without_a_copy() {
    for (after, copied) in [(599_999, true), (600_000, false)] {
        let rig = Rig::new();
        rig.activate().await;
        rig.cold.put("k", bytes("v")).await.unwrap();
        rig.client("p1").create("k", Take::CopyBack).await.unwrap();
        let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
        let error = Error::Unavailable("down".to_owned());
        hot.fail_next_on(Op::PutIfMatch, ENTRIES_PREFIX, error);
        let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
        assert_eq!(r.step().await, report(1));
        assert_eq!(
            rig.shape("k").await,
            Some((EntryOp::Cb, EntryState::Pending, None))
        );
        rig.mono.advance(Duration::from_millis(after));
        assert_eq!(r.step().await, report(1));
        assert_eq!(rig.hot_has("k").await, copied, "after {after} ms");
        assert!(rig.entry("k").await.is_none());
    }
}

/// Asserts that only the oldest `copy_back_max` copy-backs are copied: with a maximum of two and
/// three open, seen at once, the first two by key are copied and the third is removed with no
/// copy.
#[tokio::test]
async fn copy_backs_over_the_maximum_are_dropped_without_a_copy() {
    let rig = Rig::new();
    rig.activate().await;
    for key in ["a", "b", "c"] {
        rig.cold.put(key, bytes("v")).await.unwrap();
        rig.client("p1").create(key, Take::CopyBack).await.unwrap();
    }
    let mut r = rig.over(rig.hot.clone(), rig.cold.clone(), "r1", |config| {
        config.copy_back_max = 2;
    });
    assert_eq!(r.step().await, report(3));
    assert!(rig.hot_has("a").await);
    assert!(rig.hot_has("b").await);
    assert!(!rig.hot_has("c").await);
    assert!(rig.cold_has("c").await);
    assert!(rig.entry("c").await.is_none());
}

/// Asserts that a key whose copy-back entry was replaced is timed afresh: the entry is seen once,
/// then deleted and asked for again (a new nonce) after the ttl has passed, and the new entry is
/// copied and not dropped as old.
#[tokio::test]
async fn a_copy_back_asked_for_again_is_timed_from_its_own_first_step() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    rig.client("p1").create("k", Take::CopyBack).await.unwrap();
    let hot = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let error = Error::Unavailable("down".to_owned());
    hot.fail_next_on(Op::PutIfMatch, ENTRIES_PREFIX, error);
    let mut r = rig.over(hot, rig.cold.clone(), "r1", |_| {});
    assert_eq!(r.step().await, report(1));
    rig.hot.delete(&entry_key("k")).await.unwrap();
    assert_eq!(r.step().await, report(0));
    rig.mono.advance(Duration::from_secs(601));
    rig.client("p2").create("k", Take::CopyBack).await.unwrap();
    assert_eq!(r.step().await, report(1));
    assert!(rig.hot_has("k").await);
}

/// Asserts the defaults of the two settings of the copy-back: a ttl of 600 seconds and at most
/// 1000 open copy-backs.
#[test]
fn the_copy_back_settings_default_to_ten_minutes_and_a_thousand() {
    let config = Rig::new().config("r1");
    assert_eq!(config.copy_back_ttl, Duration::from_secs(600));
    assert_eq!(config.copy_back_max, 1000);
}

/// Asserts that a copy-back whose lease was taken over by another holder before the hot write is
/// not written: the lease cannot be renewed, so the hot tier gets no object, the item fails with
/// `Unavailable` and the entry stays sealed (the holder that has the lease removes it).
#[tokio::test]
async fn a_copy_back_is_not_written_when_the_lease_was_taken_over() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("v")).await.unwrap();
    rig.client("p1").create("k", Take::CopyBack).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Head, "k", Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.hot.delete(LEASE_KEY).await.unwrap();
    rig.hot.put(LEASE_KEY, bytes("other\nn\n1")).await.unwrap();
    rig.mono.advance(Duration::from_millis(10_001));
    gate.release();
    let done = step.await;
    assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
    assert!(!rig.hot_has("k").await);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Cb, EntryState::Sealed, None))
    );
}

/// A renewal timer that is driven by the test: its first wait ends when `go` is signalled, and
/// its second wait (the renewal has been made and recorded) signals `done` and never ends.
struct Renewal {
    waits: std::sync::atomic::AtomicUsize,
    go: Notify,
    done: Notify,
}

#[async_trait]
impl Sleeper for Renewal {
    async fn sleep(&self, _duration: Duration) {
        if self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.go.notified().await;
        } else {
            self.done.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

/// Asserts that a copy-back whose leader loses the margin of its lease while the hot write is
/// running lands nothing: the write is parked with the lease good, the margin runs out, and when
/// the write goes on it reads no chunk, the hot tier has no object under the key, the item fails
/// with `Unavailable`, and the entry stays sealed for the successor.
#[tokio::test]
async fn a_copy_back_whose_lease_margin_runs_out_during_the_hot_write_lands_nothing() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("cold bytes")).await.unwrap();
    rig.client("p1").create("k", Take::CopyBack).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.mono.advance(Duration::from_millis(10_001));
    gate.release();
    let done = step.await;
    assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
    assert!(!rig.hot_has("k").await);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Cb, EntryState::Sealed, None))
    );
}

/// Asserts that a long copy-back finishes while the lease is renewed: the write is parked, the
/// clock moves past the end of the first lease's margin only after a renewal has been made, and
/// the write then goes on, the object is in the hot tier, and the entry is removed.
#[tokio::test]
async fn a_copy_back_that_outlasts_one_lease_term_finishes_when_the_lease_is_renewed() {
    let rig = Rig::new();
    rig.activate().await;
    rig.cold.put("k", bytes("cold bytes")).await.unwrap();
    rig.client("p1").create("k", Take::CopyBack).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Pre);
    let renewal = Arc::new(Renewal {
        waits: std::sync::atomic::AtomicUsize::new(0),
        go: Notify::new(),
        done: Notify::new(),
    });
    let timer = renewal.clone();
    let mut r = rig.over(gated, rig.cold.clone(), "r1", move |c| {
        c.base.sleeper = timer;
    });
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.mono.advance(Duration::from_secs(6));
    renewal.go.notify_one();
    tokio::select! {
        _ = &mut step => panic!("the step finished before the renewal"),
        () = renewal.done.notified() => {}
    }
    rig.mono.advance(Duration::from_secs(6));
    gate.release();
    assert_eq!(step.await, report(1));
    assert_eq!(rig.hot.get("k", None).await.unwrap(), bytes("cold bytes"));
    assert!(rig.entry("k").await.is_none());
}
