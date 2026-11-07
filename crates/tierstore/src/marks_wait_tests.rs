//! Waiting for a lock to end, and the lease a holder keeps its lock by: the polls of the config,
//! the timeout, and the refresh and the destroy window, all on a clock the test moves.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::{
    Backend, Clock, Error, Gate, GateBackend, MARKER_VERSION, MarkMove, MarkSeen, Marker,
    MarkerState, MarkerWhy, Markers, MarkersConfig, MarkersConfigError, MemoryBackend, Mono,
    Outcome, Phase, RecordingSleeper, Sleeper, TestClock, Waited, Written, encode_marker,
};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// What happens to the marker when the wait has paused so many times.
struct Then {
    after: usize,
    key: String,
    /// The body that takes the place of the lock, or none if it is removed.
    instead: Option<Bytes>,
}

/// A sleeper that moves the monotonic clock by the pause it is asked for, as a real wait would,
/// writes the pause down, and, when the test says so, changes the marker once the wait has paused
/// that many times.
struct Ticking {
    mono: Arc<Mono>,
    memory: Arc<MemoryBackend>,
    asked: Mutex<Vec<Duration>>,
    then: Option<Then>,
}

impl Ticking {
    fn asked(&self) -> Vec<Duration> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Sleeper for Ticking {
    async fn sleep(&self, duration: Duration) {
        self.mono.advance(duration);
        let pauses = {
            let mut asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
            asked.push(duration);
            asked.len()
        };
        let Some(then) = self.then.as_ref().filter(|then| then.after == pauses) else {
            return;
        };
        self.memory.delete(&then.key).await.unwrap();
        if let Some(instead) = &then.instead {
            self.memory.put(&then.key, instead.clone()).await.unwrap();
        }
    }
}

struct Rig {
    mono: Arc<Mono>,
    memory: Arc<MemoryBackend>,
    store: Arc<FaultyBackend>,
}

impl Rig {
    fn new() -> Self {
        let memory = Arc::new(MemoryBackend::with_clock(Arc::new(TestClock::new())));
        Self {
            mono: Arc::new(Mono::new()),
            store: Arc::new(FaultyBackend::new(memory.clone())),
            memory,
        }
    }

    fn ticking(&self, then: Option<Then>) -> Arc<Ticking> {
        Arc::new(Ticking {
            mono: self.mono.clone(),
            memory: self.memory.clone(),
            asked: Mutex::new(Vec::new()),
            then,
        })
    }

    /// The actor `id` over the store, on this rig's clock, with `sleeper`.
    fn actor(&self, id: &str, config: MarkersConfig, sleeper: Arc<dyn Sleeper>) -> Markers {
        Markers::with_boot(
            self.store.clone(),
            id,
            config,
            self.mono.clone(),
            sleeper,
            id,
        )
        .expect("the config holds together")
    }

    /// The actor `id` with the default config, and a sleeper that returns at once.
    fn plain(&self, id: &str) -> Markers {
        let sleeper = Arc::new(RecordingSleeper::new());
        self.actor(id, MarkersConfig::default(), sleeper)
    }

    async fn seed(&self, marker: &Marker) {
        let body = encode_marker(marker);
        self.memory.put(&marker.key, body).await.unwrap();
    }
}

fn marker(st: MarkerState, why: Option<MarkerWhy>, owner: Option<&str>) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: "k".to_owned(),
        st,
        gone: false,
        dirty: true,
        why,
        owner: owner.map(str::to_owned),
        nonce: "seed".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// A lock of someone else, which stands until the test changes it.
fn their_lock() -> Marker {
    marker(MarkerState::Lock, Some(MarkerWhy::Overwrite), Some("them"))
}

async fn until_parked<F: std::future::Future + Unpin>(gate: &Gate, call: &mut F) {
    tokio::select! {
        _ = &mut *call => panic!("the call finished instead of parking"),
        () = gate.parked() => {}
    }
}

fn done<T>(outcome: Outcome<T>) -> T {
    match outcome {
        Outcome::Done(value) => value,
        Outcome::Lost => panic!("lost"),
        Outcome::Refused => panic!("refused"),
    }
}

/// The lock of `owner` over a fresh `DIRTY` marker of `k`.
async fn locked(markers: &Markers, rig: &Rig) -> Written {
    rig.seed(&marker(MarkerState::Dirty, None, None)).await;
    let seen = markers.read("k").await.unwrap().unwrap();
    let advanced = markers
        .advance(&seen, MarkMove::Lock(MarkerWhy::Overwrite))
        .await
        .unwrap();
    done(advanced)
}

// ---- wait ----

/// Asserts the wait of 5.2 with the defaults: a lock that never ends is read, then waited for in
/// pauses of 20 ms doubling up to 1 s, the last cut short so that the wait ends at `lock_wait`
/// exactly, and then it is a timeout, which is the writer's 503. The pauses are asserted as values,
/// and the clock is the one the sleeper moves.
#[tokio::test]
async fn a_writer_waits_for_a_lock_with_the_polls_of_the_config_and_gets_503_at_lock_wait() {
    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let sleeper = rig.ticking(None);
    let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());

    let waited = writer.wait("k").await.unwrap();
    assert_eq!(waited, Waited::TimedOut);
    let pauses = [20, 40, 80, 160, 320, 640, 1000, 1000, 1000, 740].map(millis);
    assert_eq!(sleeper.asked(), pauses);
    assert_eq!(rig.mono.now(), 5000);
    assert_eq!(rig.store.calls(Op::Get), 11);
    assert_eq!(rig.store.calls(Op::PutIfMatch), 0);
    assert_eq!(rig.store.calls(Op::Delete), 0);
}

/// Asserts that the polls follow the config: the first pause, the factor and the longest pause are
/// the config's, a factor of 1 polls at a steady pace, and the last pause is cut at `lock_wait`.
#[tokio::test]
async fn the_polls_follow_lock_poll_min_max_and_factor() {
    let cases = [
        (
            millis(100),
            millis(300),
            2,
            millis(1000),
            vec![100, 200, 300, 300, 100],
        ),
        (
            millis(50),
            millis(1000),
            1,
            millis(200),
            vec![50, 50, 50, 50],
        ),
        (
            millis(10),
            millis(1000),
            3,
            millis(500),
            vec![10, 30, 90, 270, 100],
        ),
        (millis(400), millis(400), 2, millis(300), vec![300]),
    ];
    for (min, max, factor, wait, want) in cases {
        let rig = Rig::new();
        rig.seed(&their_lock()).await;
        let sleeper = rig.ticking(None);
        let config = MarkersConfig {
            lock_poll_min: min,
            lock_poll_max: max,
            lock_poll_factor: factor,
            lock_wait: wait,
            ..MarkersConfig::default()
        };
        let writer = rig.actor("w", config, sleeper.clone());
        assert_eq!(writer.wait("k").await.unwrap(), Waited::TimedOut);
        let want: Vec<Duration> = want.into_iter().map(millis).collect();
        assert_eq!(
            sleeper.asked(),
            want,
            "min {min:?} max {max:?} factor {factor}"
        );
        assert_eq!(rig.mono.now(), u64::try_from(wait.as_millis()).unwrap());
    }
}

/// Asserts that a `lock_wait` of zero reads the marker once and gives up without a pause.
#[tokio::test]
async fn a_lock_wait_of_zero_reads_once_and_times_out() {
    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let sleeper = rig.ticking(None);
    let config = MarkersConfig {
        lock_wait: Duration::ZERO,
        ..MarkersConfig::default()
    };
    let writer = rig.actor("w", config, sleeper.clone());
    assert_eq!(writer.wait("k").await.unwrap(), Waited::TimedOut);
    assert!(sleeper.asked().is_empty());
    assert_eq!(rig.store.calls(Op::Get), 1);
}

/// Asserts that a wait ends as soon as the lock does, with the marker it ended in: removed, or
/// replaced by the `DIRTY` marker the holder's release makes, which the writer may then take over
/// by compare-and-swap.
#[tokio::test]
async fn a_wait_ends_when_the_lock_ends_and_gives_the_marker_it_found() {
    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let removed = Then {
        after: 3,
        key: "k".to_owned(),
        instead: None,
    };
    let sleeper = rig.ticking(Some(removed));
    let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
    assert_eq!(writer.wait("k").await.unwrap(), Waited::Free(None));
    assert_eq!(sleeper.asked(), [20, 40, 80].map(millis));
    assert_eq!(rig.store.calls(Op::Get), 4);

    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let released = marker(MarkerState::Dirty, None, None);
    let handed_over = Then {
        after: 2,
        key: "k".to_owned(),
        instead: Some(encode_marker(&released)),
    };
    let sleeper = rig.ticking(Some(handed_over));
    let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
    let Waited::Free(Some(seen)) = writer.wait("k").await.unwrap() else {
        panic!("the wait did not end with a marker");
    };
    assert_eq!(seen.marker.st, MarkerState::Dirty);
    assert_eq!(seen, writer.read("k").await.unwrap().unwrap());
    assert_eq!(sleeper.asked(), [20, 40].map(millis));
}

/// Asserts the boundary: a lock that ends at the last poll, when the wait has run for `lock_wait`
/// exactly, is not a timeout.
#[tokio::test]
async fn a_lock_that_ends_at_lock_wait_is_not_a_timeout() {
    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let removed = Then {
        after: 10,
        key: "k".to_owned(),
        instead: None,
    };
    let sleeper = rig.ticking(Some(removed));
    let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
    assert_eq!(writer.wait("k").await.unwrap(), Waited::Free(None));
    assert_eq!(rig.mono.now(), 5000);
}

/// Asserts which markers a wait does not wait for (none, `DIRTY`, `WANT`) and which it does
/// (`CLEARING`, and a state it does not know).
#[tokio::test]
async fn a_wait_passes_dirty_and_want_and_waits_for_clearing_and_an_unknown_state() {
    let free = [None, Some(MarkerState::Dirty), Some(MarkerState::Want)];
    for st in free {
        let rig = Rig::new();
        if let Some(st) = &st {
            rig.seed(&marker(st.clone(), None, None)).await;
        }
        let sleeper = rig.ticking(None);
        let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
        let Waited::Free(seen) = writer.wait("k").await.unwrap() else {
            panic!("waited for {st:?}");
        };
        assert_eq!(seen.map(|seen| seen.marker.st), st);
        assert!(sleeper.asked().is_empty());
    }
    let held = [
        MarkerState::Clearing,
        MarkerState::Other("FROZEN".to_owned()),
    ];
    for st in held {
        let rig = Rig::new();
        rig.seed(&marker(st.clone(), None, Some("them"))).await;
        let sleeper = rig.ticking(None);
        let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
        assert_eq!(writer.wait("k").await.unwrap(), Waited::TimedOut, "{st:?}");
        assert_eq!(sleeper.asked().len(), 10);
    }
}

/// Asserts that a read of a lock does not wait and does not poll: a reader sees the lock at once
/// and the sleeper is not asked for anything.
#[tokio::test]
async fn a_read_never_waits_on_a_lock() {
    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let sleeper = rig.ticking(None);
    let reader = rig.actor("r", MarkersConfig::default(), sleeper.clone());
    let seen = reader.read("k").await.unwrap().unwrap();
    assert_eq!(seen.marker.st, MarkerState::Lock);
    assert!(sleeper.asked().is_empty());
    assert_eq!(rig.store.calls(Op::Get), 1);
}

/// Asserts that a read that fails ends the wait with that error, at the first look and in the
/// middle of the wait (here, a body that is no longer a marker).
#[tokio::test]
async fn a_wait_whose_read_fails_ends_with_that_error() {
    let rig = Rig::new();
    rig.seed(&their_lock()).await;
    let sleeper = rig.ticking(None);
    let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
    rig.store
        .fail_next_with(Op::Get, Error::Unavailable("down".to_owned()));
    let error = writer.wait("k").await.unwrap_err();
    assert_eq!(error, Error::Unavailable("down".to_owned()));
    assert!(sleeper.asked().is_empty());

    let corrupted = Then {
        after: 2,
        key: "k".to_owned(),
        instead: Some(Bytes::from_static(b"not json")),
    };
    let sleeper = rig.ticking(Some(corrupted));
    let writer = rig.actor("w", MarkersConfig::default(), sleeper.clone());
    let Err(Error::Other(text)) = writer.wait("k").await else {
        panic!("the wait did not end with the error of the read");
    };
    assert!(text.starts_with("corrupt marker: "), "{text}");
    assert_eq!(sleeper.asked(), [20, 40].map(millis));
}

// ---- the lease ----

/// Asserts when a holder is due to write its lock again: not before `lock_refresh` has passed since
/// it sent its last write, and from then on.
#[tokio::test]
async fn a_lock_is_due_for_a_refresh_after_lock_refresh() {
    let rig = Rig::new();
    let holder = rig.plain("h");
    let held = locked(&holder, &rig).await;
    assert!(!holder.refresh_due(&held));
    rig.mono.advance(millis(29_999));
    assert!(!holder.refresh_due(&held));
    rig.mono.advance(millis(1));
    assert!(holder.refresh_due(&held));
}

/// Asserts the window in which a holder may start a call that destroys: its last write of the lock
/// younger than `lock_lease - queued_call_margin` (60 s with the defaults), not at that age or after.
#[tokio::test]
async fn a_holder_may_destroy_only_while_its_lock_is_younger_than_the_lease_less_the_margin() {
    let rig = Rig::new();
    let holder = rig.plain("h");
    let held = locked(&holder, &rig).await;
    assert!(holder.may_destroy(&held));
    rig.mono.advance(millis(59_999));
    assert!(holder.may_destroy(&held));
    rig.mono.advance(millis(1));
    assert!(!holder.may_destroy(&held));
    rig.mono.advance(secs(600));
    assert!(!holder.may_destroy(&held));
}

/// Asserts that a refresh writes the same lock with a fresh nonce, so that the lock moves, and
/// starts the count again: no longer due, and a destroy window of the full 60 s.
#[tokio::test]
async fn a_refresh_writes_the_same_lock_with_a_fresh_nonce_and_starts_the_count_again() {
    let rig = Rig::new();
    let holder = rig.plain("h");
    let held = locked(&holder, &rig).await;
    rig.mono.advance(secs(35));
    assert!(holder.refresh_due(&held));

    let refreshed = done(holder.advance(&held.seen, MarkMove::Refresh).await.unwrap());
    assert_eq!(refreshed.sent_at, 35_000);
    assert_ne!(refreshed.seen.marker.nonce, held.seen.marker.nonce);
    assert_ne!(refreshed.seen.tag, held.seen.tag);
    let same = Marker {
        nonce: refreshed.seen.marker.nonce.clone(),
        ..held.seen.marker.clone()
    };
    assert_eq!(refreshed.seen.marker, same);
    let stored = holder.read("k").await.unwrap().unwrap();
    assert_eq!(stored, refreshed.seen);

    assert!(!holder.refresh_due(&refreshed));
    assert!(holder.may_destroy(&refreshed));
    rig.mono.advance(millis(59_999));
    assert!(holder.may_destroy(&refreshed));
    assert!(!holder.may_destroy(&held));
}

/// Asserts that the age of a lock counts from when its write was sent and not from when the answer
/// came: a refresh whose request waited at the store for ten seconds has the age of those ten
/// seconds already.
#[tokio::test]
async fn the_age_of_a_lock_counts_from_the_request_and_not_the_reply() {
    let rig = Rig::new();
    let gated = Arc::new(GateBackend::new(rig.store.clone()));
    let sleeper = Arc::new(RecordingSleeper::new());
    let config = MarkersConfig::default();
    let mono = rig.mono.clone();
    let holder = Markers::with_boot(gated.clone(), "h", config, mono, sleeper, "h").unwrap();
    let held = locked(&holder, &rig).await;

    let gate = gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut refreshing = Box::pin(holder.advance(&held.seen, MarkMove::Refresh));
    until_parked(&gate, &mut refreshing).await;
    rig.mono.advance(secs(10));
    assert!(gate.release());
    let refreshed = done(refreshing.await.unwrap());
    assert_eq!(refreshed.sent_at, 0);
    assert!(holder.may_destroy(&refreshed));
    rig.mono.advance(millis(50_000));
    assert!(!holder.may_destroy(&refreshed));
}

/// Asserts that a holder that refreshes every `lock_refresh` may destroy for as long as it works,
/// long past `lock_lease`, and that the lock changes at each refresh.
#[tokio::test]
async fn a_holder_that_refreshes_every_lock_refresh_may_destroy_for_as_long_as_it_works() {
    let rig = Rig::new();
    let holder = rig.plain("h");
    let mut held = locked(&holder, &rig).await;
    let mut nonces = vec![held.seen.marker.nonce.clone()];
    for round in 1..=10u64 {
        rig.mono.advance(secs(30));
        assert!(holder.refresh_due(&held), "round {round}");
        assert!(holder.may_destroy(&held), "round {round}");
        held = done(holder.advance(&held.seen, MarkMove::Refresh).await.unwrap());
        nonces.push(held.seen.marker.nonce.clone());
    }
    assert!(rig.mono.now() > 2 * 120_000);
    nonces.sort();
    nonces.dedup();
    assert_eq!(nonces.len(), 11);
}

/// Asserts that a lock whose holder missed its refreshes past `lock_lease` and was reaped by the
/// leader is lost to it: the holder has no destroy window left, its refresh and its release are
/// lost, and the marker stands as the leader made it, `DIRTY` with the lock's `gone` kept.
#[tokio::test]
async fn a_holder_whose_lock_the_leader_reaped_after_the_lease_loses_its_refresh_and_release() {
    let rig = Rig::new();
    let (holder, leader) = (rig.plain("h"), rig.plain("leader"));
    rig.seed(&marker(MarkerState::Dirty, None, None)).await;
    let seen = holder.read("k").await.unwrap().unwrap();
    let gone = Marker {
        gone: true,
        ..seen.marker.clone()
    };
    rig.memory.delete("k").await.unwrap();
    rig.seed(&gone).await;
    let seen = holder.read("k").await.unwrap().unwrap();
    let advanced = holder
        .advance(&seen, MarkMove::Lock(MarkerWhy::Overwrite))
        .await
        .unwrap();
    let held = done(advanced);
    assert!(held.seen.marker.gone);

    rig.mono.advance(millis(120_001));
    assert!(!holder.may_destroy(&held));
    let seen_by_leader = leader.read("k").await.unwrap().unwrap();
    let reaped = done(
        leader
            .advance(&seen_by_leader, MarkMove::Reap)
            .await
            .unwrap(),
    );
    assert_eq!(reaped.seen.marker.st, MarkerState::Dirty);
    assert!(reaped.seen.marker.gone);

    let refresh = holder.advance(&held.seen, MarkMove::Refresh).await.unwrap();
    assert_eq!(refresh, Outcome::Lost);
    let release = MarkMove::Release { changed: false };
    let released = holder.advance(&held.seen, release).await.unwrap();
    assert_eq!(released, Outcome::Lost);
    let stored: MarkSeen = holder.read("k").await.unwrap().unwrap();
    assert_eq!(stored, reaped.seen);
}

/// Asserts that only the owner refreshes a lock: another actor's refresh is refused with no call.
#[tokio::test]
async fn a_refresh_by_an_actor_that_does_not_own_the_lock_is_refused() {
    let rig = Rig::new();
    let (holder, other) = (rig.plain("h"), rig.plain("o"));
    let held = locked(&holder, &rig).await;
    let calls = rig.store.calls(Op::PutIfMatch);
    let refused = other.advance(&held.seen, MarkMove::Refresh).await.unwrap();
    assert_eq!(refused, Outcome::Refused);
    assert_eq!(rig.store.calls(Op::PutIfMatch), calls);
}

// ---- the config ----

/// Asserts the defaults of the design, and that they hold together: the lease is exactly two
/// refreshes plus the margin of a queued call, which the check accepts.
#[test]
fn the_defaults_are_those_of_the_design_and_pass_the_check() {
    let config = MarkersConfig::default();
    assert_eq!(config.lock_wait, secs(5));
    assert_eq!(config.lock_poll_min, millis(20));
    assert_eq!(config.lock_poll_max, secs(1));
    assert_eq!(config.lock_poll_factor, 2);
    assert_eq!(config.lock_lease, secs(120));
    assert_eq!(config.lock_refresh, secs(30));
    assert_eq!(config.queued_call_margin, secs(60));
    assert_eq!(config.check(), Ok(()));
}

/// Asserts each refusal of a config that does not hold together, and the message of the one that
/// names the numbers: a lease below two refreshes plus the margin of a queued call (by a
/// millisecond), a refresh of zero, polls that do not grow or shrink to nothing, a first pause of
/// zero or above the longest, and a refresh so long that twice it overflows.
#[test]
fn a_config_with_a_lock_lease_below_its_refreshes_plus_the_queued_call_margin_is_refused() {
    let config = |change: fn(&mut MarkersConfig)| {
        let mut config = MarkersConfig::default();
        change(&mut config);
        config.check()
    };
    let short = config(|c| c.lock_lease = millis(119_999));
    let too_short = MarkersConfigError::LeaseTooShort {
        lease: millis(119_999),
        refresh: secs(30),
        margin: secs(60),
    };
    assert_eq!(short, Err(too_short.clone()));
    assert_eq!(
        too_short.to_string(),
        "lock_lease (119.999s) is below 2 x lock_refresh (30s) + queued_call_margin (60s)"
    );
    let margin = config(|c| c.queued_call_margin = secs(61));
    assert_eq!(
        margin.map_err(|e| matches!(e, MarkersConfigError::LeaseTooShort { .. })),
        Err(true)
    );
    let huge = config(|c| c.lock_refresh = Duration::MAX);
    assert_eq!(
        huge.map_err(|e| matches!(e, MarkersConfigError::LeaseTooShort { .. })),
        Err(true)
    );

    assert_eq!(
        config(|c| c.lock_refresh = Duration::ZERO),
        Err(MarkersConfigError::RefreshZero)
    );
    assert_eq!(
        config(|c| c.lock_poll_factor = 0),
        Err(MarkersConfigError::PollFactorZero)
    );
    assert_eq!(
        config(|c| c.lock_poll_min = Duration::ZERO),
        Err(MarkersConfigError::PollMinZero)
    );
    assert_eq!(
        config(|c| c.lock_poll_min = secs(2)),
        Err(MarkersConfigError::PollOrder {
            min: secs(2),
            max: secs(1)
        })
    );
}

/// Asserts that a [`Markers`] is not made over a config that does not hold together.
#[test]
fn markers_are_not_made_over_a_config_that_does_not_hold_together() {
    let rig = Rig::new();
    let config = MarkersConfig {
        lock_poll_factor: 0,
        ..MarkersConfig::default()
    };
    let sleeper = Arc::new(RecordingSleeper::new());
    let made = Markers::new(rig.store.clone(), "x", config, rig.mono.clone(), sleeper);
    assert_eq!(made.err(), Some(MarkersConfigError::PollFactorZero));
}
