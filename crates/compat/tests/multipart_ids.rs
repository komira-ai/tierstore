//! The ids of multipart uploads across two releases. This branch starts an upload in the hot tier
//! whenever it has room and names it with a prefix on the hot tier's own id; an id without the
//! prefix is a cold upload, as every id of the release before this branch was. The release before
//! is the real `tierstore` library of its commit (`tierstore_prev`), over the same directories.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tierstore::{Backend, DirBackend, Error, Proxy, ProxyConfig, SystemClock};
use tierstore::{PartBody, TokioSleeper};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, MemoryBackend as PrevMemory,
    PartBody as PrevBody, Proxy as PrevProxy, ProxyConfig as PrevProxyConfig,
    SystemClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const BUDGET: u64 = 1 << 20;

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn part(text: &'static str) -> PartBody<'static> {
    stream::iter(vec![Ok(body(text))]).boxed()
}

fn prev_part(text: &'static str) -> PrevBody<'static> {
    stream::iter(vec![Ok(body(text))]).boxed()
}

/// A live proxy of this branch over the hot directory `hot` and the cold directory `cold`, on
/// entries; the hot tier is full if `full`.
async fn proxy(hot: &TestDir, cold: &TestDir, full: bool) -> Proxy {
    let hot = Arc::new(DirBackend::open(hot.path()).unwrap());
    hot.put("_tiering/format", body(r#"{"epoch":2,"phase":"active"}"#))
        .await
        .unwrap();
    if full {
        let usage = format!(r#"{{"hot_bytes":{BUDGET},"copy_lag":0,"time":0}}"#);
        hot.put("_tiering/usage", Bytes::from(usage)).await.unwrap();
    }
    let cold = Arc::new(DirBackend::open(cold.path()).unwrap());
    let config = ProxyConfig::new("new", BUDGET, Arc::new(SystemClock));
    let proxy = Proxy::open(hot, cold, config).await.unwrap();
    assert!(proxy.start_heartbeat(Arc::new(TokioSleeper)));
    while !proxy.is_live() {
        tokio::task::yield_now().await;
    }
    proxy
}

/// The release before this branch, over the cold directory `cold` and a hot tier of its own.
async fn before(cold: &TestDir) -> PrevProxy {
    let cold = Arc::new(PrevDir::open(cold.path()).unwrap());
    let config = PrevProxyConfig::new("old", BUDGET, Arc::new(PrevClock));
    PrevProxy::open(Arc::new(PrevMemory::new()), cold, config)
        .await
        .unwrap()
}

/// Asserts that an upload the release before this branch started, and put a part to, is completed
/// by this branch through its id as it is (no prefix): the object is made in the cold directory,
/// not in the hot one, and the id is not taken for a hot upload.
#[tokio::test]
async fn this_branch_completes_an_upload_the_previous_release_started_in_cold() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-ids-old-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-ids-old-cold");
    let old = before(&cold_dir).await;
    let id = old.create_upload("k").await.unwrap();
    let tag = old.upload_part("k", &id, 1, prev_part("by the release before"));
    let tag = tag.await.unwrap();

    let new = proxy(&hot_dir, &cold_dir, false).await;
    let listed = new.list_uploads("", None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    new.complete_upload("k", &id, &[(1, tag)]).await.unwrap();

    let cold = DirBackend::open(cold_dir.path()).unwrap();
    assert_eq!(
        cold.get("k", None).await.unwrap(),
        body("by the release before")
    );
    let hot = DirBackend::open(hot_dir.path()).unwrap();
    assert!(matches!(hot.head("k").await, Err(Error::NotFound(_))));
}

/// Asserts the other direction for a cold upload: an upload this branch started while its hot tier
/// was full has the cold directory's own id, and the release before this branch can put a part to
/// it and complete it.
#[tokio::test]
async fn the_previous_release_completes_an_upload_this_branch_started_in_cold() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-ids-new-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-ids-new-cold");
    let new = proxy(&hot_dir, &cold_dir, true).await;
    let id = new.create_upload("k").await.unwrap();
    assert!(!id.contains('~'), "{id}");

    let old = before(&cold_dir).await;
    let tag = old.upload_part("k", &id, 1, prev_part("by this branch"));
    let tag = tag.await.unwrap();
    old.complete_upload("k", &id, &[(1, tag)]).await.unwrap();
    let cold = DirBackend::open(cold_dir.path()).unwrap();
    assert_eq!(cold.get("k", None).await.unwrap(), body("by this branch"));
}

/// Asserts what the release before this branch does with an upload this branch started in the hot
/// tier: it does not know the id (the id names no upload of its cold tier), so a part is answered
/// not found and nothing is stored. The client starts the upload again. This is the one place where
/// the two releases differ for data the other wrote, and it ends with the upload.
#[tokio::test]
async fn the_previous_release_does_not_know_an_upload_this_branch_started_in_hot() {
    let hot_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-ids-hot-hot");
    let cold_dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-ids-hot-cold");
    let new = proxy(&hot_dir, &cold_dir, false).await;
    let id = new.create_upload("k").await.unwrap();
    assert!(id.contains('~'), "{id}");
    let tag = new.upload_part("k", &id, 1, part("kept")).await.unwrap();

    let old = before(&cold_dir).await;
    let refused = old.upload_part("k", &id, 2, prev_part("lost")).await;
    assert!(
        matches!(refused, Err(tierstore_prev::Error::NotFound(_))),
        "{refused:?}"
    );
    let cold = DirBackend::open(cold_dir.path()).unwrap();
    assert!(cold.list_uploads("", None, 10).await.unwrap().is_empty());

    // This branch still holds the upload, and completes it in the hot tier.
    new.complete_upload("k", &id, &[(1, tag)]).await.unwrap();
    let hot = DirBackend::open(hot_dir.path()).unwrap();
    assert_eq!(hot.get("k", None).await.unwrap(), body("kept"));
}
