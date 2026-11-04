//! A store in which this branch makes puts on entries (the format object says epoch 2 and active),
//! shared in one directory with the release before it. The release before is the real `tierstore`
//! library of its commit (`tierstore_prev`), not a copy of its behaviour.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use bytes::Bytes;
use tierstore::{
    Backend, DirBackend, Error, MemoryBackend, Proxy, ProxyConfig, SystemClock, TokioSleeper,
};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, MemoryBackend as PrevMemory, Proxy as PrevProxy,
    ProxyConfig as PrevProxyConfig, SystemClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const FORMAT: &str = "_tiering/format";
const BUDGET: u64 = 1 << 20;

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// A proxy of this branch over `hot` whose format says that writes are made on entries, and whose
/// first heartbeat is acknowledged, so that it may acknowledge a put.
async fn proxy_on_entries(hot: Arc<DirBackend>) -> Proxy {
    hot.put(FORMAT, body(r#"{"epoch":2,"phase":"active"}"#))
        .await
        .unwrap();
    let config = ProxyConfig::new("new", BUDGET, Arc::new(SystemClock));
    let proxy = Proxy::open(hot, Arc::new(MemoryBackend::new()), config)
        .await
        .unwrap();
    assert!(proxy.start_heartbeat(Arc::new(TokioSleeper)));
    // The first beat is made at once; the next is a heartbeat period away.
    while !proxy.is_live() {
        tokio::task::yield_now().await;
    }
    proxy
}

/// Asserts that the previous release, over the hot directory in which this branch made a put on an
/// entry, reads the object the put wrote (its bytes and size), lists it and no entry (the entries
/// are in a subdirectory the previous release does not read), and refuses to put the key again.
#[tokio::test]
async fn the_previous_release_reads_what_a_put_on_an_entry_wrote_and_sees_no_entry() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-entries-put-new");
    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let proxy = proxy_on_entries(hot.clone()).await;
    proxy.put("k", body("on an entry")).await.unwrap();
    assert!(!hot.list("_entries/").await.unwrap().is_empty());

    let before_hot = Arc::new(PrevDir::open(dir.path()).unwrap());
    let before = PrevProxy::open(
        before_hot.clone(),
        Arc::new(PrevMemory::new()),
        PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock)),
    )
    .await
    .unwrap();
    assert_eq!(before.get("k", None).await.unwrap(), body("on an entry"));
    assert_eq!(before.head("k").await.unwrap().size, 11);
    let listed: Vec<String> = before_hot
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|object| object.key)
        .filter(|key| !key.starts_with("_tiering/"))
        .collect();
    assert_eq!(listed, ["k"]);
    let again = before.put("k", body("again")).await;
    assert!(matches!(
        again,
        Err(tierstore_prev::Error::AlreadyExists(_))
    ));
}

/// Asserts the other direction: this branch, putting on entries over a directory in which the
/// previous release's proxy put an object, reads that object, refuses a put of its key as
/// existing before it makes an entry, and puts a new key on an entry that the previous release
/// then reads.
#[tokio::test]
async fn this_branch_on_entries_reads_what_the_previous_release_put_and_refuses_its_key() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-entries-put-old");
    let before_hot = Arc::new(PrevDir::open(dir.path()).unwrap());
    let mut old = PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock));
    old.linger = std::time::Duration::from_millis(1);
    let before = PrevProxy::open(before_hot, Arc::new(PrevMemory::new()), old)
        .await
        .unwrap();
    before
        .put("old", body("by the release before"))
        .await
        .unwrap();

    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let proxy = proxy_on_entries(hot.clone()).await;
    assert_eq!(
        proxy.get("old", None).await.unwrap(),
        body("by the release before")
    );
    let again = proxy.put("old", body("again")).await;
    assert!(matches!(again, Err(Error::AlreadyExists(_))), "{again:?}");
    assert!(hot.list("_entries/").await.unwrap().is_empty());

    proxy.put("new", body("on an entry")).await.unwrap();
    assert_eq!(before.get("new", None).await.unwrap(), body("on an entry"));
}
