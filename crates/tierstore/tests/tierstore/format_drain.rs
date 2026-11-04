//! The drained marker: the reconciler that holds the lease turns `drain` into `drained` once the
//! format has said `drain` for the settle time and a pass applied every journal without a failure.

use std::time::Duration;

use tierstore::{Backend, Error, Op, PassReport, Proxy, Reconciler};

use crate::support::{Rig, body, text};

const FORMAT: &str = "_tiering/format";
const SETTLE: Duration = Duration::from_secs(10);

/// Replaces the format object in the hot tier directly.
async fn set_format(rig: &Rig, json: &str) {
    let _ = rig.hot_mem.delete(FORMAT).await;
    rig.hot_mem.put(FORMAT, body(json)).await.unwrap();
}

async fn format(rig: &Rig) -> String {
    text(&*rig.hot_mem, FORMAT).await
}

/// A reconciler with the settle time of these tests, and a proxy `p1` that put `k` and that does
/// not read the format object again (as one that has not noticed a change yet).
async fn rig_with_a_put() -> (Rig, Proxy, Reconciler) {
    let rig = Rig::new();
    let mut proxy_config = rig.config("p1", 1000);
    proxy_config.format_refresh = Duration::from_secs(3600);
    let proxy = rig.open(proxy_config).await.unwrap();
    proxy.put("k", body("v")).await.unwrap();
    let mut config = rig.reconciler_config("r1", 1000);
    config.drain_settle = SETTLE;
    let reconciler = rig.reconciler_with(config);
    (rig, proxy, reconciler)
}

fn assert_clean(report: &PassReport) {
    assert!(report.held, "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
}

/// Asserts that, with the format saying `drain`, a pass applies the journal and leaves the object
/// as it is until the settle time has passed on the monotonic clock, and that the next pass after
/// it writes `drained`; and that the pass after that changes nothing.
#[tokio::test(start_paused = true)]
async fn drain_becomes_drained_after_the_settle_time_once_the_journals_are_applied() {
    let (rig, _proxy, mut reconciler) = rig_with_a_put().await;
    set_format(&rig, r#"{"phase":"drain"}"#).await;

    let first = reconciler.pass().await;
    assert_clean(&first);
    assert_eq!(first.copied, 1);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);

    tokio::time::advance(SETTLE - Duration::from_secs(1)).await;
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);

    tokio::time::advance(Duration::from_secs(1)).await;
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drained"}"#);

    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drained"}"#);
}

/// Asserts that `drained` keeps the epoch the `drain` object had.
#[tokio::test(start_paused = true)]
async fn drained_keeps_the_epoch_of_the_drain_it_replaces() {
    let (rig, _proxy, mut reconciler) = rig_with_a_put().await;
    set_format(&rig, r#"{"epoch":1,"phase":"drain"}"#).await;
    assert_clean(&reconciler.pass().await);
    tokio::time::advance(SETTLE).await;
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drained","epoch":1}"#);
}

/// Asserts that a pass that fails (the read of a journal object does) writes no `drained` even
/// after the settle time, and that the first pass without a failure after it does.
#[tokio::test(start_paused = true)]
async fn drained_is_not_written_by_a_pass_that_failed() {
    let (rig, proxy, mut reconciler) = rig_with_a_put().await;
    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_clean(&reconciler.pass().await);
    tokio::time::advance(SETTLE).await;
    proxy.put("k2", body("v")).await.unwrap();

    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::Get, "_journal/p1/", down.clone());
    let failed = reconciler.pass().await;
    assert_eq!(failed.failed, [down]);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);

    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drained"}"#);
}

/// Asserts that a format object the reconciler cannot read is reported as a failure of the pass,
/// without stopping the copy of the journals, and that an error of the write of `drained` is
/// reported and the write is tried again by the next pass.
#[tokio::test(start_paused = true)]
async fn a_bad_format_and_a_failed_write_are_reported_and_tried_again() {
    let (rig, _proxy, mut reconciler) = rig_with_a_put().await;
    set_format(&rig, "not json").await;
    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    let [Error::Other(what)] = report.failed.as_slice() else {
        panic!("{:?}", report.failed);
    };
    assert!(what.starts_with("corrupt format object: "), "{what}");

    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_clean(&reconciler.pass().await);
    tokio::time::advance(SETTLE).await;
    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::PutIfMatch, FORMAT, down.clone());
    assert_eq!(reconciler.pass().await.failed, [down]);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drained"}"#);
}

/// Asserts that the settle time counts from the pass that first saw `drain`: a format that went
/// away (a cancel) and came back as `drain` is settled again, and so is one seen by a reconciler
/// that gave up its lease and took it again.
#[tokio::test(start_paused = true)]
async fn the_settle_time_starts_again_after_a_cancel_and_after_the_lease_was_given_up() {
    let (rig, _proxy, mut reconciler) = rig_with_a_put().await;
    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_clean(&reconciler.pass().await);
    tokio::time::advance(SETTLE).await;
    rig.hot_mem.delete(FORMAT).await.unwrap();
    assert_clean(&reconciler.pass().await);

    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);

    tokio::time::advance(SETTLE).await;
    reconciler.stop().await;
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);

    tokio::time::advance(SETTLE).await;
    assert_clean(&reconciler.pass().await);
    assert_eq!(format(&rig).await, r#"{"phase":"drained"}"#);
}

/// Asserts that `drained` and `active` objects are left as they are, however long they stand, and
/// that a drain is not marked by a reconciler that does not hold the lease (the one that does is not
/// asked to pass again).
#[tokio::test(start_paused = true)]
async fn only_a_reconciler_with_the_lease_marks_a_drain_and_only_a_drain_is_marked() {
    let (rig, _proxy, mut reconciler) = rig_with_a_put().await;
    for json in [r#"{"phase":"drained"}"#, r#"{"epoch":2,"phase":"active"}"#] {
        set_format(&rig, json).await;
        for _ in 0..2 {
            assert_clean(&reconciler.pass().await);
            tokio::time::advance(SETTLE).await;
        }
        assert_eq!(format(&rig).await, json);
    }

    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_clean(&reconciler.pass().await);
    let mut waiting = rig.reconciler("r3", 1000);
    assert!(!waiting.pass().await.held);
    tokio::time::advance(SETTLE).await;
    assert!(!waiting.pass().await.held);
    assert_eq!(format(&rig).await, r#"{"phase":"drain"}"#);
}
