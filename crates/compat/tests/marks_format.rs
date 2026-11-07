//! A store whose format object says `active-marks` and whose registrations carry the fields of the
//! marker protocol, shared in directories with the release before it. The release before is the real
//! `tierstore` library of its commit (`tierstore_prev`), not a copy of its behaviour; that release is
//! older than the format object, so it reads none: what is held to it here is that it serves,
//! lists and leaves alone a store that has these objects, and that this branch makes no write under
//! the phase and serves what the release before wrote.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore::{
    Backend, DirBackend, Error, Marker, MarkerState, Proxy, ProxyConfig, SystemClock, encode_marker,
};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, Proxy as PrevProxy,
    ProxyConfig as PrevProxyConfig, Reconciler as PrevReconciler,
    ReconcilerConfig as PrevReconcilerConfig, SystemClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const FORMAT: &str = "_tiering/format";
const REGISTRATION: &str = "_tiering/proxies/marks-node";
const BUDGET: u64 = 1 << 20;

/// The format object of the phase `active-marks`: the fixture of the stored formats.
const ACTIVE_MARKS: &[u8] =
    include_bytes!("../../tierstore/tests/stored_formats/fixtures/format_active_marks.json");

/// The registration of a proxy that reports the phase it acts on and its writes in flight.
const REGISTRATION_BODY: &[u8] =
    br#"{"proto":3,"boot":"b","hb":4,"phase":"active-marks","writes_in_flight":0}"#;

/// A marker body for `key`.
fn marker(key: &str) -> Bytes {
    encode_marker(&Marker {
        v: 1,
        key: key.to_owned(),
        st: MarkerState::Dirty,
        gone: false,
        dirty: true,
        why: None,
        owner: None,
        nonce: "00112233445566778899aabbccddeeff".to_owned(),
        extra: Default::default(),
    })
}

/// Asserts that the previous release, started over a hot directory that holds an object, the format
/// object of `active-marks` and a registration with the new fields, with a marks directory beside it
/// that holds a marker, lists only the object, reads it, puts a new one, and runs a pass of its
/// reconciler without a failure that leaves the format object, the registration and the marks
/// directory byte for byte as they were.
#[tokio::test]
async fn the_previous_release_serves_a_store_whose_format_says_active_marks() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-format-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-format-cold");
    let marks_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-format-marks");
    let hot = DirBackend::open(hot_dir.path()).unwrap();
    let marks = DirBackend::open(marks_dir.path())
        .unwrap()
        .with_locked_keys();
    hot.put("plain", Bytes::from_static(b"plain bytes"))
        .await
        .unwrap();
    hot.put(FORMAT, Bytes::from_static(ACTIVE_MARKS))
        .await
        .unwrap();
    hot.put(REGISTRATION, Bytes::from_static(REGISTRATION_BODY))
        .await
        .unwrap();
    marks.put("plain", marker("plain")).await.unwrap();

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
    before
        .put("added", Bytes::from_static(b"added"))
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
    assert_eq!(before_hot.get(FORMAT, None).await.unwrap(), ACTIVE_MARKS);
    assert_eq!(
        before_hot.get(REGISTRATION, None).await.unwrap(),
        REGISTRATION_BODY
    );
    assert_eq!(marks.get("plain", None).await.unwrap(), marker("plain"));
    let kept: Vec<String> = marks
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert_eq!(kept, ["plain"]);
}

/// Asserts that this branch, over the hot directory in which the previous release put objects and
/// left its registration, under a format object that says `active-marks`, reads and lists what the
/// previous release put, and, as a proxy that has no marks bucket, makes no write: a put is
/// answered `Unavailable` with the text that says writes wait for the active format, and the
/// previous release's objects are as they were.
#[tokio::test]
async fn this_branch_serves_what_the_previous_release_put_and_writes_nothing_under_active_marks() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-format-old-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-marks-format-old-cold");
    let before_hot = Arc::new(PrevDir::open(hot_dir.path()).unwrap());
    let before = PrevProxy::open(
        before_hot.clone(),
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
    hot.put(FORMAT, Bytes::from_static(ACTIVE_MARKS))
        .await
        .unwrap();
    let mut config = ProxyConfig::new("new", BUDGET, Arc::new(SystemClock));
    config.format_refresh = Duration::ZERO;
    let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
    let now = Proxy::open(hot.clone(), cold, config).await.unwrap();

    let listed = now.list("").await.unwrap();
    let keys: Vec<String> = listed.into_iter().map(|object| object.key).collect();
    assert_eq!(keys, ["from-before"]);
    let read = now.get("from-before", None).await.unwrap();
    assert_eq!(read, Bytes::from_static(b"old bytes"));
    let refused = now.put("new-key", Bytes::from_static(b"new")).await;
    let Err(Error::Unavailable(text)) = refused else {
        panic!("{refused:?}");
    };
    assert!(
        text.contains("writes wait until the format object says active"),
        "{text}"
    );
    assert!(now.head("new-key").await.is_err());
    assert_eq!(hot.get(FORMAT, None).await.unwrap(), ACTIVE_MARKS);
}
