//! The pace of the heartbeat when the hot store holds a call: the period runs from the start of a
//! beat, so a beat that was cut after its limit is followed at once.
//!
//! As in `heartbeat_tests`, the loop waits through a sleeper the test steps by hand and the
//! monotonic clock is a [`Mono`] the test advances, so no test waits and no assertion is about
//! elapsed time: the duration the loop asks its sleeper for is a value.

use std::sync::Arc;
use std::time::Duration;

use super::heartbeat_tests::Stepped;
use crate::faulty::Op;
use crate::journal::proxy_key;
use crate::registration::Registration;
use crate::{
    Backend, Error, GateBackend, MemoryBackend, Mono, Phase, Proxy, ProxyConfig, TestClock,
};

const PERIOD: Duration = Duration::from_secs(10);

struct Rig {
    memory: Arc<MemoryBackend>,
    hot: Arc<GateBackend>,
    mono: Arc<Mono>,
    proxy: Proxy,
}

async fn rig() -> Rig {
    let memory = Arc::new(MemoryBackend::new());
    let hot = Arc::new(GateBackend::new(memory.clone()));
    let mono = Arc::new(Mono::new());
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(TestClock::new()));
    config.mono = mono.clone();
    let proxy = Proxy::open(hot.clone(), Arc::new(MemoryBackend::new()), config)
        .await
        .unwrap();
    Rig {
        memory,
        hot,
        mono,
        proxy,
    }
}

async fn count(memory: &MemoryBackend) -> u64 {
    let body = memory.get(&proxy_key("p1"), None).await.unwrap();
    Registration::decode(&body).unwrap().hb
}

/// Asserts that a beat held by the store for the whole period (the call is cut at its limit, one
/// period) is followed by the next beat at once and not after another period: with one of the
/// store's endpoints stalled, the beat that was sent to it costs the registration one period and
/// not two. The beat after it replaces the registration and moves `live_until` on from its own
/// send time.
#[tokio::test]
async fn a_beat_cut_after_a_whole_period_is_followed_at_once() {
    let rig = rig().await;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert_eq!(count(&rig.memory).await, 1);

    let held = rig.hot.gate(Op::PutIfMatch, &proxy_key("p1"), Phase::Pre);
    sleeper.next();
    held.parked().await;
    rig.mono.advance(PERIOD);
    assert!(held.release_with(Error::Unavailable("held by a stalled endpoint".into())));
    sleeper.beat_done().await;
    assert_eq!(count(&rig.memory).await, 1, "the held beat wrote nothing");

    sleeper.next();
    sleeper.beat_done().await;
    assert_eq!(count(&rig.memory).await, 2);
    assert!(rig.proxy.is_live(), "the beat sent at 10 s lives to 60 s");
    assert_eq!(sleeper.asked(), [PERIOD, Duration::ZERO, PERIOD]);
    rig.proxy.stop_heartbeat().await;
}

/// Asserts that a beat that fails at once still waits the whole period before the next: the
/// store that refuses quickly is not asked in a tight loop.
#[tokio::test]
async fn a_beat_that_fails_at_once_waits_the_whole_period() {
    let rig = rig().await;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;

    let refused = rig.hot.gate(Op::PutIfMatch, &proxy_key("p1"), Phase::Pre);
    sleeper.next();
    refused.parked().await;
    assert!(refused.release_with(Error::Unavailable("connection refused".into())));
    sleeper.beat_done().await;

    assert_eq!(count(&rig.memory).await, 1);
    assert_eq!(sleeper.asked(), [PERIOD, PERIOD]);
    rig.proxy.stop_heartbeat().await;
}
