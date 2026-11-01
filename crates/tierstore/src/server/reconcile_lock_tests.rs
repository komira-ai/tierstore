//! The reconciler of a storage node under `--lock-file`: it works only while its process holds
//! the lock file, and two processes of one node hand the lease over with the lock.
//!
//! The runtime's clock is paused and moved by the test, no test asserts a time, and a wait for
//! the reconciler is a count of steps of the paused clock with a yield between them, which a
//! broken reconciler fails and does not hang.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use super::cli::{Gate, reconcile_gated};
use super::lock::try_take;
use crate::{
    Backend, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, Reconciler, ReconcilerConfig,
    TestClock,
};

const MEBIBYTE: u64 = 1 << 20;

/// The key of the reconciler's lease in the hot tier.
const LEASE: &str = "_tiering/lease";

/// The tiers of a storage node after a proxy wrote one object: the hot tier, the hot tier as the
/// reconciler sees it (counting its calls), and the memory of the cold tier.
struct Tiers {
    hot: Arc<MemoryBackend>,
    seen: Arc<FaultyBackend>,
    cold_mem: Arc<MemoryBackend>,
}

impl Tiers {
    /// The tiers, and a reconciler over them.
    async fn new() -> (Self, Reconciler) {
        let clock = Arc::new(TestClock::new());
        let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let seen = Arc::new(FaultyBackend::new(hot.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
        config.linger = Duration::from_millis(1);
        let opened = Proxy::open(hot.clone(), cold.clone(), config).await;
        let proxy = opened.unwrap();
        proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
        let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock);
        let reconciler = Reconciler::new(seen.clone(), cold, settings);
        let tiers = Self {
            hot,
            seen,
            cold_mem,
        };
        (tiers, reconciler)
    }

    /// The calls the reconciler has made to the hot tier that read or write.
    fn hot_calls(&self) -> u64 {
        let ops = [Op::Put, Op::PutIfMatch, Op::Get, Op::Head, Op::List];
        ops.into_iter().map(|op| self.seen.calls(op)).sum()
    }

    /// Moves the paused clock by `step` and yields, until the lease is (`present`) or is not in
    /// the hot tier, for at most a hundred thousand steps.
    async fn lease(&self, present: bool, step: Duration) {
        for _ in 0..100_000 {
            if self.hot.get(LEASE, None).await.is_ok() == present {
                return;
            }
            tokio::time::advance(step).await;
            tokio::task::yield_now().await;
        }
        let what = if present { "taken" } else { "given up" };
        panic!("the lease was not {what}");
    }
}

/// Asserts that a reconciler whose lock file another process holds makes no call to the hot tier
/// (no lease, no read of the journals) and copies nothing while it is held, through twelve
/// seconds of a retry every second and two intervals of the reconciler; and that when the lock is
/// free the next try takes it, the first pass takes the lease and copies the object, and a halt
/// gives the lease up. It would catch a reconciler of a second process that takes the lease of
/// the first while the first drains.
#[tokio::test(start_paused = true)]
async fn a_reconciler_under_a_held_lock_file_does_nothing_until_it_takes_the_lock() {
    let (tiers, reconciler) = Tiers::new().await;
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-held");
    let path = dir.path().join("instance.lock");
    let holder = try_take(&path).unwrap().unwrap();
    let gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let halt = Arc::new(Notify::new());
    let every = Duration::from_secs(5);
    let working = reconcile_gated(reconciler, every, Arc::clone(&halt), gate);
    let watching = async {
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(tiers.hot_calls(), 0);
        assert!(tiers.cold_mem.get("k", None).await.is_err());
        drop(holder);
        tiers.lease(true, Duration::from_secs(1)).await;
        let mut copied = false;
        for _ in 0..100_000 {
            copied = tiers.cold_mem.get("k", None).await.is_ok();
            if copied {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(copied, "the object was never copied to the cold tier");
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(tiers.hot.get(LEASE, None).await.is_err());
}

/// Asserts that a reconciler that holds its lock file works at once, and when the file is
/// replaced (removed and made again by another process, which holds the new one) it gives its
/// lease up at the next interval, takes none under the new file while the other holds it, and
/// works again when the other is gone. It would catch a reconciler that goes on under a lock on
/// a file nothing else looks at, beside a second reconciler that holds the new one.
#[tokio::test(start_paused = true)]
async fn a_reconciler_stops_when_its_lock_file_is_replaced_and_works_again_under_the_new_one() {
    let (tiers, reconciler) = Tiers::new().await;
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-replaced");
    let path = dir.path().join("instance.lock");
    let gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let halt = Arc::new(Notify::new());
    let every = Duration::from_secs(5);
    let working = reconcile_gated(reconciler, every, Arc::clone(&halt), gate);
    let watching = async {
        tiers.lease(true, Duration::ZERO).await;
        std::fs::remove_file(&path).unwrap();
        let other = try_take(&path).unwrap().unwrap();
        tiers.lease(false, every).await;
        let calls = tiers.hot_calls();
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(tiers.hot_calls(), calls);
        assert!(tiers.hot.get(LEASE, None).await.is_err());
        drop(other);
        tiers.lease(true, Duration::from_secs(1)).await;
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(tiers.hot.get(LEASE, None).await.is_err());
}

/// Asserts that a lock file that cannot be used stops the start: a link is not followed, and a
/// directory that is not there is not made.
#[tokio::test]
async fn a_lock_file_that_cannot_be_used_is_an_error_at_the_start() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-refused");
    let target = dir.path().join("target");
    std::fs::write(&target, b"").unwrap();
    let link = dir.path().join("link.lock");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(Gate::open(&link, Duration::from_secs(1)).is_err());
    let missing = dir.path().join("none/instance.lock");
    assert!(Gate::open(&missing, Duration::from_secs(1)).is_err());
}

/// Asserts that a reconciler halted while another process holds its lock file ends, and has made
/// no call to the hot tier, and that the lock is still the other's: a v-next stopped before
/// v-current exits (a rollback of the hand-over) leaves the lease of v-current alone. It would
/// catch a waiter that does not end when it is halted (the drain of its process would not end
/// either), and one that gives up or takes a lease it never held.
#[tokio::test(start_paused = true)]
async fn a_reconciler_halted_while_it_waits_for_the_lock_ends_and_touches_nothing() {
    let (tiers, reconciler) = Tiers::new().await;
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-halted");
    let path = dir.path().join("instance.lock");
    let holder = try_take(&path).unwrap().unwrap();
    let gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let halt = Arc::new(Notify::new());
    let every = Duration::from_secs(5);
    let working = reconcile_gated(reconciler, every, Arc::clone(&halt), gate);
    let watching = async {
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert_eq!(tiers.hot_calls(), 0);
    assert_eq!(tiers.seen.calls(Op::Delete), 0);
    assert!(try_take(&path).unwrap().is_none());
    drop(holder);
}

/// The calls `seen` has passed to the hot tier, of every kind a lease or a pass makes.
fn calls_of(seen: &FaultyBackend) -> u64 {
    let ops = [
        Op::Put,
        Op::PutIfMatch,
        Op::Get,
        Op::Head,
        Op::List,
        Op::Delete,
    ];
    ops.into_iter().map(|op| seen.calls(op)).sum()
}

/// The lines of the lease object in `hot` (the holder, the nonce, the generation), or none.
async fn lease_lines(hot: &MemoryBackend) -> Vec<String> {
    match hot.get(LEASE, None).await {
        Ok(body) => {
            let text = String::from_utf8(body.to_vec()).unwrap();
            text.lines().map(str::to_owned).collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Moves the paused clock by `step` and yields, until the lease in `hot` names `holder`, for at
/// most a hundred thousand steps; the lines of the lease then.
async fn held_by(hot: &MemoryBackend, holder: &str, step: Duration) -> Vec<String> {
    for _ in 0..100_000 {
        let lines = lease_lines(hot).await;
        if lines.first().is_some_and(|first| first == holder) {
            return lines;
        }
        tokio::time::advance(step).await;
        tokio::task::yield_now().await;
    }
    panic!("the lease was never held by {holder}");
}

/// Asserts the hand-over of the reconciler between two processes of one storage node, over one
/// hot tier and one lock file, with the proxy ids of their slots as holder names. v-current
/// (`tierstore-9301`) holds the lock file and the lease. v-next (`tierstore-9302`) makes no call
/// to the hot tier while v-current holds the lock, so no `Lease::ensure` (its calls are counted,
/// through twelve tries of the lock). v-current, halted, gives the lease up while it still holds
/// the lock: its delete of the lease is held in flight on the paused clock, and the lock is not
/// free all that while. v-next then takes the lock, and at its first pass the lease: the lease
/// names it, and its generation is 1, so the first `ensure` after the lock found the lease free.
/// The clock of the lease never moves, so the lease was not taken over by expiry. It would catch
/// a process that lets the lock go before it gives the lease up (its successor's first `ensure`
/// finds the lease held, and the node reconciles nothing for an interval), and a v-next that
/// asks for the lease before it holds the lock.
#[tokio::test(start_paused = true)]
async fn v_next_takes_the_lease_v_current_gave_up_before_it_let_the_lock_go() {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::with_clock(
        clock.clone(),
    ))));
    let mut config = ProxyConfig::new("tierstore-9301", MEBIBYTE, clock.clone());
    config.linger = Duration::from_millis(1);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
    let current_hot = Arc::new(FaultyBackend::new(hot.clone()));
    let next_hot = Arc::new(FaultyBackend::new(hot.clone()));
    let reconciler = |seen: &Arc<FaultyBackend>, holder: &str| {
        let settings = ReconcilerConfig::new(holder, MEBIBYTE, clock.clone());
        Reconciler::new(seen.clone(), cold.clone(), settings)
    };
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-hand-over");
    let path = dir.path().join("instance.lock");
    let current_gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let next_gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let current_halt = Arc::new(Notify::new());
    let next_halt = Arc::new(Notify::new());
    let every = Duration::from_secs(5);
    let current = reconcile_gated(
        reconciler(&current_hot, "tierstore-9301"),
        every,
        Arc::clone(&current_halt),
        current_gate,
    );
    let next = reconcile_gated(
        reconciler(&next_hot, "tierstore-9302"),
        every,
        Arc::clone(&next_halt),
        next_gate,
    );
    let watching = async {
        let _ = held_by(&hot, "tierstore-9301", Duration::ZERO).await;
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(calls_of(&next_hot), 0);
        assert_eq!(lease_lines(&hot).await[0], "tierstore-9301");

        // The release of v-current is held in flight: its delete waits on the paused clock.
        let deletes = current_hot.calls(Op::Delete);
        current_hot.set_delay(Op::Delete, Some(Duration::from_secs(3)));
        current_halt.notify_one();
        for _ in 0..100_000 {
            if current_hot.calls(Op::Delete) > deletes {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            current_hot.calls(Op::Delete) > deletes,
            "the lease was never given up"
        );
        assert!(lease_lines(&hot).await.first().is_some());
        let free = try_take(&path).unwrap();
        assert!(free.is_none(), "the lock was let go before the lease");
        drop(free);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls_of(&next_hot), 0);

        let lines = held_by(&hot, "tierstore-9302", Duration::from_secs(1)).await;
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[2], "1", "{lines:?}");
        next_halt.notify_one();
    };
    tokio::join!(current, next, watching);
    assert!(hot.get(LEASE, None).await.is_err());
}
