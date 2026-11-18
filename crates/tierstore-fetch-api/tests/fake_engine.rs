//! `FakeEngine` answers waits from a script or parks them until the test completes them, and logs
//! every `ensure` and `wait`.
//!
//! Catches:
//! - a wait that is not `Ready` when nothing is scripted, or that ignores a scripted outcome;
//! - a parked wait that answers before `complete`, or that is not counted by `parked`;
//! - a `complete` that misses a waiter, counts a dropped one, or does not answer later waits;
//! - an `ensure` or a `wait` whose fields are lost in the log.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use futures::FutureExt;
use tierstore_fetch_api::engine::{Engine, PackSpec, WaitOutcome};
use tierstore_fetch_api::fake::FakeEngine;
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, PackId, PackKey, PackRoot, Rank};

fn pack_id(s: &str) -> PackId {
    PackId {
        key: PackKey(s.to_owned()),
        root: PackRoot::Digest([1; 32]),
    }
}

fn spec(s: &str) -> PackSpec {
    PackSpec {
        id: pack_id(s),
        size: 1 << 21,
        digests: None,
        upload_part_size: 0,
    }
}

fn range() -> ByteRange {
    ByteRange { offset: 0, len: 10 }
}

#[tokio::test]
async fn a_wait_is_ready_when_nothing_is_scripted() {
    let e = FakeEngine::new();
    assert_eq!(
        e.wait(&spec("p"), range(), JobId(1)).await,
        WaitOutcome::Ready
    );
}

#[tokio::test]
async fn a_scripted_outcome_answers_every_later_wait_of_that_pack_only() {
    let e = FakeEngine::new();
    e.set_outcome(pack_id("p"), WaitOutcome::Eio { cell: 4 });
    e.set_outcome(pack_id("q"), WaitOutcome::Lost);
    assert_eq!(
        e.wait(&spec("p"), range(), JobId(1)).await,
        WaitOutcome::Eio { cell: 4 }
    );
    assert_eq!(
        e.wait(&spec("p"), range(), JobId(1)).await,
        WaitOutcome::Eio { cell: 4 }
    );
    assert_eq!(
        e.wait(&spec("q"), range(), JobId(1)).await,
        WaitOutcome::Lost
    );
    assert_eq!(
        e.wait(&spec("r"), range(), JobId(1)).await,
        WaitOutcome::Ready
    );
}

#[tokio::test]
async fn set_outcome_ends_parking_and_answers_the_parked_waits() {
    let e = FakeEngine::new();
    e.park(pack_id("p"));
    let sp = spec("p");
    let mut parked = Box::pin(e.wait(&sp, range(), JobId(2)));
    assert!(parked.as_mut().now_or_never().is_none());
    e.set_outcome(pack_id("p"), WaitOutcome::Lost);
    assert_eq!(parked.await, WaitOutcome::Lost);
    assert_eq!(e.wait(&sp, range(), JobId(1)).await, WaitOutcome::Lost);
    assert_eq!(e.parked(&pack_id("p")), 0);
}

#[tokio::test]
async fn parked_waits_stay_pending_until_complete_answers_them_all() {
    let e = Arc::new(FakeEngine::new());
    e.park(pack_id("p"));
    let mut waits = Vec::new();
    for j in 0..3 {
        let e = Arc::clone(&e);
        waits.push(tokio::spawn(async move {
            e.wait(&spec("p"), range(), JobId(j)).await
        }));
    }
    // Progress event: all three are parked, none has answered.
    while e.parked(&pack_id("p")) < 3 {
        tokio::task::yield_now().await;
    }
    assert!(waits.iter().all(|w| !w.is_finished()));
    assert_eq!(e.complete(&pack_id("p"), WaitOutcome::Eio { cell: 2 }), 3);
    for w in waits {
        assert_eq!(w.await.unwrap(), WaitOutcome::Eio { cell: 2 });
    }
    assert_eq!(e.parked(&pack_id("p")), 0);
}

#[tokio::test]
async fn a_wait_after_complete_answers_the_outcome_at_once() {
    let e = FakeEngine::new();
    e.park(pack_id("p"));
    assert_eq!(e.complete(&pack_id("p"), WaitOutcome::Ready), 0);
    assert_eq!(
        e.wait(&spec("p"), range(), JobId(1)).await,
        WaitOutcome::Ready
    );
    assert_eq!(e.parked(&pack_id("p")), 0);
}

#[tokio::test]
async fn a_wait_whose_future_was_dropped_is_not_counted_by_complete() {
    let e = FakeEngine::new();
    e.park(pack_id("p"));
    let sp = spec("p");
    let mut kept = Box::pin(e.wait(&sp, range(), JobId(1)));
    let mut dropped = Box::pin(e.wait(&sp, range(), JobId(2)));
    assert!(kept.as_mut().now_or_never().is_none());
    assert!(dropped.as_mut().now_or_never().is_none());
    assert_eq!(e.parked(&pack_id("p")), 2);
    drop(dropped);
    assert_eq!(e.complete(&pack_id("p"), WaitOutcome::Lost), 1);
    assert_eq!(kept.await, WaitOutcome::Lost);
}

#[tokio::test]
async fn a_parked_wait_is_pending_when_polled_once() {
    let e = FakeEngine::new();
    e.park(pack_id("p"));
    let sp = spec("p");
    assert!(e.wait(&sp, range(), JobId(1)).now_or_never().is_none());
}

#[tokio::test]
async fn parked_counts_only_the_named_pack() {
    let e = FakeEngine::new();
    e.park(pack_id("p"));
    let sp = spec("p");
    let mut w = Box::pin(e.wait(&sp, range(), JobId(1)));
    assert!(w.as_mut().now_or_never().is_none());
    assert_eq!(e.parked(&pack_id("p")), 1);
    assert_eq!(e.parked(&pack_id("other")), 0);
}

#[test]
fn ensure_logs_every_field_in_order() {
    let e = FakeEngine::new();
    assert!(e.ensures().is_empty());
    let runs = [
        CellRun { first: 0, count: 2 },
        CellRun { first: 7, count: 1 },
    ];
    e.ensure(&spec("p"), &runs, Class::Prepare, JobId(5), Rank(9));
    e.ensure(&spec("q"), &[], Class::Background, JobId(6), Rank(0));
    let got = e.ensures();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].pack, pack_id("p"));
    assert_eq!(got[0].runs, runs.to_vec());
    assert_eq!(got[0].class, Class::Prepare);
    assert_eq!(got[0].job, JobId(5));
    assert_eq!(got[0].rank, Rank(9));
    assert_eq!(got[1].pack, pack_id("q"));
    assert!(got[1].runs.is_empty());
    assert_eq!(got[1].class, Class::Background);
}

#[tokio::test]
async fn wait_logs_pack_range_and_job_in_order() {
    let e = FakeEngine::new();
    assert!(e.waits().is_empty());
    let _ = e
        .wait(&spec("p"), ByteRange { offset: 3, len: 4 }, JobId(8))
        .await;
    let _ = e.wait(&spec("q"), range(), JobId(9)).await;
    assert_eq!(
        e.waits(),
        vec![
            (pack_id("p"), ByteRange { offset: 3, len: 4 }, JobId(8)),
            (pack_id("q"), range(), JobId(9)),
        ]
    );
}
