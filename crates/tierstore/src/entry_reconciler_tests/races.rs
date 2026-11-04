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

/// Asserts that a destructive step is refused when less than the margin of the lease is left, at
/// the margin and a millisecond under it: the copy publish of a put and the deletes of a handed-over
/// delete are made with exactly the margin left, and refused (the item fails, and nothing is
/// published or deleted) with a millisecond less; a later step renews the lease and does the work.
#[tokio::test]
async fn a_cold_publish_and_a_delete_need_the_margin_of_the_lease() {
    for (left, allowed) in [(10_000, true), (10_001, false)] {
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
            let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
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
            assert_eq!(moved, allowed, "left {left}, delete {delete}");
            assert_eq!(done.failed.len(), usize::from(!allowed));
            if !allowed {
                assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
                let again = r.step().await;
                assert_eq!((again.backed_off, again.changed), (1, 0));
                rig.mono.advance(Duration::from_secs(1));
                assert_eq!(r.step().await.changed, 1);
                let moved = if delete {
                    !rig.hot_has("k").await
                } else {
                    rig.cold_has("k").await
                };
                assert!(moved);
            }
        }
    }
}

/// Asserts that the lease is looked at again after the hot read that a copy streams from: with the
/// hot read parked and the margin of the lease used up meanwhile, the read completes, the cold put
/// is never sent (the cold tier does not get the object), and the item fails with `Unavailable`.
#[tokio::test]
async fn a_copy_whose_lease_margin_runs_out_during_the_hot_read_is_not_published() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register("p1").await;
    land(&rig, "p1", "k").await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Get, "k", Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.mono.advance(Duration::from_millis(10_001));
    gate.release();
    let done = step.await;
    assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
    assert!(!rig.cold_has("k").await);
    assert!(rig.hot_has("k").await);
}

/// Asserts that the lease is looked at again after the cold delete of a handed-over delete: with
/// the cold delete parked and the margin of the lease used up meanwhile, the cold delete completes,
/// the hot delete is never sent (the hot object stays), and the item fails with `Unavailable`.
#[tokio::test]
async fn a_delete_whose_lease_margin_runs_out_during_the_cold_delete_leaves_the_hot_object() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.cold.put("k", bytes("v")).await.unwrap();
    rig.plant("k", EntryOp::Del, EntryState::Pending, None)
        .await;
    let gated = Arc::new(GateBackend::new(rig.cold.clone()));
    let gate = gated.gate(Op::Delete, "k", Phase::Pre);
    let mut r = rig.over(rig.hot.clone(), gated, "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    rig.mono.advance(Duration::from_millis(10_001));
    gate.release();
    let done = step.await;
    assert!(matches!(done.failed.as_slice(), [Error::Unavailable(_)]));
    assert!(!rig.cold_has("k").await);
    assert!(rig.hot_has("k").await);
}
