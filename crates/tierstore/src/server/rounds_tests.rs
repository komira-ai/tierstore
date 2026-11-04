//! The rounds of a node's reconciler: nothing is done until the format says `active` at epoch 2,
//! and then each round is a step of the entry reconciler.
//!
//! The tiers are in memory and the loop waits on a paused clock, so no test reads a clock or waits
//! for a time.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use super::EntryTuning;
use super::Rounds;
use crate::Backend;
use crate::journal::{FORMAT_KEY, LEASE_KEY, USAGE_KEY, Usage};
use crate::server::cli_reconcile::reconcile;
use crate::{
    Created, Entries, Error, FaultyBackend, MemoryBackend, Op, Outcome, ReconcilerConfig, Step,
    Take, TestClock,
};

const BUDGET: u64 = 1 << 20;
const ACTIVE: &str = r#"{"epoch":2,"phase":"active"}"#;
const DEPARTURE: Duration = Duration::from_secs(60);

/// Two tiers in memory, the hot one behind a fault injector.
struct Rig {
    clock: Arc<TestClock>,
    hot_mem: Arc<MemoryBackend>,
    hot: Arc<FaultyBackend>,
    cold: Arc<MemoryBackend>,
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        Self {
            hot: Arc::new(FaultyBackend::new(hot_mem.clone())),
            cold: Arc::new(MemoryBackend::with_clock(clock.clone())),
            hot_mem,
            clock,
        }
    }

    fn rounds(&self) -> Rounds {
        let config = ReconcilerConfig::new("node/reconciler", BUDGET, self.clock.clone());
        Rounds::new(self.hot.clone(), self.cold.clone(), config, DEPARTURE)
    }

    /// As [`Rig::rounds`], made the way the serving paths make them: with `tuning`.
    fn tuned_rounds(&self, tuning: &EntryTuning) -> Rounds {
        let mut config = ReconcilerConfig::new("node/reconciler", BUDGET, self.clock.clone());
        // The usage is paced by the monotonic clock, which the test moves.
        config.mono = self.clock.clone();
        Rounds::tuned(
            self.hot.clone(),
            self.cold.clone(),
            config,
            DEPARTURE,
            tuning,
        )
    }

    /// The times in the usage object after each of five rounds that are 12 seconds apart.
    async fn usage_times(&self, mut rounds: Rounds) -> Vec<u64> {
        let mut times = Vec::new();
        for _ in 0..5 {
            let round = rounds.round().await;
            assert!(round.failed.is_empty(), "{:?}", round.failed);
            self.clock.advance(12_000);
            if let Ok(body) = self.hot_mem.get(USAGE_KEY, None).await {
                let usage: Usage = serde_json::from_slice(&body).unwrap();
                times.push(usage.time);
            }
        }
        rounds.stop().await;
        times.sort_unstable();
        times.dedup();
        times
    }

    /// A put that landed on its entry, as a proxy leaves it.
    async fn landed_put(&self, key: &str) {
        self.hot_mem
            .put(key, Bytes::from_static(b"e"))
            .await
            .unwrap();
        let client = Entries::new(self.hot_mem.clone(), "p2", self.clock.clone());
        let Created::Taken(held) = client.create(key, Take::Put).await.unwrap() else {
            panic!("the entry exists");
        };
        let landed = client.advance(&held, Step::LandPut).await.unwrap();
        assert!(matches!(landed, Outcome::Done(_)));
    }

    async fn set_format(&self, body: &'static str) {
        self.hot_mem.delete(FORMAT_KEY).await.ok();
        self.hot_mem
            .put(FORMAT_KEY, Bytes::from_static(body.as_bytes()))
            .await
            .unwrap();
    }

    async fn cold_has(&self, key: &str) -> bool {
        self.cold.get(key, None).await.is_ok()
    }

    async fn lease_held(&self) -> bool {
        self.hot_mem.get(LEASE_KEY, None).await.is_ok()
    }
}

/// Asserts that with no format object, and with each format that does not run entries (a drain, a
/// drained store, an epoch below 2), a round does nothing: the landed put is not copied, no lease
/// is taken, nothing failed, and the loop is told to wait the idle wait (one second).
#[tokio::test(start_paused = true)]
async fn before_the_epoch_a_round_does_nothing_and_waits() {
    let formats = [
        "",
        r#"{"phase":"drain"}"#,
        r#"{"phase":"drained"}"#,
        r#"{"epoch":1,"phase":"active"}"#,
        r#"{"epoch":2,"phase":"drain"}"#,
    ];
    for format in formats {
        let rig = Rig::new();
        rig.landed_put("k").await;
        if !format.is_empty() {
            rig.set_format(format).await;
        }
        let round = rig.rounds().round().await;
        assert!(round.failed.is_empty(), "{format}: {:?}", round.failed);
        assert_eq!(round.pause, Some(Duration::from_secs(1)), "{format}");
        assert!(!rig.cold_has("k").await, "{format}");
        assert!(!rig.lease_held().await, "{format}");
    }
}

/// Asserts that the round that reads the active format starts the work: a node that did nothing
/// before works the entries from then on (a landed put is copied to the cold tier, its entry
/// removed, the lease taken, and the loop told to go on at once), and gives the lease up when it
/// stops.
#[tokio::test(start_paused = true)]
async fn the_round_that_reads_the_active_format_works_the_entries() {
    let rig = Rig::new();
    rig.landed_put("entered").await;
    let mut rounds = rig.rounds();
    assert!(rounds.round().await.failed.is_empty());
    assert!(!rig.cold_has("entered").await);
    assert!(!rig.lease_held().await);

    rig.set_format(ACTIVE).await;
    let round = rounds.round().await;
    assert!(round.failed.is_empty(), "{:?}", round.failed);
    assert_eq!(round.pause, None);
    assert!(rig.cold_has("entered").await);
    assert!(rig.lease_held().await);
    let next = rounds.round().await;
    assert!(next.failed.is_empty());
    assert!(next.pause.is_some());
    rounds.stop().await;
    assert!(!rig.lease_held().await);
}

/// Asserts that a node that starts when the format is active already works the entries in its first
/// round, and takes the lease for them.
#[tokio::test(start_paused = true)]
async fn a_node_that_starts_after_the_epoch_works_the_entries_at_once() {
    let rig = Rig::new();
    rig.set_format(ACTIVE).await;
    rig.landed_put("entered").await;
    let mut rounds = rig.rounds();
    let round = rounds.round().await;
    assert!(round.failed.is_empty(), "{:?}", round.failed);
    assert_eq!(round.pause, None);
    assert!(rig.cold_has("entered").await);
    assert!(rig.lease_held().await);
    rounds.stop().await;
}

/// Asserts that a format object that cannot be read fails the round with the store's error and
/// does nothing (no copy, no lease), because whether writes are made on entries is not known; the
/// next round, with the object readable, does the work.
#[tokio::test(start_paused = true)]
async fn a_format_that_cannot_be_read_fails_the_round_and_does_nothing() {
    let rig = Rig::new();
    rig.set_format(ACTIVE).await;
    rig.landed_put("k").await;
    let down = Error::Unavailable("format".to_owned());
    rig.hot.fail_next_on(Op::Get, FORMAT_KEY, down.clone());
    let mut rounds = rig.rounds();
    let round = rounds.round().await;
    assert_eq!(round.failed, [down]);
    assert!(!rig.cold_has("k").await);
    assert!(!rig.lease_held().await);
    let next = rounds.round().await;
    assert!(next.failed.is_empty());
    assert!(rig.cold_has("k").await);
}

/// Asserts that the loop works the entries and gives the lease up when it is halted: a landed put
/// is copied by the first round, and the lease is gone after the halt.
#[tokio::test(start_paused = true)]
async fn the_loop_works_the_entries_until_halted_and_gives_the_lease_up() {
    let rig = Rig::new();
    rig.set_format(ACTIVE).await;
    rig.landed_put("entered").await;
    let halt = Arc::new(Notify::new());
    let working = reconcile(rig.rounds(), Arc::clone(&halt));
    let watching = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(rig.cold_has("entered").await);
        assert!(rig.lease_held().await);
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(!rig.lease_held().await);
}

/// Asserts that `Rounds::tuned` hands its `EntryTuning` to the running entry reconciler: with the
/// default `usage_interval` (10 seconds) the usage object is rewritten by rounds 12 seconds apart,
/// and with one of an hour it is published once and left as it was.
#[tokio::test(start_paused = true)]
async fn the_tuning_a_round_is_made_with_reaches_the_running_reconciler() {
    let slow = EntryTuning {
        usage_interval: Duration::from_secs(3600),
        ..EntryTuning::default()
    };
    let rig = Rig::new();
    rig.set_format(ACTIVE).await;
    rig.landed_put("k").await;
    let by_default = rig
        .usage_times(rig.tuned_rounds(&EntryTuning::default()))
        .await;
    assert!(by_default.len() > 1, "{by_default:?}");

    let rig = Rig::new();
    rig.set_format(ACTIVE).await;
    rig.landed_put("k").await;
    let tuned = rig.usage_times(rig.tuned_rounds(&slow)).await;
    assert_eq!(tuned.len(), 1, "{tuned:?}");
}
