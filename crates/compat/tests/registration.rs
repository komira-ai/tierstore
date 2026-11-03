//! The registration of a proxy, `_tiering/proxies/<id>`, shared by this branch and the release
//! before it in one directory. This branch writes a JSON body where that release wrote an empty
//! object. The release before is the real `tierstore` library of its commit (`tierstore_prev`), its
//! own proxy and reconciler, not a copy of its behaviour.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore::{
    Backend, DirBackend, MemoryBackend, Mono, Proxy, ProxyConfig, Reconciler, ReconcilerConfig,
    SystemClock,
};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, MemoryBackend as PrevMemory, Proxy as PrevProxy,
    ProxyConfig as PrevProxyConfig, Reconciler as PrevReconciler,
    ReconcilerConfig as PrevReconcilerConfig, SystemClock as PrevClock,
};
use tierstore_testkit::dir::TestDir;

const REGISTRATION: &str = "_tiering/proxies/p1";

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// Asserts that the previous release's reconciler finds the journal of a proxy of this branch
/// through its registration and copies what the proxy wrote, whether the registration has the body
/// `open` writes (a count of zero) or the body of a beat (a count above zero): it lists the names
/// of `_tiering/proxies/` and never reads the body.
#[tokio::test]
async fn the_previous_release_reconciles_a_proxy_of_this_branch_whatever_its_registration_says() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-registration-new");
    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let cold = Arc::new(MemoryBackend::new());
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(SystemClock));
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold, config).await.unwrap();
    let opened = hot.get(REGISTRATION, None).await.unwrap();
    assert!(opened.starts_with(b"{"), "{opened:?}");

    let before_hot = Arc::new(PrevDir::open(dir.path()).unwrap());
    let before_cold = Arc::new(PrevMemory::new());
    let settings = PrevReconcilerConfig::new("old", 1 << 20, Arc::new(PrevClock));
    let mut reconciler = PrevReconciler::new(before_hot, before_cold.clone(), settings);

    proxy.put("k1", body("one")).await.unwrap();
    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.failed.len()), (1, 0), "{report:?}");
    assert_eq!(before_cold.get("k1", None).await.unwrap(), body("one"));

    let tag = hot.head(REGISTRATION).await.unwrap().tag;
    let beat = body(r#"{"proto":1,"boot":"b","hb":4,"a_later_field":true}"#);
    hot.put_if_match(REGISTRATION, beat, &tag).await.unwrap();
    proxy.put("k2", body("two")).await.unwrap();
    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.failed.len()), (1, 0), "{report:?}");
    assert_eq!(before_cold.get("k2", None).await.unwrap(), body("two"));
}

/// Asserts the other direction: a proxy of this branch opens over the empty registration the
/// previous release's proxy wrote, and leaves it; this branch's reconciler, a retention and more
/// after it first saw that registration, still finds the previous proxy's journal and copies what it
/// wrote, and does not remove its registration (an empty one is of a process that never beats).
#[tokio::test]
async fn this_branch_reads_the_registration_of_the_previous_release_and_never_removes_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-registration-old");
    let before_hot = Arc::new(PrevDir::open(dir.path()).unwrap());
    let mut old = PrevProxyConfig::new("p1", 1 << 20, Arc::new(PrevClock));
    old.linger = Duration::from_millis(1);
    let old_proxy = PrevProxy::open(before_hot, Arc::new(PrevMemory::new()), old)
        .await
        .unwrap();
    old_proxy.put("k1", body("one")).await.unwrap();

    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    assert_eq!(hot.get(REGISTRATION, None).await.unwrap().len(), 0);
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(SystemClock));
    config.linger = Duration::from_millis(1);
    let _new_proxy = Proxy::open(hot.clone(), Arc::new(MemoryBackend::new()), config)
        .await
        .unwrap();
    assert_eq!(hot.get(REGISTRATION, None).await.unwrap().len(), 0);

    let mono = Arc::new(Mono::new());
    let cold = Arc::new(MemoryBackend::new());
    let mut settings = ReconcilerConfig::new("new", 1 << 20, Arc::new(SystemClock));
    settings.mono = mono.clone();
    let retention = settings.registration_retention;
    let mut reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.failed.len()), (1, 0), "{report:?}");
    assert_eq!(cold.get("k1", None).await.unwrap(), body("one"));

    mono.advance(retention + Duration::from_secs(1));
    let report = reconciler.pass().await;
    assert_eq!(report.failed.len(), 0, "{report:?}");
    assert!(hot.head(REGISTRATION).await.is_ok());
}
