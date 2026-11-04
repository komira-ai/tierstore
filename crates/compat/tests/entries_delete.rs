//! A store in which this branch makes deletes on entries (the format object says epoch 2 and
//! active), shared in one directory with the release before it. The release before is the real
//! `tierstore` library of its commit (`tierstore_prev`), not a copy of its behaviour.

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
/// first heartbeat is acknowledged, so that it may acknowledge a put or a delete.
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

/// The proxy of the release before over the hot directory `dir`.
async fn proxy_before(dir: &std::path::Path) -> PrevProxy {
    let hot = Arc::new(PrevDir::open(dir).unwrap());
    let mut config = PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock));
    config.linger = std::time::Duration::from_millis(1);
    PrevProxy::open(hot, Arc::new(PrevMemory::new()), config)
        .await
        .unwrap()
}

/// Asserts that this branch, deleting on entries over a directory in which the previous release's
/// proxy put an object, deletes it (neither proxy finds it afterwards) and leaves no entry.
#[tokio::test]
async fn this_branch_deletes_on_an_entry_what_the_previous_release_put() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-entries-delete-old");
    let before = proxy_before(dir.path()).await;
    before
        .put("old", body("by the release before"))
        .await
        .unwrap();

    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let proxy = proxy_on_entries(hot.clone()).await;
    proxy.delete("old").await.unwrap();
    assert!(matches!(proxy.head("old").await, Err(Error::NotFound(_))));
    let seen = before.head("old").await;
    assert!(matches!(seen, Err(tierstore_prev::Error::NotFound(_))));
    assert!(hot.list("_entries/").await.unwrap().is_empty());
}

/// Asserts that the previous release, over the hot directory in which this branch put an object
/// on an entry and deleted it (leaving a delete entry in the subdirectory the previous release does
/// not read), finds the key gone and may put it again.
#[tokio::test]
async fn the_previous_release_finds_gone_what_this_branch_deleted_on_an_entry() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-entries-delete-new");
    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let proxy = proxy_on_entries(hot.clone()).await;
    proxy.put("k", body("on an entry")).await.unwrap();
    proxy.delete("k").await.unwrap();
    assert!(!hot.list("_entries/").await.unwrap().is_empty());

    let before = proxy_before(dir.path()).await;
    let gone = before.get("k", None).await;
    assert!(matches!(gone, Err(tierstore_prev::Error::NotFound(_))));
    before.put("k", body("again")).await.unwrap();
    assert_eq!(before.get("k", None).await.unwrap(), body("again"));
}
