//! The leader under `drain` over the entry protocol: it finishes the entries, waits for the
//! registrations to report a pause and no write in flight, and writes `drained` over the format
//! object by its content tag.

use super::*;
use crate::registration::Registration;

const DRAINED: &[u8] = br#"{"phase":"drained"}"#;

fn drain_at(epoch: u64) -> Format {
    Format {
        phase: FormatPhase::Drain,
        epoch: Some(epoch),
        marks_bucket: None,
    }
}

impl Rig {
    /// The format object, replaced by `format`.
    async fn set_format(&self, format: &Format) {
        self.hot.delete(FORMAT_KEY).await.ok();
        self.hot.put(FORMAT_KEY, format_body(format)).await.unwrap();
    }

    /// The registration of `id` as a heartbeat of this release writes it: the phase it acts on and
    /// the writes it has begun and not ended.
    async fn report(&self, id: &str, phase: Option<FormatPhase>, writes: Option<u64>) {
        let body = Registration {
            proto: 2,
            boot: format!("boot-{id}"),
            hb: 7,
            peers: None,
            phase,
            writes_in_flight: writes,
        }
        .encode();
        self.hot.delete(&proxy_key(id)).await.ok();
        self.hot.put(&proxy_key(id), body).await.unwrap();
    }

    async fn quiet(&self, id: &str) {
        self.report(id, Some(FormatPhase::Drain), Some(0)).await;
    }

    async fn format_bytes(&self) -> Bytes {
        self.hot.get(FORMAT_KEY, None).await.unwrap()
    }
}

/// Asserts that under `drain` at epoch 2 a landed put is copied to the cold tier and its entry
/// removed as under `active`, and that in the same step, with the one registration at rest, the
/// format object becomes `{"phase":"drained"}`; the step after it does nothing.
#[tokio::test]
async fn a_drain_over_the_entries_finishes_them_and_writes_drained() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    land(&rig, "p1", "k").await;
    rig.quiet("p1").await;
    let mut r = rig.reconciler();
    let step = r.step().await;
    assert_eq!(
        step,
        StepReport {
            drained: true,
            ..report(1)
        }
    );
    assert!(rig.cold_has("k").await);
    assert!(rig.entry("k").await.is_none());
    assert_eq!(rig.format_bytes().await, DRAINED);
    assert_eq!(r.step().await, StepReport::default());
}

/// Asserts that a registration holds the pause, and `drained` is not written, while it reports a
/// phase other than `drain` or `drained`, a write in flight, no phase or no count (the body of a
/// release that does not write the fields, `{}`, and a body that is no registration): each is
/// tried in turn, and the registration that then reports a pause and no write lets the next step
/// write `drained`.
#[tokio::test]
async fn a_registration_that_may_still_write_holds_the_drain() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    let mut r = rig.reconciler();
    let holds: [(Option<FormatPhase>, Option<u64>); 5] = [
        (Some(FormatPhase::Active), Some(0)),
        (Some(FormatPhase::Drain), Some(2)),
        (None, Some(0)),
        (Some(FormatPhase::Drained), None),
        (Some(FormatPhase::ActiveMarks), Some(0)),
    ];
    for (phase, writes) in holds {
        rig.report("p1", phase, writes).await;
        let step = r.step().await;
        assert!(!step.drained, "{phase:?} {writes:?}");
        assert!(step.failed.is_empty());
        assert_eq!(rig.format_bytes().await, format_body(&drain_at(2)));
    }
    for body in ["{}", ""] {
        rig.hot.delete(&proxy_key("p1")).await.unwrap();
        rig.hot.put(&proxy_key("p1"), bytes(body)).await.unwrap();
        assert!(!r.step().await.drained, "{body:?}");
    }
    rig.report("p1", Some(FormatPhase::Drained), Some(0)).await;
    assert!(r.step().await.drained);
    assert_eq!(rig.format_bytes().await, DRAINED);
}

/// Asserts that every registration is waited for, not only the first: with `p1` at rest and `p2`
/// still counting a write, `drained` is not written, and it is once `p2` reports none.
#[tokio::test]
async fn the_drain_waits_for_every_registration() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.quiet("p1").await;
    rig.report("p2", Some(FormatPhase::Drain), Some(1)).await;
    let mut r = rig.reconciler();
    assert!(!r.step().await.drained);
    rig.quiet("p2").await;
    assert!(r.step().await.drained);
}

/// Asserts that a registration with no heartbeat fields stops holding the drain once it has stood
/// unchanged for the departure, counted on the leader's clock from the first step that saw it, and
/// that a registration that is gone holds nothing: an entry-less store with one such registration
/// is drained one departure after the leader first saw it, not before.
#[tokio::test]
async fn a_registration_of_a_departed_process_does_not_hold_the_drain() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.hot.put(&proxy_key("old"), bytes("{}")).await.unwrap();
    rig.quiet("p1").await;
    let mut r = rig.reconciler();
    assert!(!r.step().await.drained);
    rig.mono.advance(Duration::from_secs(59));
    assert!(!r.step().await.drained);
    rig.mono.advance(Duration::from_secs(1));
    assert!(r.step().await.drained);

    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    let mut r = rig.reconciler();
    assert!(r.step().await.drained);
}

/// Asserts that an entry that stays holds the drain: a put held by a live owner is left (pinned),
/// and `drained` is written in the step after the owner has departed and the entry was reaped and
/// finished.
#[tokio::test]
async fn an_open_entry_holds_the_drain_until_it_is_finished() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.plant("k", EntryOp::Put, EntryState::Held, Some("p1"))
        .await;
    rig.quiet("p1").await;
    let mut r = rig.reconciler();
    let step = r.step().await;
    assert!(!step.drained);
    assert_eq!(step.pinned, 1);
    assert!(rig.entry("k").await.is_some());
    rig.mono.advance(Duration::from_secs(60));
    rig.clock.advance(60_000);
    let reaped = r.step().await;
    assert!(
        !reaped.drained,
        "the reap makes it pending, the next step finishes it"
    );
    assert!(r.step().await.drained);
    assert!(rig.cold_has("k").await);
    assert!(rig.entry("k").await.is_none());
}

/// Asserts that a `--cancel` that lands while the leader works wins: the format object is changed
/// to `active` at epoch 2 after the leader read it and before it writes `drained`, the write is
/// refused by the store, the step reports no failure and no `drained`, and the object stays
/// `active`.
#[tokio::test]
async fn a_cancel_that_lands_before_the_write_of_drained_wins() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.quiet("p1").await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::PutIfMatch, FORMAT_KEY, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let active = Format {
        phase: FormatPhase::Active,
        epoch: Some(2),
        marks_bucket: None,
    };
    rig.set_format(&active).await;
    gate.release();
    let done = step.await;
    assert!(!done.drained);
    assert!(done.failed.is_empty(), "{:?}", done.failed);
    assert_eq!(rig.format_bytes().await, format_body(&active));
}

/// Asserts that a format object cancelled between the read of its body and the read of its tag
/// is not drained: the step does nothing, and the object is the one `--cancel` wrote.
#[tokio::test]
async fn a_cancel_before_the_tag_is_read_ends_the_step_without_a_lease() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::Head, FORMAT_KEY, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let active = Format {
        phase: FormatPhase::Active,
        epoch: Some(2),
        marks_bucket: None,
    };
    rig.set_format(&active).await;
    gate.release();
    assert_eq!(step.await, StepReport::default());
    assert!(rig.hot.head(LEASE_KEY).await.is_err());

    rig.set_format(&drain_at(2)).await;
    let faulty = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let error = Error::Unavailable("head".to_owned());
    faulty.fail_next_on(Op::Head, FORMAT_KEY, error.clone());
    let mut r = rig.over(faulty, rig.cold.clone(), "r2", |_| {});
    let failed = r.step().await;
    assert_eq!(failed.failed, [error]);
    assert!(!failed.active);
}

/// Asserts the failures of the settling: a step whose own work failed settles nothing, a
/// registration that cannot be read, and a write of `drained` that fails for a reason other than
/// a changed object are each reported as failed and write nothing; a registration deleted
/// between the listing and the read is a departed process; and the next step, with the store
/// healthy, writes `drained`.
#[tokio::test]
async fn the_failures_of_the_settling_are_reported_and_the_next_step_writes_drained() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.quiet("p1").await;
    let faulty = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let mut r = rig.over(faulty.clone(), rig.cold.clone(), "r1", |_| {});
    let down = Error::Unavailable("down".to_owned());

    // A step that failed already settles nothing, however quiet the proxies are.
    faulty.fail_next_on(Op::List, ENTRIES_PREFIX, down.clone());
    let step = r.step().await;
    assert_eq!(step.failed, std::slice::from_ref(&down));
    assert!(!step.drained);
    assert_eq!(faulty.calls_on(Op::Get, &proxy_key("p1")), 0);

    faulty.fail_next_on(Op::Get, &proxy_key("p1"), down.clone());
    let step = r.step().await;
    assert_eq!(step.failed, std::slice::from_ref(&down));
    assert!(!step.drained);

    faulty.fail_next_on(Op::PutIfMatch, FORMAT_KEY, down.clone());
    let step = r.step().await;
    assert_eq!(step.failed, [down]);
    assert!(!step.drained);
    assert_eq!(rig.format_bytes().await, format_body(&drain_at(2)));

    let gone = Error::NotFound("gone".to_owned());
    faulty.fail_next_on(Op::Get, &proxy_key("p1"), gone);
    let step = r.step().await;
    assert!(step.failed.is_empty(), "{:?}", step.failed);
    assert!(step.drained);
    assert_eq!(rig.format_bytes().await, DRAINED);
}

/// Asserts that a listing of `_entries/` that fails in the settling, after the registrations were
/// read, is reported as failed and writes nothing, and that the next step writes `drained`.
#[tokio::test]
async fn a_listing_of_the_entries_that_fails_after_the_registrations_holds_the_drain() {
    let rig = Rig::new();
    rig.set_format(&drain_at(2)).await;
    rig.quiet("p1").await;
    let faulty = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let gated = Arc::new(GateBackend::new(faulty.clone()));
    // Only the settling reads a registration, and it lists `_entries/` right after.
    let gate = gated.gate(Op::Get, &proxy_key("p1"), Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;
    let down = Error::Unavailable("entries".to_owned());
    faulty.fail_next_on(Op::List, ENTRIES_PREFIX, down.clone());
    gate.release();
    let failed = step.await;
    assert_eq!(failed.failed, [down]);
    assert!(!failed.drained);
    assert_eq!(rig.format_bytes().await, format_body(&drain_at(2)));
    assert!(r.step().await.drained);
}

/// Asserts that a `drain` the entries do not own is left alone: with no epoch (the pause before
/// the entries), another epoch (1 or 3), or naming a marks bucket this reconciler does not have, the step
/// takes no lease, copies nothing and writes nothing.
#[tokio::test]
async fn a_drain_of_another_protocol_is_not_this_leaders() {
    let rig = Rig::new();
    rig.hot.put("k", bytes("v")).await.unwrap();
    land(&rig, "p1", "k").await;
    rig.quiet("p1").await;
    let formats = [
        Format {
            phase: FormatPhase::Drain,
            epoch: None,
            marks_bucket: None,
        },
        drain_at(1),
        drain_at(3),
        Format {
            marks_bucket: Some("m".to_owned()),
            ..drain_at(3)
        },
    ];
    let mut r = rig.reconciler();
    for format in formats {
        rig.set_format(&format).await;
        assert_eq!(r.step().await, StepReport::default(), "{format:?}");
        assert_eq!(rig.format_bytes().await, format_body(&format));
    }
    assert!(!rig.cold_has("k").await);
    assert!(rig.hot.head(LEASE_KEY).await.is_err());
}
