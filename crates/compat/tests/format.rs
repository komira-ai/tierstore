//! A store that holds the objects of the bridge release (`_entries/` objects, and the format
//! object `_tiering/format` that pauses writes), shared with the release before it: the release
//! before is the real `tierstore` library of its commit (`tierstore_prev`), run over the same
//! directories as this branch.

use std::sync::Arc;

use bytes::Bytes;
use tierstore::SystemClock;
use tierstore::{Backend, DirBackend, Error, Proxy, ProxyConfig, Reconciler, ReconcilerConfig};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, Error as PrevError, Proxy as PrevProxy,
    ProxyConfig as PrevProxyConfig, Reconciler as PrevReconciler,
    ReconcilerConfig as PrevReconcilerConfig, SystemClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const FORMAT: &str = "_tiering/format";
const BUDGET: u64 = 1 << 20;

/// Asserts that the previous release, started over a hot directory in which this branch holds an
/// `_entries/` object and a format object that says `drained`, lists only the plain object this
/// branch put, reads it, finds no object under the `_entries/` key (it is in a subdirectory that
/// the previous release does not read, and it does not reserve the prefix), refuses the format
/// key as reserved, puts a new object (it does not read the format, which is why this release
/// is rolled to every node before anything pauses writes), and runs a pass of its reconciler
/// without a failure that leaves the format object byte for byte as it was; and that this branch
/// then reads the object the previous release put.
#[tokio::test]
async fn the_previous_release_serves_a_store_that_holds_entry_and_format_objects() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-format-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-format-cold");
    let hot = Arc::new(DirBackend::open(hot_dir.path()).unwrap());
    let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
    let now = Proxy::open(
        hot.clone(),
        cold.clone(),
        ProxyConfig::new("new", BUDGET, Arc::new(SystemClock)),
    )
    .await
    .unwrap();
    now.put("plain", Bytes::from_static(b"plain bytes"))
        .await
        .unwrap();
    let entry = "_entries/0a0b0c0d";
    hot.put(entry, Bytes::from_static(b"{}")).await.unwrap();
    let drained = Bytes::from_static(br#"{"phase":"drained"}"#);
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
    let reserved = before.get(FORMAT, None).await;
    assert_eq!(reserved, Err(PrevError::Reserved(FORMAT.to_owned())));
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

/// Asserts that this branch, over a store the previous release wrote (an object and its journal,
/// with no format object), runs a reconciler pass that copies the object to the cold directory
/// without a failure and writes no format object, and that this branch's proxy then puts and
/// reads as it did: a store without a format object is no pause.
#[tokio::test]
async fn this_branch_reconciles_what_the_previous_release_wrote_and_writes_no_format() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-format-old-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-format-old-cold");
    let before = PrevProxy::open(
        Arc::new(PrevDir::open(hot_dir.path()).unwrap()),
        Arc::new(PrevDir::open(cold_dir.path()).unwrap()),
        PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock)),
    )
    .await
    .unwrap();
    before
        .put("from-before", Bytes::from_static(b"old bytes"))
        .await
        .unwrap();

    let hot = Arc::new(DirBackend::open(hot_dir.path()).unwrap());
    let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
    let mut reconciler = Reconciler::new(
        hot.clone(),
        cold.clone(),
        ReconcilerConfig::new("new-r", BUDGET, Arc::new(SystemClock)),
    );
    let report = reconciler.pass().await;
    assert!(report.held, "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.copied, 1);
    let copied = cold.get("from-before", None).await.unwrap();
    assert_eq!(copied, Bytes::from_static(b"old bytes"));
    let format = hot.head(FORMAT).await;
    assert_eq!(format, Err(Error::NotFound(FORMAT.to_owned())));

    let now = Proxy::open(
        hot.clone(),
        cold,
        ProxyConfig::new("new", BUDGET, Arc::new(SystemClock)),
    )
    .await
    .unwrap();
    now.put("after", Bytes::from_static(b"new bytes"))
        .await
        .unwrap();
    assert_eq!(
        now.get("after", None).await.unwrap(),
        Bytes::from_static(b"new bytes")
    );
}
