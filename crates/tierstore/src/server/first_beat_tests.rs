//! The wait for the proxy's first beat.

use std::sync::Arc;
use std::time::Duration;

use super::wait_first_beat;
use crate::{FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, TestClock, TokioSleeper};

async fn proxy_over(hot: Arc<FaultyBackend>) -> Proxy {
    let clock = Arc::new(TestClock::new());
    let cold = Arc::new(MemoryBackend::new());
    let config = ProxyConfig::new("p1", 1000, clock);
    Proxy::open(hot, cold, config).await.unwrap()
}

/// Asserts that the wait ends as soon as the first beat is acknowledged and says so, and that a
/// proxy whose beat is not started is waited for the whole limit and reported not live.
#[tokio::test(start_paused = true)]
async fn the_wait_ends_at_the_first_beat_and_at_the_limit_without_one() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let proxy = proxy_over(hot).await;
    assert!(!wait_first_beat(&proxy, Duration::from_secs(10)).await);

    assert!(proxy.start_heartbeat(Arc::new(TokioSleeper)));
    assert!(wait_first_beat(&proxy, Duration::from_secs(5)).await);
}

/// Asserts that a hot tier that does not answer the beat for longer than the limit does not hold
/// the start: the wait gives up at the limit and says the proxy is not live.
#[tokio::test(start_paused = true)]
async fn a_hot_tier_that_does_not_answer_the_beat_does_not_hold_the_start() {
    let mem = Arc::new(MemoryBackend::new());
    let hot = Arc::new(FaultyBackend::new(mem));
    let proxy = proxy_over(hot.clone()).await;
    hot.set_delay(Op::Put, Some(Duration::from_secs(3600)));
    hot.set_delay(Op::Head, Some(Duration::from_secs(3600)));
    assert!(proxy.start_heartbeat(Arc::new(TokioSleeper)));
    assert!(!wait_first_beat(&proxy, Duration::from_secs(5)).await);
}
