//! The reconciler's loop: it works round after round until it is halted.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use crate::{
    Created, Entries, FaultyBackend, MemoryBackend, Mono, Op, Outcome, ReconcilerConfig, Step,
    Take, TestClock,
};

use super::reconcile;
use super::rounds::Rounds;
use crate::Backend;

const MEBIBYTE: u64 = 1 << 20;

/// Makes the hot tier active and lands a put of `k` in it on its entry, as a proxy leaves it.
async fn land_k(hot: &Arc<MemoryBackend>, clock: &Arc<TestClock>) {
    let active = Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
    hot.put("_tiering/format", active).await.unwrap();
    hot.put("k", Bytes::from_static(b"v")).await.unwrap();
    let client = Entries::new(hot.clone(), "p1", clock.clone());
    let Created::Taken(held) = client.create("k", Take::Put).await.unwrap() else {
        panic!("the entry exists");
    };
    let landed = client.advance(&held, Step::LandPut).await.unwrap();
    assert!(matches!(landed, Outcome::Done(_)));
}

/// Asserts that the reconciler's loop works at once, tries again, after the delay of an item that
/// failed, a copy that failed, and gives its lease up when it is halted: the copy that failed in
/// the first round is made in a later one, and the lease is gone after the halt. The delay of the
/// item is on the monotonic clock of the reconciler, which the test moves.
#[tokio::test(start_paused = true)]
async fn the_reconciler_works_round_after_round_until_halted() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    land_k(&hot, &clock).await;
    cold.fail_next(Op::Put, 1);
    let mono = Arc::new(Mono::new());
    let mut settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    settings.mono = mono.clone();
    let rounds = Rounds::new(hot.clone(), cold.clone(), settings, Duration::from_secs(60));
    let halt = Arc::new(Notify::new());
    let working = reconcile(rounds, Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(cold_mem.get("k", None).await.is_err());
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        mono.advance(Duration::from_secs(2));
        tokio::time::sleep(Duration::from_secs(3)).await;
        let copied = cold_mem.get("k", None).await.unwrap();
        assert_eq!(copied, Bytes::from_static(b"v"));
        assert!(hot.get("_tiering/lease", None).await.is_ok());
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(hot.get("_tiering/lease", None).await.is_err());
}

/// Asserts that a halt cuts a round that is still running and gives the lease up at once: a copy
/// to a cold tier that takes an hour is not waited for, the object is not copied, and the lease
/// is gone.
#[tokio::test(start_paused = true)]
async fn a_halt_cuts_the_round_that_is_running() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
    land_k(&hot, &clock).await;
    cold.set_delay(Op::Put, Some(Duration::from_secs(3600)));
    let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock.clone());
    let rounds = Rounds::new(hot.clone(), cold.clone(), settings, Duration::from_secs(60));
    let halt = Arc::new(Notify::new());
    let working = reconcile(rounds, Arc::clone(&halt));
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
