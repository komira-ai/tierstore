//! What a reconciler that took over from another does before it evicts an object the other copied
//! (it holds no tag of it): it compares the bytes of the two tiers, never their tags, since two
//! tiers of different kinds tag the same bytes differently.

use std::sync::Arc;

use tierstore::{Backend, Error, Op, Reconciler};

use crate::chunked::{Chunked, Ending};
use crate::reconciler_reservations::{after_a_takeover, five_objects};
use crate::support::{Rig, assert_absent, body, read};

fn down() -> Error {
    Error::Unavailable("down".to_owned())
}

/// A reconciler over `hot` and `cold` that took over from one that copied the rig's five objects,
/// with a budget of 100 bytes.
async fn takeover_over(rig: &Rig, hot: Chunked, cold: Chunked) -> Reconciler {
    let mut first = rig.reconciler("r1", 1000);
    first.pass().await;
    first.stop().await;
    let config = rig.reconciler_config("r2", 100);
    Reconciler::new(Arc::new(hot), Arc::new(cold), config)
}

/// Asserts that a takeover compares the bytes of the two tiers and not the pieces they come in:
/// objects with the same bytes, read in chunks of different sizes (with empty chunks between), are
/// evicted down to the low-water mark.
#[tokio::test(start_paused = true)]
async fn a_takeover_evicts_objects_the_cold_tier_holds_the_same_bytes_of_in_other_chunks() {
    let rig = five_objects().await;
    let hot = Chunked::new(rig.hot_mem.clone(), 4, Ending::Whole);
    let cold = Chunked::new(rig.cold_mem.clone(), 7, Ending::Whole);
    let mut second = takeover_over(&rig, hot, cold).await;

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_eq!(read(&*rig.hot_mem, "k3").await, body(&"x".repeat(21)));
}

/// Asserts that a takeover keeps a hot object whose cold object is of the same size and other
/// bytes, and evicts the objects after it.
#[tokio::test(start_paused = true)]
async fn a_takeover_keeps_a_hot_object_the_cold_tier_holds_other_bytes_of_the_same_size() {
    let (rig, mut second) = after_a_takeover().await;
    rig.cold_mem.delete("k1").await.unwrap();
    let other = "x".repeat(20) + "y";
    rig.cold_mem.put("k1", body(&other)).await.unwrap();

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
    assert_eq!(read(&*rig.cold_mem, "k1").await, body(&other));
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_absent(&*rig.hot_mem, "k3").await;
}

/// Asserts that a takeover keeps every hot object when a read of either tier ends before the other
/// (an object replaced between the HEAD and the read), and that a read that breaks is reported
/// with nothing evicted.
#[tokio::test(start_paused = true)]
async fn a_takeover_keeps_hot_objects_when_a_read_of_either_tier_ends_early_or_breaks() {
    let reads = [
        (Ending::Short, Ending::Whole, false),
        (Ending::Whole, Ending::Short, false),
        (Ending::Broken, Ending::Whole, true),
        (Ending::Whole, Ending::Broken, true),
    ];
    for (hot_ending, cold_ending, breaks) in reads {
        let rig = five_objects().await;
        let hot = Chunked::new(rig.hot_mem.clone(), 5, hot_ending);
        let cold = Chunked::new(rig.cold_mem.clone(), 5, cold_ending);
        let mut second = takeover_over(&rig, hot, cold).await;

        let report = second.pass().await;
        let case = format!("{hot_ending:?} {cold_ending:?}");
        assert_eq!(report.evicted, 0, "{case}");
        let broke = Error::Unavailable("read broke".to_owned());
        let failed = if breaks { vec![broke] } else { Vec::new() };
        assert_eq!(report.failed, failed, "{case}");
        assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
    }
}

/// Asserts that a failed read of the hot tier or of the cold tier before an eviction without a tag
/// is reported, evicts nothing, and is retried by the next pass, which evicts.
#[tokio::test(start_paused = true)]
async fn a_takeover_whose_read_of_either_tier_fails_reports_it_and_evicts_on_the_next_pass() {
    for hot_fails in [true, false] {
        let (rig, mut second) = after_a_takeover().await;
        let tier = if hot_fails { &rig.hot } else { &rig.cold };
        tier.fail_next_on(Op::Get, "k1", down());

        let report = second.pass().await;
        assert_eq!(report.failed, [down()], "hot: {hot_fails}");
        assert_eq!(report.evicted, 0);
        assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));

        let report = second.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new());
        assert_eq!(report.evicted, 2);
        assert_absent(&*rig.hot_mem, "k1").await;
    }
}

/// Asserts that an object that is not there when the bytes are read does not abort the pass: a hot
/// object that vanished is forgotten (nothing is lost) and a cold object that vanished leaves the
/// hot object in place, with the objects after it evicted and no failure reported.
#[tokio::test(start_paused = true)]
async fn a_takeover_whose_read_finds_an_object_gone_goes_on_with_the_next_candidate() {
    for hot_gone in [true, false] {
        let (rig, mut second) = after_a_takeover().await;
        let tier = if hot_gone { &rig.hot } else { &rig.cold };
        tier.fail_next_on(Op::Get, "k1", Error::NotFound("k1".to_owned()));

        let report = second.pass().await;
        assert_eq!(report.failed, Vec::<Error>::new(), "hot: {hot_gone}");
        assert_eq!(
            report.evicted,
            if hot_gone { 1 } else { 2 },
            "hot: {hot_gone}"
        );
        assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
        assert_absent(&*rig.hot_mem, "k2").await;
    }
}

/// Asserts that a candidate left in the hot tier is asked again by a reconciler that takes over
/// from the checkpoint the pass stored: the stored cursor stays on the candidate, so the journal
/// is read from it, and the candidate is evicted once the cold tier holds its object.
#[tokio::test(start_paused = true)]
async fn a_takeover_after_a_pass_that_kept_a_candidate_asks_again_from_the_stored_cursor() {
    let (rig, mut second) = after_a_takeover().await;
    let held = rig.cold_mem.get("k1", None).await.unwrap();
    rig.cold_mem.delete("k1").await.unwrap();
    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
    second.stop().await;

    rig.cold_mem.put("k1", held).await.unwrap();
    let mut third = rig.reconciler("r3", 40);
    let report = third.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
}
