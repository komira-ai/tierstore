//! What a reconciler that took over from another does before it evicts an object the other copied
//! (it holds no tag of it): it compares the bytes of the two tiers, never their tags, since two
//! tiers of different kinds tag the same bytes differently. The hot object is read in ranges, each
//! with its content tag, so that the bytes compared are those of the object the eviction deletes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tierstore::{Backend, Error, Op, Reconciler};

use crate::churn::{Change, Churn};

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

/// Asserts that the comparison consumes the reads chunk by chunk and stops at the first
/// difference: objects of 21 bytes that differ in the first byte, whose cold object is read in
/// chunks of 3, are pulled from the cold tier for one chunk only (not for the whole object), and
/// the hot object stays.
#[tokio::test(start_paused = true)]
async fn a_takeover_compares_chunk_by_chunk_and_stops_at_the_first_difference() {
    let rig = five_objects().await;
    rig.cold_mem.delete("k1").await.unwrap();
    rig.cold_mem
        .put("k1", body(&("y".to_owned() + &"x".repeat(20))))
        .await
        .unwrap();
    let hot = Chunked::new(rig.hot_mem.clone(), 3, Ending::Whole);
    let cold = Chunked::new(rig.cold_mem.clone(), 3, Ending::Whole);
    let cold_pulled = cold.pulled();
    let mut second = takeover_over(&rig, hot, cold).await;

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&"x".repeat(21)));
    // k1 is pulled for one chunk, and each of the objects after it (7 chunks each) for all of them
    // when it matches: the 2 evicted ones and no more.
    assert_eq!(cold_pulled.load(Ordering::SeqCst), 1 + 2 * 7);
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

/// Asserts that a takeover keeps every hot object when a read of either tier ends before the other,
/// or both end together before the size the HEAD gave (an object replaced between the HEAD and the
/// read), or the cold read goes on after the hot object ends, and that a read that breaks is
/// reported with nothing evicted.
#[tokio::test(start_paused = true)]
async fn a_takeover_keeps_hot_objects_when_a_read_of_either_tier_ends_early_or_breaks() {
    let reads = [
        (Ending::Short, Ending::Whole, false),
        (Ending::Whole, Ending::Short, false),
        (Ending::Short, Ending::Short, false),
        (Ending::Whole, Ending::Long, false),
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

/// Asserts that a takeover keeps a hot object that was replaced between its HEAD and the read of
/// its bytes and put back before the delete. The hot tier holds A (the cold tier holds B, of the
/// same size and the same bytes but the last): the read finds B, which is the cold object's bytes,
/// and A is put back before the delete. The range read of B has B's tag, not the HEAD's, so the
/// comparison does not match, A survives, and the objects after it are evicted.
#[tokio::test(start_paused = true)]
async fn a_takeover_keeps_a_hot_object_replaced_and_put_back_around_the_read_of_its_bytes() {
    let rig = five_objects().await;
    let a = "x".repeat(21);
    let b = "x".repeat(20) + "y";
    let mut first = rig.reconciler("r1", 1000);
    first.pass().await;
    first.stop().await;
    rig.cold_mem.delete("k1").await.unwrap();
    rig.cold_mem.put("k1", body(&b)).await.unwrap();
    let hot = Churn::around_a_marked_read(
        rig.hot_mem.clone(),
        Change::Object(body(&b)),
        Change::Object(body(&a)),
    );
    let config = rig.reconciler_config("r2", 100);
    let mut second = Reconciler::new(Arc::new(hot), rig.cold.clone(), config);

    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 2);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&a));
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_absent(&*rig.hot_mem, "k3").await;
}

/// Asserts that a hot object larger than one range is compared range by range: of two objects of
/// 2.5 MiB, the one whose cold object differs in the last byte (in the third range) is kept and
/// the one the cold tier holds as it is is evicted.
#[tokio::test(start_paused = true)]
async fn a_takeover_compares_an_object_of_several_ranges_to_its_last_byte() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 100 << 20).await;
    let whole = "x".repeat(5 << 19);
    for key in ["k1", "k2"] {
        proxy.put(key, body(&whole)).await.unwrap();
    }
    let mut first = rig.reconciler("r1", 100 << 20);
    first.pass().await;
    first.stop().await;
    let other = "x".repeat((5 << 19) - 1) + "y";
    rig.cold_mem.delete("k1").await.unwrap();
    rig.cold_mem.put("k1", body(&other)).await.unwrap();

    let mut second = rig.reconciler("r2", 1);
    let report = second.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.evicted, 1);
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&whole));
    assert_absent(&*rig.hot_mem, "k2").await;
}
