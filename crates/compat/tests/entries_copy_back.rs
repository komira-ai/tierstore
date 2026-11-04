//! A store in which this branch asks for a copy-back on an entry (the format object says epoch 2
//! and active), shared in one directory with the release before it. The release before is the real
//! `tierstore` library of its commit (`tierstore_prev`), not a copy of its behaviour.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use bytes::Bytes;
use tierstore::{Backend, DirBackend, MemoryBackend, Proxy, ProxyConfig, SystemClock};
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

/// A proxy of this branch over `hot` and `cold` whose format says that it works on entries.
async fn proxy_on_entries(hot: Arc<DirBackend>, cold: Arc<MemoryBackend>) -> Proxy {
    hot.put(FORMAT, body(r#"{"epoch":2,"phase":"active"}"#))
        .await
        .unwrap();
    let config = ProxyConfig::new("new", BUDGET, Arc::new(SystemClock));
    Proxy::open(hot, cold, config).await.unwrap()
}

/// Asserts that the previous release, over the hot directory in which this branch asked for a
/// copy-back of a cold object, lists no key for the entry (the entries are in a subdirectory the
/// previous release does not read), finds no object under the key in the hot tier, and reads the
/// object from its own cold tier: the entry changes nothing the previous release sees.
#[tokio::test]
async fn the_previous_release_sees_nothing_of_a_copy_back_asked_for_on_an_entry() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-copy-back-new");
    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let cold = Arc::new(MemoryBackend::new());
    cold.put("k", body("cold bytes")).await.unwrap();
    let proxy = proxy_on_entries(hot.clone(), cold).await;
    assert_eq!(proxy.get("k", None).await.unwrap(), body("cold bytes"));
    while hot.list("_entries/").await.unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    assert!(hot.head("k").await.is_err());

    let before_hot = Arc::new(PrevDir::open(dir.path()).unwrap());
    let before_cold = Arc::new(PrevMemory::new());
    before_cold.put("k", body("cold bytes")).await.unwrap();
    let before = PrevProxy::open(
        before_hot.clone(),
        before_cold,
        PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock)),
    )
    .await
    .unwrap();
    let listed: Vec<String> = before_hot
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|object| object.key)
        .filter(|key| !key.starts_with("_tiering/"))
        .collect();
    assert!(listed.is_empty(), "{listed:?}");
    assert_eq!(before.get("k", None).await.unwrap(), body("cold bytes"));
}

/// Asserts the other direction: an object the previous release's proxy copied back into the hot
/// directory is a hot hit for this branch, which then asks for no copy-back and makes no entry.
#[tokio::test]
async fn this_branch_on_entries_serves_what_the_previous_release_copied_back_and_asks_for_nothing()
{
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-copy-back-old");
    let before_hot = Arc::new(PrevDir::open(dir.path()).unwrap());
    before_hot
        .put("k", body("copied back by the release before"))
        .await
        .unwrap();

    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let proxy = proxy_on_entries(hot.clone(), Arc::new(MemoryBackend::new())).await;
    assert_eq!(
        proxy.get("k", None).await.unwrap(),
        body("copied back by the release before")
    );
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(hot.list("_entries/").await.unwrap().is_empty());
}
