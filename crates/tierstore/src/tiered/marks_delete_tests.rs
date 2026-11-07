//! The delete of a key under the format `active-marks`: the requests it makes of each store and in
//! which order, what it leaves in the marker and in the hot tier, what it answers when a lock or a
//! call is in the way, and the gate that opens it.
//!
//! The proxy is over a hot tier and a marks bucket (each in memory behind a fault injector and a
//! gate) and a cold tier that no test lets a delete touch. Pauses go to a sleeper that writes
//! them down and moves the monotonic clock by them; no test asserts a time. The leader is not
//! built yet: where a test needs a lock reaped, it is `MarkMove::Reap` taken by an actor of the
//! test's.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::marks_delete::MarkedDelete;
use crate::faulty::{FaultyBackend, Op};
use crate::journal::FORMAT_KEY;
use crate::{
    Backend, Error, GateBackend, MARKER_VERSION, MarkMove, Marker, MarkerState, MarkerWhy, Markers,
    MarkersConfig, MemoryBackend, Mono, Proxy, ProxyConfig, Sleeper, TestClock, decode_marker,
    encode_marker,
};

/// The format of a store whose writes are made on markers, in the bucket `marks`.
pub(crate) const MARKS_FORMAT: &str =
    r#"{"epoch":3,"marks_bucket":"marks","phase":"active-marks"}"#;
const BUDGET: u64 = 1000;

pub(crate) fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No method panics while holding the lock, so a poisoned one is still whole.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the marker is replaced by once the wait has paused so many times.
struct Then {
    after: usize,
    key: String,
    instead: Marker,
}

/// A sleeper that writes the pause down and moves the monotonic clock by it, as a real wait would,
/// and that a test makes change a marker after so many pauses, or stop at a pause of one length
/// until it lets it go.
pub(crate) struct Ticking {
    mono: Arc<Mono>,
    marks: Arc<MemoryBackend>,
    asked: Mutex<Vec<Duration>>,
    then: Mutex<Option<Then>>,
    stop_at: Mutex<Option<Duration>>,
    reached: Notify,
    go: Notify,
}

impl Ticking {
    pub(crate) fn asked(&self) -> Vec<Duration> {
        locked(&self.asked).clone()
    }

    /// After the `after`th pause, `instead` takes the place of the marker of its key.
    pub(crate) fn then_replace(&self, after: usize, instead: Marker) {
        let key = instead.key.clone();
        *locked(&self.then) = Some(Then {
            after,
            key,
            instead,
        });
    }

    /// The next pause of this length stops until [`Ticking::let_go`].
    pub(crate) fn stop_at(&self, length: Duration) {
        *locked(&self.stop_at) = Some(length);
    }

    /// Returns once a pause is stopped.
    pub(crate) async fn stopped(&self) {
        self.reached.notified().await;
    }

    pub(crate) fn let_go(&self) {
        self.go.notify_one();
    }
}

#[async_trait]
impl Sleeper for Ticking {
    async fn sleep(&self, duration: Duration) {
        self.mono.advance(duration);
        let pauses = {
            let mut asked = locked(&self.asked);
            asked.push(duration);
            asked.len()
        };
        let replace = locked(&self.then).take_if(|then| then.after == pauses);
        if let Some(then) = replace {
            self.marks.delete(&then.key).await.unwrap();
            let body = encode_marker(&then.instead);
            self.marks.put(&then.key, body).await.unwrap();
        }
        let stop = locked(&self.stop_at).take_if(|at| *at == duration);
        if stop.is_some() {
            self.reached.notify_one();
            self.go.notified().await;
        }
    }
}

/// The shape of a marker: state, reason, `gone` and owner.
pub(crate) type Shape = Option<(MarkerState, Option<MarkerWhy>, bool, Option<String>)>;

/// The shape of a `LOCK(why)` of `owner`.
pub(crate) fn lock_shape(why: MarkerWhy, gone: bool, owner: &str) -> Shape {
    Some((MarkerState::Lock, Some(why), gone, Some(owner.to_owned())))
}

/// The shape of a `DIRTY` marker.
pub(crate) fn dirty_shape(gone: bool) -> Shape {
    Some((MarkerState::Dirty, None, gone, None))
}

/// A marker of `key` in the state `st`, with nothing else set but what the caller changes.
pub(crate) fn marker(key: &str, st: MarkerState, gone: bool) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone,
        dirty: true,
        why: None,
        owner: None,
        nonce: "seed".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// A `LOCK(why)` of `owner` on `key`, as an actor that is not the proxy under test holds it.
pub(crate) fn foreign_lock(key: &str, why: MarkerWhy) -> Marker {
    Marker {
        why: Some(why),
        owner: Some("other".to_owned()),
        ..marker(key, MarkerState::Lock, false)
    }
}

/// A proxy `p1` over a hot tier and a marks bucket, each behind a fault injector and a gate, and a
/// cold tier behind a fault injector.
pub(crate) struct Rig {
    pub(crate) mono: Arc<Mono>,
    pub(crate) hot_mem: Arc<MemoryBackend>,
    pub(crate) cold_mem: Arc<MemoryBackend>,
    pub(crate) marks_mem: Arc<MemoryBackend>,
    pub(crate) hot_faults: Arc<FaultyBackend>,
    pub(crate) marks_faults: Arc<FaultyBackend>,
    pub(crate) cold: Arc<FaultyBackend>,
    pub(crate) hot: Arc<GateBackend>,
    pub(crate) marks: Arc<GateBackend>,
    pub(crate) sleeper: Arc<Ticking>,
    pub(crate) proxy: Arc<Proxy>,
}

impl Rig {
    /// A live proxy whose format says `active-marks` in the bucket `marks`, which is its own.
    pub(crate) async fn new() -> Self {
        Self::build(MARKS_FORMAT, |_| {}).await
    }

    /// A live proxy whose format says `format` (none if empty), with its settings changed by
    /// `tune`. It has a marks bucket named `marks`.
    pub(crate) async fn build(format: &str, tune: impl FnOnce(&mut ProxyConfig)) -> Self {
        let clock = Arc::new(TestClock::new());
        let mono = Arc::new(Mono::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let marks_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        if !format.is_empty() {
            hot_mem.put(FORMAT_KEY, body(format)).await.unwrap();
        }
        let hot_faults = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let marks_faults = Arc::new(FaultyBackend::new(marks_mem.clone()));
        let hot = Arc::new(GateBackend::new(hot_faults.clone()));
        let marks = Arc::new(GateBackend::new(marks_faults.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let sleeper = Arc::new(Ticking {
            mono: mono.clone(),
            marks: marks_mem.clone(),
            asked: Mutex::new(Vec::new()),
            then: Mutex::new(None),
            stop_at: Mutex::new(None),
            reached: Notify::new(),
            go: Notify::new(),
        });
        let mut config = ProxyConfig::new("p1", BUDGET, clock);
        config.mono = mono.clone();
        config.usage_refresh = Duration::ZERO;
        config.format_refresh = Duration::ZERO;
        config.sleeper = sleeper.clone();
        config.marks = Some(marks.clone());
        config.marks_name = Some("marks".to_owned());
        tune(&mut config);
        let proxy = Proxy::open(hot.clone(), cold.clone(), config)
            .await
            .unwrap();
        let rig = Self {
            mono,
            hot_mem,
            cold_mem,
            marks_mem,
            hot_faults,
            marks_faults,
            cold,
            hot,
            marks,
            sleeper,
            proxy: Arc::new(proxy),
        };
        rig.live();
        rig
    }

    /// Moves `live_until` of the proxy to ten minutes from now.
    pub(crate) fn live(&self) {
        let until = crate::Clock::now(&*self.mono) + 600_000;
        self.proxy.inner.heartbeat.deadline.extend_to(until);
    }

    /// Moves the monotonic clock far past `live_until`.
    pub(crate) fn stop_being_live(&self) {
        self.mono.advance(secs(3600));
    }

    /// Puts `key` in both tiers.
    pub(crate) async fn seed_both(&self, key: &str) {
        self.hot_mem.put(key, body("v")).await.unwrap();
        self.cold_mem.put(key, body("v")).await.unwrap();
    }

    /// Stores `marker` under the name of its key, as a writer of another actor would.
    pub(crate) async fn seed_marker(&self, marker: &Marker) {
        self.marks_mem
            .put(&marker.key, encode_marker(marker))
            .await
            .unwrap();
    }

    /// The marker of `key`, read behind the back of the proxy.
    pub(crate) async fn stored(&self, key: &str) -> Option<Marker> {
        let found = self.marks_mem.get(key, None).await.ok()?;
        Some(decode_marker(&found).unwrap())
    }

    pub(crate) async fn shape(&self, key: &str) -> Shape {
        let marker = self.stored(key).await?;
        Some((marker.st, marker.why, marker.gone, marker.owner))
    }

    pub(crate) async fn hot_has(&self, key: &str) -> bool {
        self.hot_mem.head(key).await.is_ok()
    }

    pub(crate) async fn cold_has(&self, key: &str) -> bool {
        self.cold_mem.head(key).await.is_ok()
    }

    /// The calls on the marks bucket on `key`, in order.
    pub(crate) fn marks_log(&self, key: &str) -> Vec<Op> {
        calls_on(&self.marks_faults, key)
    }

    /// The calls on the hot tier on `key`, in order.
    pub(crate) fn hot_log(&self, key: &str) -> Vec<Op> {
        calls_on(&self.hot_faults, key)
    }

    /// Another actor's calls on the markers, which share the marks bucket and nothing else.
    pub(crate) fn leader(&self) -> Markers {
        Markers::new(
            self.marks_mem.clone(),
            "leader",
            MarkersConfig::default(),
            self.mono.clone(),
            Arc::new(crate::RecordingSleeper::new()),
        )
        .unwrap()
    }

    /// The reap of the lock on `key` as the leader makes it once it has judged the owner gone.
    pub(crate) async fn reap(&self, key: &str) {
        let leader = self.leader();
        let seen = leader.read(key).await.unwrap().unwrap();
        let reaped = leader.advance(&seen, MarkMove::Reap).await.unwrap();
        assert!(matches!(reaped, crate::Outcome::Done(_)), "{reaped:?}");
    }

    /// Starts a delete of `key` the way `Proxy::delete` does, on a task of its own, and gives the
    /// task and the receiver of the answer, so that a test can tell the answer from the end of the
    /// release that follows it.
    pub(crate) fn start(
        &self,
        key: &str,
    ) -> (JoinHandle<()>, oneshot::Receiver<Result<(), Error>>) {
        let (answer, asked) = oneshot::channel();
        (tokio::spawn(self.make(key).run(answer)), asked)
    }

    /// A delete of `key`, made and not started.
    pub(super) fn make(&self, key: &str) -> MarkedDelete {
        let inner = Arc::clone(&self.proxy.inner);
        let markers = Arc::clone(inner.markers.as_ref().unwrap());
        MarkedDelete::new(inner, markers, key)
    }

    /// A delete of `key` run to the end of its release.
    pub(crate) async fn delete(&self, key: &str) -> Result<(), Error> {
        let (task, answer) = self.start(key);
        let result = answer.await.unwrap();
        task.await.unwrap();
        result
    }
}

fn calls_on(faults: &FaultyBackend, key: &str) -> Vec<Op> {
    let log = faults.call_log();
    log.into_iter()
        .filter(|(_, on)| on == key)
        .map(|(op, _)| op)
        .collect()
}

/// Lets the other tasks of the runtime run until they have nothing ready: more turns than any task
/// of these tests needs.
pub(crate) async fn let_the_others_run() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// Asserts the requests of a delete of a key both tiers hold: wave 1 reads the marker, which is
/// absent, so the lock is created; the hot tier is asked for the object under the lock; `gone` is
/// written into the lock before the hot DELETE (the marker says `LOCK(DELETE, gone)` and the object
/// is still there when the DELETE arrives); the answer is given; the lock is released to `DIRTY`
/// keeping `gone`. The cold tier is asked nothing and still holds the object.
#[tokio::test]
async fn a_delete_takes_a_lock_heads_decides_gone_deletes_hot_answers_204_and_asks_cold_nothing() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.hot.gate(Op::Delete, "k", crate::Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;

    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, true, "p1")
    );
    assert!(rig.hot_has("k").await);
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert!(gate.release());
    assert_eq!(answer.await.unwrap(), Ok(()));
    task.await.unwrap();

    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.hot_log("k"), [Op::Head, Op::Delete]);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
    assert_eq!(
        rig.marks_log("k"),
        [Op::Get, Op::Put, Op::PutIfMatch, Op::PutIfMatch]
    );
    assert!(rig.cold_has("k").await);
    assert_eq!(rig.cold.call_log(), []);
}

/// Asserts that a delete of a key only the cold tier holds writes `gone` only after its HEAD of
/// the hot tier under the lock: while the HEAD is parked the lock says `gone = false`. It makes no
/// hot DELETE, answers, leaves `DIRTY(gone)`, and asks cold nothing.
#[tokio::test]
async fn a_delete_of_a_cold_only_key_decides_gone_after_the_head_under_the_lock() {
    let rig = Rig::new().await;
    rig.cold_mem.put("k", body("v")).await.unwrap();
    let gate = rig.hot.gate(Op::Head, "k", crate::Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    assert_eq!(
        rig.shape("k").await,
        lock_shape(MarkerWhy::Delete, false, "p1")
    );
    assert!(gate.release());
    assert_eq!(answer.await.unwrap(), Ok(()));
    task.await.unwrap();

    assert_eq!(rig.shape("k").await, dirty_shape(true));
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert!(rig.cold_has("k").await);
    assert_eq!(rig.cold.call_log(), []);
}

/// Asserts that a delete of a key that has a `DIRTY` or a `WANT` marker locks it by a change on the
/// tag from the wave-1 read, without a create that would fail: one read, three changes (lock,
/// `gone`, release) and no create. The `gone` it found is kept by the lock and by the release.
#[tokio::test]
async fn a_delete_of_a_key_with_a_dirty_marker_locks_it_by_cas() {
    for (st, found_gone) in [
        (MarkerState::Dirty, false),
        (MarkerState::Dirty, true),
        (MarkerState::Want, false),
    ] {
        let rig = Rig::new().await;
        rig.seed_both("k").await;
        rig.seed_marker(&marker("k", st, found_gone)).await;
        let gate = rig.hot.gate(Op::Head, "k", crate::Phase::Pre);
        let (task, answer) = rig.start("k");
        gate.parked().await;
        assert_eq!(
            rig.shape("k").await,
            lock_shape(MarkerWhy::Delete, found_gone, "p1")
        );
        assert!(gate.release());
        assert_eq!(answer.await.unwrap(), Ok(()));
        task.await.unwrap();

        assert_eq!(
            rig.marks_log("k"),
            [Op::Get, Op::PutIfMatch, Op::PutIfMatch, Op::PutIfMatch]
        );
        assert_eq!(rig.shape("k").await, dirty_shape(true));
        assert!(!rig.hot_has("k").await);
    }
}

/// Asserts that a delete of a key that exists nowhere is answered, as S3 does, and still writes
/// the marker: a lock created, `gone` written, a release. It makes no hot DELETE.
#[tokio::test]
async fn a_delete_of_an_absent_key_is_204_and_leaves_a_gone_marker() {
    let rig = Rig::new().await;
    assert_eq!(rig.delete("k").await, Ok(()));
    assert_eq!(rig.shape("k").await, dirty_shape(true));
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert_eq!(
        rig.marks_log("k"),
        [Op::Get, Op::Put, Op::PutIfMatch, Op::PutIfMatch]
    );
    assert_eq!(rig.cold.call_log(), []);
}

/// Asserts that a put that lands its object between the HEAD of a delete of a cold-only key and
/// its `gone` write is not deleted: the HEAD found nothing, so the delete makes no hot DELETE, and
/// the object that landed stays visible in hot; the delete is ordered before that put.
#[tokio::test]
async fn a_delete_of_a_cold_only_key_does_not_delete_an_object_that_landed_after_its_head() {
    let rig = Rig::new().await;
    rig.cold_mem.put("k", body("old")).await.unwrap();
    let gate = rig.marks.gate(Op::PutIfMatch, "k", crate::Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    rig.hot_mem.put("k", body("new")).await.unwrap();
    assert!(gate.release());
    assert_eq!(answer.await.unwrap(), Ok(()));
    task.await.unwrap();

    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), body("new"));
    assert_eq!(rig.hot_log("k"), [Op::Head]);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a delete that finds a lock waits with the polls of the config (20 ms, then 40 ms)
/// and takes the key once the lock has ended: the marker is read once for wave 1 and once for each
/// poll, and the delete then locks the `DIRTY` marker that replaced the lock.
#[tokio::test]
async fn a_delete_that_finds_a_lock_waits_with_the_polls_of_the_config_and_then_takes_the_key() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.seed_marker(&foreign_lock("k", MarkerWhy::Overwrite))
        .await;
    rig.sleeper
        .then_replace(2, marker("k", MarkerState::Dirty, false));
    assert_eq!(rig.delete("k").await, Ok(()));

    let asked = rig.sleeper.asked();
    assert_eq!(
        asked,
        [Duration::from_millis(20), Duration::from_millis(40)]
    );
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a delete whose wait for a lock reaches `lock_wait` with the lock still there is
/// `SlowDown`, touches neither the hot tier nor the lock, and the pauses it made add up to the
/// 5 seconds of `lock_wait`.
#[tokio::test]
async fn a_delete_whose_lock_wait_ends_is_slowdown_and_changes_nothing() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let theirs = foreign_lock("k", MarkerWhy::Overwrite);
    rig.seed_marker(&theirs).await;
    let answer = rig.delete("k").await;

    assert!(matches!(answer, Err(Error::SlowDown(_))), "{answer:?}");
    assert_eq!(rig.sleeper.asked().iter().sum::<Duration>(), secs(5));
    assert_eq!(rig.stored("k").await, Some(theirs));
    assert_eq!(rig.hot_log("k"), []);
    assert!(rig.hot_has("k").await);
}

/// Asserts that a delete waits once only: a lock that comes back after the wait ended, while the
/// delete's change of the marker was losing a race, is `SlowDown` and not a second wait.
#[tokio::test]
async fn a_delete_that_waited_and_finds_the_key_locked_again_is_slowdown() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.seed_marker(&foreign_lock("k", MarkerWhy::Overwrite))
        .await;
    rig.sleeper
        .then_replace(1, marker("k", MarkerState::Dirty, false));
    let gate = rig.marks.gate(Op::PutIfMatch, "k", crate::Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    let again = foreign_lock("k", MarkerWhy::Evict);
    rig.marks_mem.delete("k").await.unwrap();
    rig.seed_marker(&again).await;
    assert!(gate.release());

    let answer = answer.await.unwrap();
    assert!(matches!(answer, Err(Error::SlowDown(_))), "{answer:?}");
    task.await.unwrap();
    assert_eq!(rig.stored("k").await, Some(again));
    assert_eq!(rig.sleeper.asked().len(), 1);
    assert!(rig.hot_has("k").await);
}

/// Asserts that a lock create that finds a marker which appeared since the wave-1 read (the store
/// said it exists) reads the marker again and locks what it finds by a change on its tag.
#[tokio::test]
async fn a_lock_create_that_meets_a_marker_that_appeared_reads_again_and_locks_it_by_cas() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    let gate = rig.marks.gate(Op::Put, "k", crate::Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, false))
        .await;
    assert!(gate.release());
    assert_eq!(answer.await.unwrap(), Ok(()));
    task.await.unwrap();

    assert_eq!(
        rig.marks_log("k"),
        [
            Op::Get,
            Op::Put,
            Op::Get,
            Op::PutIfMatch,
            Op::PutIfMatch,
            Op::PutIfMatch
        ]
    );
    assert!(!rig.hot_has("k").await);
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a lock change that loses a race (the marker was changed after the delete read it)
/// reads the marker again and tries again on the new tag.
#[tokio::test]
async fn a_lock_change_that_loses_a_race_reads_again_and_locks_the_marker_it_finds() {
    let rig = Rig::new().await;
    rig.seed_both("k").await;
    rig.seed_marker(&marker("k", MarkerState::Dirty, false))
        .await;
    let gate = rig.marks.gate(Op::PutIfMatch, "k", crate::Phase::Pre);
    let (task, answer) = rig.start("k");
    gate.parked().await;
    rig.marks_mem.delete("k").await.unwrap();
    rig.seed_marker(&marker("k", MarkerState::Want, false))
        .await;
    assert!(gate.release());
    assert_eq!(answer.await.unwrap(), Ok(()));
    task.await.unwrap();

    assert_eq!(
        rig.marks_log("k"),
        [
            Op::Get,
            Op::PutIfMatch,
            Op::Get,
            Op::PutIfMatch,
            Op::PutIfMatch,
            Op::PutIfMatch
        ]
    );
    assert_eq!(rig.shape("k").await, dirty_shape(true));
}

/// Asserts that a delete whose creates and changes keep losing is `Unavailable` after
/// `entry_rounds` tries, holds no lock, and has touched the hot tier not at all.
#[tokio::test]
async fn a_delete_whose_marker_keeps_changing_is_unavailable_after_entry_rounds() {
    let rig = Rig::build(MARKS_FORMAT, |config| config.entry_rounds = 2).await;
    rig.seed_both("k").await;
    for _ in 0..2 {
        let exists = Error::AlreadyExists("k".to_owned());
        rig.marks_faults.fail_next_with(Op::Put, exists);
    }
    let answer = rig.delete("k").await;

    assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    assert_eq!(
        rig.marks_log("k"),
        [Op::Get, Op::Put, Op::Get, Op::Put, Op::Get]
    );
    assert_eq!(rig.shape("k").await, None);
    assert_eq!(rig.hot_log("k"), []);
}

/// Asserts that a delete in the format `active` is made on the entry as before and never touches
/// the marks bucket, and that a proxy with a marks bucket is no different from one without.
#[tokio::test]
async fn a_delete_under_the_entry_format_never_touches_the_marks_bucket() {
    let rig = Rig::build(r#"{"epoch":2,"phase":"active"}"#, |_| {}).await;
    rig.seed_both("k").await;
    assert_eq!(rig.proxy.delete("k").await, Ok(()));

    assert!(!rig.hot_has("k").await);
    assert!(!rig.cold_has("k").await);
    assert_eq!(rig.marks_faults.call_log(), []);
}

/// Asserts that a delete is `Unavailable` and touches nothing, in either store, while the format
/// pauses writes, says `active-marks` without the epoch of the markers or without the name of the
/// bucket, is an `active-marks` of a proxy that has no marks bucket or another one, says a phase
/// this release does not know, or is not there.
#[tokio::test]
async fn a_delete_is_unavailable_while_the_format_does_not_open_writes_on_markers() {
    let named = r#"{"epoch":3,"marks_bucket":"other","phase":"active-marks"}"#;
    let cases = [
        (r#"{"phase":"drain"}"#, "paused"),
        (r#"{"phase":"drained"}"#, "paused"),
        (
            r#"{"epoch":2,"marks_bucket":"marks","phase":"active-marks"}"#,
            "wait until",
        ),
        (r#"{"epoch":3,"phase":"active-marks"}"#, "wait until"),
        (r#"{"epoch":4,"phase":"someday"}"#, "wait until"),
        ("", "wait until"),
        (named, "not this proxy's marks bucket"),
    ];
    for (format, said) in cases {
        let rig = Rig::build(format, |_| {}).await;
        rig.seed_both("k").await;
        let answer = rig.proxy.delete("k").await;

        let Err(Error::Unavailable(text)) = answer else {
            panic!("{format}: {answer:?}");
        };
        assert!(text.contains(said), "{format}: {text}");
        assert_eq!(rig.marks_faults.call_log(), [], "{format}");
        assert_eq!(rig.hot_log("k"), [], "{format}");
        assert!(
            rig.hot_has("k").await && rig.cold_has("k").await,
            "{format}"
        );
    }
}

/// Asserts that a proxy with no marks bucket (a `dir` hot tier), and one whose own bucket has no
/// name, refuse a delete under `active-marks` as a proxy of another bucket does, and touch nothing.
#[tokio::test]
async fn a_proxy_without_a_marks_bucket_or_with_another_refuses_a_delete_under_active_marks() {
    let tunes: [fn(&mut ProxyConfig); 3] = [
        |config| config.marks = None,
        |config| config.marks_name = None,
        |config| config.marks_name = Some("elsewhere".to_owned()),
    ];
    for tune in tunes {
        let rig = Rig::build(MARKS_FORMAT, tune).await;
        rig.seed_both("k").await;
        let answer = rig.proxy.delete("k").await;
        assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
        assert_eq!(rig.marks_faults.call_log(), []);
        assert!(rig.hot_has("k").await);
    }
}

/// Asserts that a key under a reserved prefix is refused as it is under the entry format, and that
/// the settings of the markers are checked when the proxy is opened: a lease that is below two
/// refreshes and the margin keeps the proxy from opening.
#[tokio::test]
async fn a_reserved_key_is_refused_and_unusable_marker_settings_keep_a_proxy_from_opening() {
    let rig = Rig::new().await;
    let answer = rig.proxy.delete("_tiering/format").await;
    assert!(matches!(answer, Err(Error::Reserved(_))), "{answer:?}");
    assert_eq!(rig.marks_faults.call_log(), []);

    let clock = Arc::new(TestClock::new());
    let mut config = ProxyConfig::new("p2", BUDGET, clock.clone());
    config.marks = Some(Arc::new(MemoryBackend::new()));
    config.marks_put.markers.lock_lease = secs(1);
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let opened = Proxy::open(hot, Arc::new(MemoryBackend::with_clock(clock)), config).await;
    assert!(matches!(opened, Err(Error::Other(_))));
}
