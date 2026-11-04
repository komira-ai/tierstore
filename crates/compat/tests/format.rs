//! A store that holds the objects of the bridge release (`_entries/` objects, and the format
//! object `_tiering/format` that pauses writes), shared with the release before it: the release
//! before is the real `tierstore` library of its commit (`tierstore_prev`), run over the same
//! directories as this branch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use bytes::Bytes;
use tierstore::{Backend, DirBackend, Proxy, ProxyConfig, SystemClock, TokioSleeper};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, Error as PrevError, Proxy as PrevProxy,
    ProxyConfig as PrevProxyConfig, Reconciler as PrevReconciler,
    ReconcilerConfig as PrevReconcilerConfig, SystemClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const FORMAT: &str = "_tiering/format";
const BUDGET: u64 = 1 << 20;

/// Asserts that the previous release, started over a hot directory in which this branch put an
/// object on an entry and then a format object that says `drained` was written, lists only the plain object this
/// branch put, reads it, finds no object under the `_entries/` key (it is in a subdirectory that
/// the previous release does not read, and it does not reserve the prefix), puts a new object (it does not read the format, which is why this release
/// is rolled to every node before anything pauses writes), and runs a pass of its reconciler
/// without a failure that leaves the format object byte for byte as it was; and that this branch
/// then reads the object the previous release put.
#[tokio::test]
async fn the_previous_release_serves_a_store_that_holds_entry_and_format_objects() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-format-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-format-cold");
    let hot = Arc::new(DirBackend::open(hot_dir.path()).unwrap());
    let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
    let active = Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
    hot.put(FORMAT, active).await.unwrap();
    let now = Proxy::open(
        hot.clone(),
        cold.clone(),
        ProxyConfig::new("new", BUDGET, Arc::new(SystemClock)),
    )
    .await
    .unwrap();
    assert!(now.start_heartbeat(Arc::new(TokioSleeper)));
    while !now.is_live() {
        tokio::task::yield_now().await;
    }
    now.put("plain", Bytes::from_static(b"plain bytes"))
        .await
        .unwrap();
    let entry = "_entries/0a0b0c0d";
    hot.put(entry, Bytes::from_static(b"{}")).await.unwrap();
    let drained = Bytes::from_static(br#"{"phase":"drained"}"#);
    hot.delete(FORMAT).await.unwrap();
    hot.put(FORMAT, drained.clone()).await.unwrap();

    let before_hot = Arc::new(PrevDir::open(hot_dir.path()).unwrap());
    let before_cold = Arc::new(PrevDir::open(cold_dir.path()).unwrap());
    let before = PrevProxy::open(
        before_hot.clone(),
        before_cold.clone(),
        PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock)),
    )
    .await
    .unwrap();

    let listed = before.list("").await.unwrap();
    let keys: Vec<String> = listed.into_iter().map(|object| object.key).collect();
    assert_eq!(keys, ["plain"]);
    let plain = before.get("plain", None).await.unwrap();
    assert_eq!(plain, Bytes::from_static(b"plain bytes"));
    let missing = before.get(entry, None).await;
    assert_eq!(missing, Err(PrevError::NotFound(entry.to_owned())));
    before
        .put("added", Bytes::from_static(b"added bytes"))
        .await
        .unwrap();

    let mut reconciler = PrevReconciler::new(
        before_hot.clone(),
        before_cold,
        PrevReconcilerConfig::new("old-r", BUDGET, Arc::new(PrevClock)),
    );
    let report = reconciler.pass().await;
    assert!(report.held, "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(before_hot.get(FORMAT, None).await.unwrap(), drained);

    let added = now.get("added", None).await.unwrap();
    assert_eq!(added, Bytes::from_static(b"added bytes"));
}
