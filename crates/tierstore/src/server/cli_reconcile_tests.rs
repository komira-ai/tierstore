//! The reconciler's loop: it works every interval until it is halted.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use crate::{
    Backend, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, Reconciler, ReconcilerConfig,
    TestClock,
};

use super::reconcile;

const MEBIBYTE: u64 = 1 << 20;

/// Asserts that the reconciler's loop works at once and then at every interval, tries again at
/// the next interval a copy that failed, and gives its lease up when it is halted: the copy that
/// failed in the first pass is made in the second, and the lease is gone after the halt.
#[tokio::test(start_paused = true)]
async fn the_reconciler_works_every_interval_until_halted() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.fail_next(Op::Put, 1);
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(cold_mem.get("k", None).await.is_err());
        tokio::time::sleep(Duration::from_secs(5)).await;
        let copied = cold_mem.get("k", None).await.unwrap();
        assert_eq!(copied, Bytes::from_static(b"v"));
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(hot.get("_tiering/lease", None).await.is_err());
}

/// Asserts that a halt cuts a pass that is still running and gives the lease up at once: a copy
/// to a cold tier that takes an hour is not waited for, the object is not copied, and the lease
/// is gone.
#[tokio::test(start_paused = true)]
async fn a_halt_cuts_the_pass_that_is_running() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    cold.set_delay(Op::Put, Some(Duration::from_secs(3600)));
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let reconciler = Reconciler::new(hot.clone(), cold.clone(), settings);
    let halt = Arc::new(Notify::new());
    let working = reconcile(reconciler, Duration::from_secs(5), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    let both = async { tokio::join!(working, watching) };
    let done = tokio::time::timeout(Duration::from_secs(60), both).await;
    assert!(done.is_ok());
    assert!(hot.get("_tiering/lease", None).await.is_err());
    assert!(cold_mem.get("k", None).await.is_err());
}
