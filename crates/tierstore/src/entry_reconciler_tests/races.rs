//! The races between the reconciler and the clients, each driven by holding one store call of the
//! reconciler at a gate while a client acts, then letting it go.

use super::*;

/// A client's read of the entry of `key`, which must be there.
async fn read(rig: &Rig, id: &str, key: &str) -> Seen {
    rig.client(id).read(key).await.unwrap().unwrap()
}

/// Asserts that a delete that amends a landed put after the reconciler read it makes the
/// reconciler's hold fail, so that no copy starts: the cold tier never gets the object, and the
/// entry is the delete's, `(DEL, HELD, client)`, untouched.
#[tokio::test]
async fn a_delete_that_amends_a_put_before_the_hold_stops_the_copy_from_starting() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register("p1").await;
    land(&rig, "p1", "k").await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::PutIfMatch, ENTRIES_PREFIX, Phase::Pre);
    let cold = Arc::new(FaultyBackend::new(rig.cold.clone()));
    let mut r = rig.over(gated, cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let pending = read(&rig, "p1", "k").await;
    let amended = rig
        .client("p1")
        .advance(&pending, Step::Amend)
        .await
        .unwrap();
    assert!(matches!(amended, Outcome::Done(_)));
    gate.release();
    assert_eq!(step.await, report(1));
    assert_eq!(cold.calls(Op::Put) + cold.calls(Op::Head), 0);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Del, EntryState::Held, Some("p1".to_owned())))
    );
}

/// Asserts that a delete that amends the reconciler's held entry while the copy runs waits for the
/// release: the copy runs to its end (the late publish is in the cold tier), the seal fails and the
/// entry stays the delete's, which a step sees as held by a live owner; once the delete has handed
/// its entry over, the next step deletes the key from both tiers and removes the entry, which is
/// the release the delete polls for.
#[tokio::test]
async fn a_delete_that_amends_a_copy_in_flight_has_its_entry_released_after_the_copy() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register("p1").await;
    land(&rig, "p1", "k").await;
    let gated = Arc::new(GateBackend::new(rig.cold.clone()));
    let gate = gated.gate(Op::Put, "k", Phase::Pre);
    let mut r = rig.over(rig.hot.clone(), gated, "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let held = read(&rig, "p1", "k").await;
    assert_eq!(held.entry.owner.as_deref(), Some("r1"));
    let client = rig.client("p1");
    let amend = client.advance(&held, Step::AmendCopy).await.unwrap();
    assert!(matches!(amend, Outcome::Done(_)));
    let amended = read(&rig, "p1", "k").await;
    assert!(Step::AmendCopy.waits());
    gate.release();
    assert_eq!(step.await, report(1));
    assert!(rig.cold_has("k").await);
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Del, EntryState::Held, Some("p1".to_owned())))
    );
    let waiting = r.step().await;
    assert_eq!((waiting.pinned, waiting.changed), (1, 0));

    rig.cold.delete("k").await.unwrap();
    rig.hot.delete("k").await.unwrap();
    let handed = client.advance(&amended, Step::Handover).await.unwrap();
    assert!(matches!(handed, Outcome::Done(_)));
    assert_eq!(r.step().await, report(1));
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that a delete that claims a handed-over delete while the reconciler is sealing it makes
/// the seal fail, and the entry is left as the claim made it; and the same for a pending
/// copy-back that a delete amends.
#[tokio::test]
async fn a_delete_that_takes_an_entry_before_the_seal_leaves_it_as_the_delete_made_it() {
    for op in [EntryOp::Del, EntryOp::Cb] {
        let rig = Rig::new();
        rig.activate().await;
        rig.register("p1").await;
        rig.plant("k", op, EntryState::Pending, None).await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        let gated = Arc::new(GateBackend::new(rig.hot.clone()));
        let gate = gated.gate(Op::PutIfMatch, ENTRIES_PREFIX, Phase::Pre);
        let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
        let mut step = Box::pin(r.step());
        park(&mut step, &gate).await;
        let pending = read(&rig, "p1", "k").await;
        let step_of_delete = if op == EntryOp::Del {
            Step::Claim
        } else {
            Step::Amend
        };
        let taken = rig
            .client("p1")
            .advance(&pending, step_of_delete)
            .await
            .unwrap();
        assert!(matches!(taken, Outcome::Done(_)));
        gate.release();
        assert_eq!(step.await, report(1));
        assert!(rig.hot_has("k").await);
        assert_eq!(
            rig.shape("k").await,
            Some((EntryOp::Del, EntryState::Held, Some("p1".to_owned())))
        );
    }
}

/// Asserts that a reap that loses the race to the owner is no failure and no change of the
/// owner's work: the owner lands its put while the reconciler is reaping, and the entry is
/// `(PUT, PENDING)` as the owner made it.
#[tokio::test]
async fn a_reap_that_loses_to_the_owner_leaves_the_entry_as_the_owner_made_it() {
    let rig = Rig::new();
    rig.activate().await;
    let client = rig.client("slow");
    client.create("k", Take::Put).await.unwrap();
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::PutIfMatch, ENTRIES_PREFIX, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let held = read(&rig, "slow", "k").await;
    let landed = client.advance(&held, Step::LandPut).await.unwrap();
    assert!(matches!(landed, Outcome::Done(_)));
    gate.release();
    let done = step.await;
    assert_eq!((done.changed, done.failed.len()), (1, 0));
    assert_eq!(
        rig.shape("k").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
}

/// Asserts what a destructive step does when less than the margin of the lease is left: the copy
/// publish of a put and the deletes of a handed-over delete are made with exactly the margin left
/// without a write to the lease, and with a millisecond less the lease (still held) is renewed
/// with one write and the work is done in the same step; when that renewal fails, the item fails
/// with `Unavailable` and nothing is published or deleted.
#[tokio::test]
async fn a_cold_publish_and_a_delete_renew_a_held_lease_that_is_short_of_the_margin() {
    for (left, writes, renewal_fails) in [(10_000, 0, false), (10_001, 1, false), (10_001, 1, true)]
    {
        for delete in [false, true] {
            let rig = Rig::new();
            rig.activate().await;
            rig.hot.put("k", bytes("v")).await.unwrap();
            rig.register("p1").await;
            if delete {
                rig.cold.put("k", bytes("v")).await.unwrap();
                rig.plant("k", EntryOp::Del, EntryState::Pending, None)
                    .await;
            } else {
                land(&rig, "p1", "k").await;
            }
            let gated = Arc::new(GateBackend::new(rig.hot.clone()));
            let gate = gated.gate(Op::PutIfMatch, ENTRIES_PREFIX, Phase::Pre);
            let faulty = Arc::new(FaultyBackend::new(gated));
            if renewal_fails {
                let error = Error::Unavailable("lease".to_owned());
                faulty.fail_next_on(Op::PutIfMatch, LEASE_KEY, error);
            }
            let mut r = rig.over(faulty.clone(), rig.cold.clone(), "r1", |_| {});
            let mut step = Box::pin(r.step());
            park(&mut step, &gate).await;
            // The lease was written at time zero: it is valid for 20 s (ttl less margin).
            rig.mono.advance(Duration::from_millis(left));
            gate.release();
            let done = step.await;
            let moved = if delete {
                !rig.hot_has("k").await
            } else {
                rig.cold_has("k").await
            };
            let case = format!("left {left}, delete {delete}, renewal fails {renewal_fails}");
            assert_eq!(moved, !renewal_fails, "{case}");
            assert_eq!(faulty.calls_on(Op::PutIfMatch, LEASE_KEY), writes, "{case}");
            assert_eq!(done.failed.len(), usize::from(renewal_fails), "{case}");
            if renewal_fails {
                assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
            }
        }
    }
}

/// Asserts that a copy whose lease has less than the margin left after the hot read it streams
/// from is published when the lease can be renewed (the renewal is one write), and is not
/// published when it cannot: the read completes, the cold put is never sent, and the item fails
/// with `Unavailable`.
#[tokio::test]
async fn a_copy_whose_lease_margin_runs_out_during_the_hot_read_is_published_only_if_renewed() {
    for renewal_fails in [false, true] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        rig.register("p1").await;
        land(&rig, "p1", "k").await;
        let gated = Arc::new(GateBackend::new(rig.hot.clone()));
        let gate = gated.gate(Op::Get, "k", Phase::Pre);
        let faulty = Arc::new(FaultyBackend::new(gated));
        if renewal_fails {
            let error = Error::Unavailable("lease".to_owned());
            faulty.fail_next_on(Op::PutIfMatch, LEASE_KEY, error);
        }
        let mut r = rig.over(faulty.clone(), rig.cold.clone(), "r1", |_| {});
        let mut step = Box::pin(r.step());
        park(&mut step, &gate).await;
        rig.mono.advance(Duration::from_millis(10_001));
        gate.release();
        let done = step.await;
        assert_eq!(rig.cold_has("k").await, !renewal_fails);
        assert!(rig.hot_has("k").await);
        if renewal_fails {
            assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
        } else {
            assert!(done.failed.is_empty());
        }
    }
}

/// Asserts that a handed-over delete whose lease has less than the margin left after the cold
/// delete goes on to the hot delete when the lease can be renewed, and when it cannot the cold
/// delete has been made, the hot delete is never sent (the hot object stays), and the item fails
/// with `Unavailable`.
#[tokio::test]
async fn a_delete_whose_lease_margin_runs_out_during_the_cold_delete_goes_on_only_if_renewed() {
    for renewal_fails in [false, true] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        rig.cold.put("k", bytes("v")).await.unwrap();
        rig.plant("k", EntryOp::Del, EntryState::Pending, None)
            .await;
        let gated = Arc::new(GateBackend::new(rig.cold.clone()));
        let gate = gated.gate(Op::Delete, "k", Phase::Pre);
        let faulty = Arc::new(FaultyBackend::new(rig.hot.clone()));
        if renewal_fails {
            let error = Error::Unavailable("lease".to_owned());
            faulty.fail_next_on(Op::PutIfMatch, LEASE_KEY, error);
        }
        let mut r = rig.over(faulty, gated, "r1", |_| {});
        let mut step = Box::pin(r.step());
        park(&mut step, &gate).await;
        rig.mono.advance(Duration::from_millis(10_001));
        gate.release();
        let done = step.await;
        assert!(!rig.cold_has("k").await);
        assert_eq!(rig.hot_has("k").await, renewal_fails);
        if renewal_fails {
            assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
        } else {
            assert!(done.failed.is_empty());
        }
    }
}

/// A wait of the renewal timer that ends when the test says so, once: the renewal then starts as
/// the timer's would after one `renew`, and the waits after it never end.
struct Wake(tokio::sync::Notify);

#[async_trait]
impl Sleeper for Wake {
    async fn sleep(&self, _duration: Duration) {
        self.0.notified().await;
    }
}

/// Asserts the default setting of the lease, `renew` equal to the margin: an item that starts a
/// copy while the timer's renewal is in flight (the clock is just past `renew`, so less than the
/// margin is left) is copied in that same step, after the renewal's answer and with no second
/// write to the lease: the cold tier has the object, nothing fails, and the lease object was
/// replaced by one put. Catches the item refused with "less than the margin of the lease is left"
/// for the whole latency of every renewal while the lease is held.
#[tokio::test]
async fn an_item_started_while_the_timers_renewal_is_in_flight_is_copied_in_that_step() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register("p1").await;
    land(&rig, "p1", "k").await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let item = gated.gate(Op::Head, "k", Phase::Pre);
    let renewal = gated.gate(Op::PutIfMatch, LEASE_KEY, Phase::Pre);
    let counted = Arc::new(FaultyBackend::new(gated));
    let wake = Arc::new(Wake(tokio::sync::Notify::new()));
    let timer = wake.clone();
    let mut r = rig.over(counted.clone(), rig.cold.clone(), "r1", move |config| {
        config.base.lease_ttl = Duration::from_secs(30);
        config.base.lease_renew = Some(Duration::from_secs(10));
        config.base.lease_margin = Some(Duration::from_secs(10));
        config.base.sleeper = timer;
    });
    let mut step = Box::pin(r.step());
    park(&mut step, &item).await;
    rig.mono.advance(Duration::from_millis(10_001));
    wake.0.notify_one();
    park(&mut step, &renewal).await;
    assert!(item.release());
    assert!(renewal.release());
    let done = step.await;
    assert_eq!(done, report(1));
    assert!(rig.cold_has("k").await);
    assert!(rig.entry("k").await.is_none());
    assert_eq!(counted.calls_on(Op::PutIfMatch, LEASE_KEY), 1);
}

/// Asserts that a lease that is genuinely lost is still refused, and is not taken again: another
/// holder replaced the lease object, or it is gone, or the holder's own clock is at the end of the
/// ttl; the item fails with `Unavailable`, the cold tier does not get the object, the entry is
/// left as it was, and the lease object is as the other holder left it. A lease past the ttl on
/// the holder's clock is not renewed either, so no write is made to it.
#[tokio::test]
async fn an_item_is_still_refused_when_the_lease_was_lost() {
    // What happens to the lease object, and the time that passes after the lease was written.
    let cases = [
        (true, false, 10_001),
        (false, true, 10_001),
        (false, false, 30_000),
    ];
    for (n, (replaced, gone, passed)) in cases.into_iter().enumerate() {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        rig.register("p1").await;
        land(&rig, "p1", "k").await;
        let gated = Arc::new(GateBackend::new(rig.hot.clone()));
        let item = gated.gate(Op::Head, "k", Phase::Pre);
        let counted = Arc::new(FaultyBackend::new(gated));
        let mut r = rig.over(counted.clone(), rig.cold.clone(), "r1", |_| {});
        let mut step = Box::pin(r.step());
        park(&mut step, &item).await;
        if replaced {
            rig.hot.delete(LEASE_KEY).await.unwrap();
            rig.hot.put(LEASE_KEY, bytes("other\nn\n1")).await.unwrap();
        }
        if gone {
            rig.hot.delete(LEASE_KEY).await.unwrap();
        }
        rig.mono.advance(Duration::from_millis(passed));
        item.release();
        let done = step.await;
        assert!(
            matches!(done.failed.as_slice(), [Error::Unavailable(_)]),
            "case {n}"
        );
        assert!(!rig.cold_has("k").await, "case {n}");
        assert_eq!(
            rig.shape("k").await,
            Some((EntryOp::Put, EntryState::Held, Some("r1".to_owned()))),
            "case {n}"
        );
        let writes = counted.calls_on(Op::PutIfMatch, LEASE_KEY);
        assert_eq!(writes, u64::from(passed < 30_000), "case {n}");
        // The one create-only put is the take at the start of the step.
        assert_eq!(counted.calls_on(Op::Put, LEASE_KEY), 1, "case {n}");
        let body = rig.hot.get(LEASE_KEY, None).await.ok();
        assert_eq!(body.is_some(), !gone, "case {n}");
        if replaced {
            assert_eq!(body, Some(bytes("other\nn\n1")), "case {n}");
        }
    }
}
