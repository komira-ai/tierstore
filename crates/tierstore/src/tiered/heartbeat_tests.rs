//! The heartbeat of a proxy: the registration it writes, what moves `live_until`, what does not,
//! and the stop.
//!
//! The loop waits through a sleeper the test steps by hand: it says when a beat is done (the loop
//! has reached its wait) and goes on only when told. The monotonic clock is a [`Mono`] the test
//! advances, so no test waits and no assertion is about elapsed time.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use super::pause;
use crate::faulty::{FaultyBackend, Op};
use crate::journal::{FORMAT_KEY, proxy_key};
use crate::registration::{PROTO, Registration};
use crate::{
    Backend, Error, GateBackend, MemoryBackend, Mono, Phase, Proxy, ProxyConfig, Sleeper, TestClock,
};

const PERIOD: Duration = Duration::from_secs(10);

/// A sleeper that records what it was asked for, tells the test it was reached, and returns only
/// when the test lets it.
struct Stepped {
    asked: Mutex<Vec<Duration>>,
    reached: Semaphore,
    go: Semaphore,
}

impl Stepped {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            reached: Semaphore::new(0),
            go: Semaphore::new(0),
        })
    }

    /// Returns when the loop has finished a beat and is waiting.
    async fn beat_done(&self) {
        self.reached.acquire().await.unwrap().forget();
    }

    /// Lets the loop leave its wait.
    fn next(&self) {
        self.go.add_permits(1);
    }

    fn asked(&self) -> Vec<Duration> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sleeper for Stepped {
    async fn sleep(&self, duration: Duration) {
        self.asked.lock().unwrap().push(duration);
        self.reached.add_permits(1);
        self.go.acquire().await.unwrap().forget();
    }
}

struct Rig {
    memory: Arc<MemoryBackend>,
    hot: Arc<FaultyBackend>,
    mono: Arc<Mono>,
    proxy: Proxy,
}

async fn rig_over(memory: Arc<MemoryBackend>) -> Rig {
    let hot = Arc::new(FaultyBackend::new(memory.clone()));
    let cold = Arc::new(MemoryBackend::new());
    let mono = Arc::new(Mono::new());
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(TestClock::new()));
    config.mono = mono.clone();
    let proxy = Proxy::open(hot.clone(), cold, config).await.unwrap();
    Rig {
        memory,
        hot,
        mono,
        proxy,
    }
}

async fn rig() -> Rig {
    rig_over(Arc::new(MemoryBackend::new())).await
}

async fn registration(memory: &MemoryBackend) -> Option<Registration> {
    let body = memory.get(&proxy_key("p1"), None).await.ok()?;
    Registration::decode(&body)
}

/// Asserts that a proxy that has not started its heartbeat has written a registration that says
/// so (`hb` zero, so the reconciler never counts it as departed) and is not live.
#[tokio::test]
async fn open_writes_a_registration_of_a_process_that_does_not_beat_and_is_not_live() {
    let rig = rig().await;
    let written = registration(&rig.memory).await.unwrap();
    assert_eq!((written.proto, written.hb), (PROTO, 0));
    assert!(!written.boot.is_empty());
    assert!(!rig.proxy.is_live());
}

/// Asserts that a registration that exists (the empty one of an older release) is left as it is by
/// `open`, which still succeeds, and that the first beat then replaces it.
#[tokio::test]
async fn an_older_registration_is_left_by_open_and_replaced_by_the_first_beat() {
    let memory = Arc::new(MemoryBackend::new());
    memory
        .put(&proxy_key("p1"), bytes::Bytes::new())
        .await
        .unwrap();
    let rig = rig_over(memory).await;
    assert_eq!(
        rig.memory.get(&proxy_key("p1"), None).await.unwrap().len(),
        0
    );

    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert_eq!(registration(&rig.memory).await.unwrap().hb, 1);
    rig.proxy.stop_heartbeat().await;
}

/// Asserts the beats: the first is done at once and the loop then waits one period through the
/// sleeper; every beat writes a count one higher, with the same boot id; the proxy is live for the
/// departure time less the margin from the time the request was sent, by the monotonic clock (50
/// seconds of the default 60 less 10), and the second beat, sent at 50 seconds, moves that on to 100.
#[tokio::test]
async fn each_beat_writes_the_next_count_and_moves_live_until_to_its_send_time_plus_the_span() {
    let rig = rig().await;
    let boot = registration(&rig.memory).await.unwrap().boot;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));

    sleeper.beat_done().await;
    let first = registration(&rig.memory).await.unwrap();
    assert_eq!((first.hb, first.boot.as_str()), (1, boot.as_str()));
    assert!(rig.proxy.is_live());

    rig.mono.advance(Duration::from_secs(49));
    assert!(rig.proxy.is_live());
    rig.mono.advance(Duration::from_secs(1));
    assert!(!rig.proxy.is_live(), "live until 50 s after the send at 0");

    sleeper.next();
    sleeper.beat_done().await;
    let second = registration(&rig.memory).await.unwrap();
    assert_eq!((second.hb, second.boot.as_str()), (2, boot.as_str()));
    assert!(rig.proxy.is_live(), "the beat sent at 50 s lives to 100 s");
    assert_eq!(sleeper.asked(), [PERIOD, PERIOD]);
    rig.proxy.stop_heartbeat().await;
}

/// Asserts that a beat the store does not acknowledge moves nothing: after a failed replacement
/// `live_until` is still the one the first beat gave, the count does not advance, and the next
/// beat reads the registration again (a HEAD) and replaces what it finds, which moves it on from
/// the time of that beat's send.
#[tokio::test]
async fn a_beat_that_fails_leaves_live_until_alone_and_the_next_beat_reads_the_object_again() {
    let rig = rig().await;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert_eq!(rig.hot.calls(Op::Head), 1);

    rig.hot.fail_next(Op::PutIfMatch, 1);
    rig.mono.advance(Duration::from_secs(30));
    sleeper.next();
    sleeper.beat_done().await;
    assert_eq!(registration(&rig.memory).await.unwrap().hb, 1);
    rig.mono.advance(Duration::from_secs(20));
    assert!(
        !rig.proxy.is_live(),
        "the first beat's 50 s ran out at 50 s"
    );

    sleeper.next();
    sleeper.beat_done().await;
    assert_eq!(rig.hot.calls(Op::Head), 2);
    assert_eq!(registration(&rig.memory).await.unwrap().hb, 2);
    assert!(rig.proxy.is_live(), "sent at 50 s, live to 100 s");
    rig.proxy.stop_heartbeat().await;
}

/// Asserts that a registration deleted under a proxy that beats is written again: the beat that
/// finds it gone fails, the next one creates it with the count it was at.
#[tokio::test]
async fn a_registration_deleted_under_a_beating_proxy_is_created_again() {
    let rig = rig().await;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;

    rig.memory.delete(&proxy_key("p1")).await.unwrap();
    sleeper.next();
    sleeper.beat_done().await;
    assert!(registration(&rig.memory).await.is_none());

    sleeper.next();
    sleeper.beat_done().await;
    assert_eq!(registration(&rig.memory).await.unwrap().hb, 2);
    rig.proxy.stop_heartbeat().await;
}

/// Asserts that a beat whose head fails with something other than "not found" is a failed beat
/// (nothing is written, `live_until` stays), and that the loop goes on.
#[tokio::test]
async fn a_beat_whose_read_fails_writes_nothing() {
    let rig = rig().await;
    rig.hot.fail_next(Op::Head, 1);
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert_eq!(registration(&rig.memory).await.unwrap().hb, 0);
    assert!(!rig.proxy.is_live());

    sleeper.next();
    sleeper.beat_done().await;
    assert_eq!(registration(&rig.memory).await.unwrap().hb, 1);
    assert!(rig.proxy.is_live());
    rig.proxy.stop_heartbeat().await;
}

/// Asserts the lifecycle: a second start while it runs does nothing, a stop waits for the task and
/// no replacement is made after it returns, a second stop is harmless, and a stopped heartbeat is
/// not started again (the proxy has said it is leaving).
#[tokio::test]
async fn the_heartbeat_starts_once_stops_for_good_and_writes_nothing_after_the_stop() {
    let rig = rig().await;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    assert!(!rig.proxy.start_heartbeat(Stepped::new()));
    sleeper.beat_done().await;

    rig.proxy.stop_heartbeat().await;
    let writes = rig.hot.calls(Op::PutIfMatch) + rig.hot.calls(Op::Put);
    sleeper.next();
    tokio::task::yield_now().await;
    assert_eq!(
        rig.hot.calls(Op::PutIfMatch) + rig.hot.calls(Op::Put),
        writes
    );

    rig.proxy.stop_heartbeat().await;
    assert!(!rig.proxy.start_heartbeat(Stepped::new()));
}

/// Asserts that stopping a heartbeat that was never started is harmless and closes it too.
#[tokio::test]
async fn a_heartbeat_that_was_never_started_can_be_stopped_and_then_cannot_start() {
    let rig = rig().await;
    rig.proxy.stop_heartbeat().await;
    assert!(!rig.proxy.start_heartbeat(Stepped::new()));
}

/// Asserts that the heartbeat of a proxy that is dropped ends by itself: the task's stop channel
/// closes with the proxy, so no task is left beating over the store.
#[tokio::test]
async fn dropping_the_proxy_ends_its_heartbeat() {
    let rig = rig().await;
    let sleeper = Stepped::new();
    assert!(rig.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    let hot = rig.hot.clone();
    drop(rig.proxy);
    sleeper.next();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(hot.calls(Op::PutIfMatch), 1);
}

fn config(mono: &Arc<Mono>) -> ProxyConfig {
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(TestClock::new()));
    config.mono = mono.clone();
    config
}

/// Asserts that a process that opens over the registration of an earlier process that beat
/// replaces it with its own count of zero (so the leader, seeing it never change, still never
/// removes it), and that the first beat of this process then takes it on from there.
#[tokio::test]
async fn open_over_the_registration_of_an_earlier_process_that_beat_writes_the_zero_count() {
    let first = rig().await;
    let sleeper = Stepped::new();
    assert!(first.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    first.proxy.stop_heartbeat().await;
    let before = registration(&first.memory).await.unwrap();
    assert_eq!(before.hb, 1);

    let second = rig_over(first.memory.clone()).await;
    let after = registration(&second.memory).await.unwrap();
    assert_eq!(after.hb, 0);
    assert_ne!(after.boot, before.boot);
    assert!(!second.proxy.is_live());

    let sleeper = Stepped::new();
    assert!(second.proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert_eq!(registration(&second.memory).await.unwrap().hb, 1);
    second.proxy.stop_heartbeat().await;
}

/// Asserts that an open that cannot read or replace the registration it finds fails with that
/// error and does not go on: a failed head, a failed read and a refused replacement are each returned, and the registration is left as it was.
#[tokio::test]
async fn open_fails_with_the_error_of_a_registration_it_cannot_read_or_replace() {
    let memory = Arc::new(MemoryBackend::new());
    let body = Registration {
        proto: 1,
        boot: "b".into(),
        hb: 4,
    }
    .encode();
    memory.put(&proxy_key("p1"), body.clone()).await.unwrap();
    let mono = Arc::new(Mono::new());
    for op in [Op::Head, Op::Get, Op::PutIfMatch] {
        let hot = Arc::new(FaultyBackend::new(memory.clone()));
        hot.fail_next(op, 1);
        let cold = Arc::new(MemoryBackend::new());
        let opened = Proxy::open(hot, cold, config(&mono)).await;
        assert!(matches!(opened, Err(Error::Unavailable(_))), "{op:?}");
        assert_eq!(memory.get(&proxy_key("p1"), None).await.unwrap(), body);
    }
}

/// Asserts that a failure to create the registration, other than "already there", is returned by
/// `open`.
#[tokio::test]
async fn open_fails_when_the_registration_cannot_be_created() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    hot.fail_next(Op::Put, 1);
    let cold = Arc::new(MemoryBackend::new());
    let opened = Proxy::open(hot, cold, config(&Arc::new(Mono::new()))).await;
    assert!(matches!(opened, Err(Error::Unavailable(_))));
}

/// Asserts that two stop calls that overlap a beat in progress both wait for it: the beat is held
/// at the store, neither call returns while it is held, and both have returned, with the beat
/// written, once it is released.
#[tokio::test]
async fn overlapping_stop_calls_both_wait_for_the_beat_in_progress() {
    let memory = Arc::new(MemoryBackend::new());
    let gated = Arc::new(GateBackend::new(memory.clone()));
    let cold = Arc::new(MemoryBackend::new());
    let proxy = Proxy::open(gated.clone(), cold, config(&Arc::new(Mono::new())))
        .await
        .unwrap();
    let held = gated.gate(Op::PutIfMatch, &proxy_key("p1"), Phase::Pre);
    assert!(proxy.start_heartbeat(Stepped::new()));

    let returned = AtomicUsize::new(0);
    let (proxy_ref, returned_ref) = (&proxy, &returned);
    let stop = || async move {
        proxy_ref.stop_heartbeat().await;
        returned_ref.fetch_add(1, Ordering::SeqCst);
    };
    let release = async {
        held.parked().await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(returned.load(Ordering::SeqCst), 0);
        assert!(held.release());
    };
    tokio::join!(stop(), stop(), release);

    assert_eq!(returned.load(Ordering::SeqCst), 2);
    assert_eq!(registration(&memory).await.unwrap().hb, 1);
    proxy.stop_heartbeat().await;
}

/// Asserts that each beat reads the format object and keeps what it says without any request: the
/// proxy read "no format" at its first look and would not read again for an hour, and the beat
/// after the object was written takes up the epoch; a beat whose read fails changes nothing; the
/// beat that finds the object gone takes the epoch away.
#[tokio::test]
async fn a_beat_takes_up_the_format_without_a_request() {
    let memory = Arc::new(MemoryBackend::new());
    let hot = Arc::new(FaultyBackend::new(memory.clone()));
    let cold = Arc::new(MemoryBackend::new());
    let mut settings = config(&Arc::new(Mono::new()));
    settings.format_refresh = Duration::from_secs(3600);
    let proxy = Proxy::open(hot.clone(), cold, settings).await.unwrap();
    assert!(!proxy.inner.entries_active().await);
    let active = bytes::Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
    memory.put(FORMAT_KEY, active).await.unwrap();
    assert!(!proxy.inner.entries_active().await);

    let sleeper = Stepped::new();
    assert!(proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert!(proxy.inner.entries_active().await);

    memory.delete(FORMAT_KEY).await.unwrap();
    hot.fail_next_on(Op::Get, FORMAT_KEY, Error::Unavailable("down".to_owned()));
    sleeper.next();
    sleeper.beat_done().await;
    assert!(proxy.inner.entries_active().await);

    sleeper.next();
    sleeper.beat_done().await;
    assert!(!proxy.inner.entries_active().await);
    proxy.stop_heartbeat().await;
}

const ACTIVE: &[u8] = br#"{"epoch":2,"phase":"active"}"#;
const DRAINED: &[u8] = br#"{"phase":"drained"}"#;

/// A proxy over `hot` that does not read the format object again for an hour.
async fn proxy_over(hot: Arc<dyn Backend>) -> Proxy {
    let mut settings = config(&Arc::new(Mono::new()));
    settings.format_refresh = Duration::from_secs(3600);
    Proxy::open(hot, Arc::new(MemoryBackend::new()), settings)
        .await
        .unwrap()
}

/// Asserts that a look at the format by the heartbeat does not overtake the read of a request: a
/// request has read "no format" and is held before it is told, the format becomes `active`, and the
/// look, started while the request is held, waits for it. Once the request is released the look
/// reads the newer object, so the proxy ends with the epoch and not with the older answer.
#[tokio::test]
async fn a_look_at_the_format_that_starts_during_a_request_read_keeps_the_newer_object() {
    let memory = Arc::new(MemoryBackend::new());
    let hot = Arc::new(GateBackend::new(memory.clone()));
    let proxy = proxy_over(hot.clone()).await;
    let gate = hot.gate(Op::Get, FORMAT_KEY, Phase::Post);
    let inner = Arc::clone(&proxy.inner);
    let request = tokio::spawn(async move { inner.entries_active().await });
    gate.parked().await;
    memory
        .put(FORMAT_KEY, bytes::Bytes::from_static(ACTIVE))
        .await
        .unwrap();

    let look = pause::refresh(
        &proxy.inner.pause,
        &proxy.inner.format_gate,
        memory.as_ref(),
        Duration::from_secs(3600),
    );
    let release = async {
        tokio::task::yield_now().await;
        assert!(gate.release());
    };
    tokio::join!(look, release);

    assert!(!request.await.unwrap());
    assert!(proxy.inner.entries_active().await);
}

/// Asserts the other order: a look at the format has read `active` and is held before it keeps it,
/// the format becomes `drained`, and a write that comes while the look is held does not read the
/// object beside it: it waits, and when the look is released it uses what the look kept, so the
/// older read is not replaced by a read that ran at the same time.
#[tokio::test]
async fn a_request_that_comes_during_a_look_at_the_format_waits_for_it_and_uses_what_it_kept() {
    let memory = Arc::new(MemoryBackend::new());
    memory
        .put(FORMAT_KEY, bytes::Bytes::from_static(ACTIVE))
        .await
        .unwrap();
    let proxy = proxy_over(memory.clone()).await;
    let looking = Arc::new(GateBackend::new(memory.clone()));
    let gate = looking.gate(Op::Get, FORMAT_KEY, Phase::Post);
    let inner = Arc::clone(&proxy.inner);
    let look = tokio::spawn(async move {
        pause::refresh(
            &inner.pause,
            &inner.format_gate,
            looking.as_ref(),
            Duration::from_secs(3600),
        )
        .await;
    });
    gate.parked().await;
    memory.delete(FORMAT_KEY).await.unwrap();
    memory
        .put(FORMAT_KEY, bytes::Bytes::from_static(DRAINED))
        .await
        .unwrap();

    let inner = Arc::clone(&proxy.inner);
    let request = tokio::spawn(async move { inner.admit_write("k").await });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(!request.is_finished());
    assert!(gate.release());
    look.await.unwrap();

    request.await.unwrap().unwrap();
}
