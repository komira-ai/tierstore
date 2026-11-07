//! The plain puts of a proxy whose format is `active-marks`: the rig the tests of the overwrite and
//! of the landing share, and what admits a put.
//!
//! No test waits: the proxy's sleeper is the [`Pacer`], which writes the pauses down, moves the
//! monotonic clock by them when the test says so, and holds back the pauses a test names.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tokio::sync::watch;

use super::overwrite::{PlainPut, PutBody, PutClaim};
use crate::Backend;
use crate::journal::{FORMAT_KEY, proxy_key};
use crate::{
    Error, FaultyBackend, Gate, GateBackend, MARKER_VERSION, Marker, MarkerState, MarkerWhy,
    MemoryBackend, Mono, Op, PartBody, Proxy, ProxyConfig, Sleeper, TestClock, decode_marker,
    encode_marker,
};

/// The format object of a store whose writes are made on markers.
pub(super) const MARKS_FORMAT: &str =
    r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;

pub(super) fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// A body of the chunks `parts`.
pub(super) fn chunks(parts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts.iter().map(|part| Ok(body(part))).collect();
    stream::iter(items).boxed()
}

/// A held body.
pub(super) fn held(text: &str) -> PutBody<'static> {
    PutBody::Held(body(text))
}

/// A stream of the chunks `parts`, whose length is known.
pub(super) fn streamed(parts: &[&'static str]) -> PutBody<'static> {
    let len = parts.iter().map(|part| part.len() as u64).sum();
    PutBody::Stream {
        len: Some(len),
        body: chunks(parts),
    }
}

/// The sleeper of the proxy in a test.
pub(super) struct Pacer {
    mono: Arc<Mono>,
    state: Mutex<PacerState>,
    reached: watch::Sender<bool>,
    opened: watch::Sender<bool>,
    count: watch::Sender<usize>,
}

#[derive(Default)]
struct PacerState {
    asked: Vec<Duration>,
    /// Whether a pause moves the monotonic clock on by its length.
    advance: bool,
    /// The number of pauses after which every pause is held for good.
    stop_after: Option<usize>,
    /// The length of the pauses that are held until [`Pacer::open`].
    hold: Option<Duration>,
}

impl Pacer {
    fn new(mono: Arc<Mono>) -> Self {
        Self {
            mono,
            state: Mutex::default(),
            reached: watch::channel(false).0,
            opened: watch::channel(false).0,
            count: watch::channel(0).0,
        }
    }

    fn state(&self) -> MutexGuard<'_, PacerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes every pause move the monotonic clock by its length, as a real wait would.
    pub(super) fn tick(&self) {
        self.state().advance = true;
    }

    /// Holds every pause after the `count`th for good, and lets [`Pacer::reached`] return.
    pub(super) fn stop_after(&self, count: usize) {
        self.state().stop_after = Some(count);
    }

    /// Holds the pauses of exactly `length` until [`Pacer::open`].
    pub(super) fn hold(&self, length: Duration) {
        self.state().hold = Some(length);
    }

    pub(super) fn open(&self) {
        self.opened.send_replace(true);
    }

    /// Returns once the pauses are held for good (see [`Pacer::stop_after`]).
    pub(super) async fn reached(&self) {
        self.reached
            .subscribe()
            .wait_for(|reached| *reached)
            .await
            .ok();
    }

    /// Returns once `count` pauses have been asked for.
    pub(super) async fn slept(&self, count: usize) {
        self.count
            .subscribe()
            .wait_for(|slept| *slept >= count)
            .await
            .ok();
    }

    /// Returns once a pause of exactly `length` has been asked for.
    pub(super) async fn slept_for(&self, length: Duration) {
        let mut changes = self.count.subscribe();
        while !self.state().asked.contains(&length) {
            changes.changed().await.ok();
        }
    }

    /// The length of every pause so far.
    pub(super) fn asked(&self) -> Vec<Duration> {
        self.state().asked.clone()
    }
}

#[async_trait]
impl Sleeper for Pacer {
    async fn sleep(&self, duration: Duration) {
        let (stop, hold) = {
            let mut state = self.state();
            state.asked.push(duration);
            self.count.send_replace(state.asked.len());
            if state.advance {
                self.mono.advance(duration);
            }
            let stop = state
                .stop_after
                .is_some_and(|after| state.asked.len() > after);
            (stop, state.hold == Some(duration))
        };
        if stop {
            self.reached.send_replace(true);
            std::future::pending::<()>().await;
        }
        if hold {
            self.opened
                .subscribe()
                .wait_for(|opened| *opened)
                .await
                .ok();
        }
        tokio::task::yield_now().await;
    }
}

/// A wait on the timer of the runtime that then moves the monotonic clock by the time waited: for
/// a test on a paused clock whose stores delay their calls on the same timer, so that a waiting
/// proxy lets the runtime go idle and the virtual time pass.
struct Timer {
    mono: Arc<Mono>,
}

#[async_trait]
impl Sleeper for Timer {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
        self.mono.advance(duration);
    }
}

/// A proxy `p1` over a hot tier, a marks bucket and a cold tier, each behind a gate and a fault
/// injector, whose format is `active-marks`.
pub(super) struct Rig {
    pub(super) mono: Arc<Mono>,
    pub(super) hot_mem: Arc<MemoryBackend>,
    pub(super) cold_mem: Arc<MemoryBackend>,
    pub(super) marks_mem: Arc<MemoryBackend>,
    pub(super) hot_faults: Arc<FaultyBackend>,
    pub(super) hot: Arc<GateBackend>,
    pub(super) marks_faults: Arc<FaultyBackend>,
    pub(super) marks: Arc<GateBackend>,
    pub(super) cold_faults: Arc<FaultyBackend>,
    pub(super) cold: Arc<GateBackend>,
    pub(super) pacer: Arc<Pacer>,
    sleeper: Arc<dyn Sleeper>,
    pub(super) proxy: Arc<Proxy>,
}

impl Rig {
    pub(super) async fn new() -> Self {
        Self::with(MARKS_FORMAT, |_| {}).await
    }

    /// A rig whose format object says `format` (none if empty), with `tune` applied to the
    /// proxy's settings.
    pub(super) async fn with(format: &str, tune: impl FnOnce(&mut ProxyConfig)) -> Self {
        Self::over(format, false, tune, |hot| hot).await
    }

    /// A rig whose proxies wait on the timer of the runtime (see [`Timer`]) and not on the
    /// [`Pacer`].
    pub(super) async fn timed() -> Self {
        Self::over(MARKS_FORMAT, true, |_| {}, |hot| hot).await
    }

    /// As [`Rig::with`], for a proxy whose hot tier is the rig's gate seen through `wrap`.
    pub(super) async fn over(
        format: &str,
        timed: bool,
        tune: impl FnOnce(&mut ProxyConfig),
        wrap: impl FnOnce(Arc<GateBackend>) -> Arc<dyn Backend>,
    ) -> Self {
        let clock = Arc::new(TestClock::new());
        let mono = Arc::new(Mono::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let marks_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        if !format.is_empty() {
            hot_mem.put(FORMAT_KEY, body(format)).await.unwrap();
        }
        let hot_faults = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let hot = Arc::new(GateBackend::new(hot_faults.clone()));
        let marks_faults = Arc::new(FaultyBackend::new(marks_mem.clone()));
        let marks = Arc::new(GateBackend::new(marks_faults.clone()));
        let cold_faults = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let cold = Arc::new(GateBackend::new(cold_faults.clone()));
        let pacer = Arc::new(Pacer::new(mono.clone()));
        let mut config = ProxyConfig::new("p1", 1000, clock);
        config.mono = mono.clone();
        config.usage_refresh = Duration::ZERO;
        config.format_refresh = Duration::ZERO;
        let sleeper: Arc<dyn Sleeper> = if timed {
            Arc::new(Timer { mono: mono.clone() })
        } else {
            pacer.clone()
        };
        config.sleeper = sleeper.clone();
        config.marks = Some(marks.clone());
        config.marks_name = Some("hot-marks".to_owned());
        tune(&mut config);
        let proxy = Proxy::open(wrap(hot.clone()), cold.clone(), config)
            .await
            .unwrap();
        proxy
            .inner
            .heartbeat
            .deadline
            .extend_to(crate::Clock::now(&*mono) + 3_600_000);
        Self {
            mono,
            hot_mem,
            cold_mem,
            marks_mem,
            hot_faults,
            hot,
            marks_faults,
            marks,
            cold_faults,
            cold,
            pacer,
            sleeper,
            proxy: Arc::new(proxy),
        }
    }

    /// A second live proxy `id` over the same three stores, with the waits of the first.
    pub(super) async fn another_proxy(&self, id: &str) -> Arc<Proxy> {
        let mut config = ProxyConfig::new(id, 1000, Arc::new(TestClock::new()));
        config.mono = self.mono.clone();
        config.usage_refresh = Duration::ZERO;
        config.format_refresh = Duration::ZERO;
        config.sleeper = self.sleeper.clone();
        config.marks = Some(self.marks.clone());
        config.marks_name = Some("hot-marks".to_owned());
        let proxy = Proxy::open(self.hot.clone(), self.cold.clone(), config)
            .await
            .unwrap();
        proxy
            .inner
            .heartbeat
            .deadline
            .extend_to(crate::Clock::now(&*self.mono) + 3_600_000);
        Arc::new(proxy)
    }

    /// A plain put of the held `text` with no claim.
    pub(super) async fn put(&self, key: &str, text: &str) -> Result<PlainPut, Error> {
        self.proxy
            .put_plain(key, held(text), &PutClaim::default())
            .await
    }

    /// A plain put of `text` as a stream of one chunk (`stream`) or held whole.
    pub(super) async fn put_kind(
        &self,
        key: &str,
        text: &'static str,
        stream: bool,
    ) -> Result<PlainPut, Error> {
        let body = if stream {
            streamed(&[text])
        } else {
            held(text)
        };
        self.proxy.put_plain(key, body, &PutClaim::default()).await
    }

    pub(super) async fn seed_hot(&self, key: &str, text: &str) {
        self.hot_mem.put(key, body(text)).await.unwrap();
    }

    pub(super) async fn seed_cold(&self, key: &str, text: &str) {
        self.cold_mem.put(key, body(text)).await.unwrap();
    }

    pub(super) async fn hot_text(&self, key: &str) -> Option<String> {
        let got = self.hot_mem.get(key, None).await.ok()?;
        Some(String::from_utf8(got.to_vec()).unwrap())
    }

    pub(super) async fn cold_text(&self, key: &str) -> Option<String> {
        let got = self.cold_mem.get(key, None).await.ok()?;
        Some(String::from_utf8(got.to_vec()).unwrap())
    }

    /// The content tag of the hot object.
    pub(super) async fn hot_tag(&self, key: &str) -> String {
        self.hot_mem.head(key).await.unwrap().tag
    }

    pub(super) async fn seed_marker(&self, marker: &Marker) {
        self.marks_mem
            .put(&marker.key, encode_marker(marker))
            .await
            .unwrap();
    }

    /// The marker of `key` as stored.
    pub(super) async fn marker(&self, key: &str) -> Option<Marker> {
        let got = self.marks_mem.get(key, None).await.ok()?;
        Some(decode_marker(&got).unwrap())
    }

    /// The state and the fields a test asserts of the marker of `key`: state, `gone`, `dirty`,
    /// reason and owner.
    pub(super) async fn shape(&self, key: &str) -> Option<Shape> {
        let marker = self.marker(key).await?;
        Some((
            marker.st,
            marker.gone,
            marker.dirty,
            marker.why,
            marker.owner,
        ))
    }

    /// The calls to the hot tier that are about data keys (not the proxy's own), in order.
    pub(super) fn hot_log(&self) -> Vec<(Op, String)> {
        data(self.hot_faults.call_log())
    }

    /// The calls to the marks bucket, in order.
    pub(super) fn marks_log(&self) -> Vec<(Op, String)> {
        self.marks_faults.call_log()
    }
}

/// What a test asserts of a marker.
pub(super) type Shape = (MarkerState, bool, bool, Option<MarkerWhy>, Option<String>);

/// The calls of `log` on keys that are not the proxy's own.
fn data(log: Vec<(Op, String)>) -> Vec<(Op, String)> {
    log.into_iter()
        .filter(|(_, key)| !key.starts_with('_'))
        .collect()
}

/// A marker of `key` with the given state, as someone else wrote it.
pub(super) fn marker(key: &str, st: MarkerState, gone: bool, owner: Option<&str>) -> Marker {
    let lock = st == MarkerState::Lock;
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone,
        dirty: true,
        why: lock.then_some(MarkerWhy::Overwrite),
        owner: owner.map(str::to_owned),
        nonce: "seed".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// Polls `call` until the call parked by `gate` is held; `call` must not end first.
pub(super) async fn until_parked<F: std::future::Future + Unpin>(gate: &Gate, call: &mut F) {
    futures::future::select(Box::pin(gate.parked()), call).await;
}

/// Lets the tasks that are ready run: a release that a put has handed to a task of its own.
pub(super) async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// Asserts that an error is the one that tells a client to retry.
pub(super) fn is_busy(error: &Error) -> bool {
    matches!(error, Error::Unavailable(_))
}

/// Asserts, for each format, that a plain put is refused with [`Error::Unavailable`] and that the
/// proxy asked for the format object and nothing else of a store: a proxy that reads any format but
/// `active-marks` makes no write on markers, and a store that cannot be read is read as one.
#[tokio::test]
async fn a_put_under_any_format_but_active_marks_is_unavailable_and_asks_nothing() {
    let formats = [
        "",
        r#"{"epoch":2,"phase":"active"}"#,
        r#"{"phase":"drain"}"#,
        r#"{"phase":"drained"}"#,
        r#"{"phase":"from-the-future"}"#,
        "not json",
    ];
    for format in formats {
        let rig = Rig::with(format, |_| {}).await;
        rig.seed_hot("k", "old").await;
        let put = rig.put("k", "new").await;
        assert!(is_busy(&put.unwrap_err()), "format {format:?}");
        assert!(rig.hot_log().is_empty(), "format {format:?}");
        assert!(rig.marks_log().is_empty(), "format {format:?}");
        assert!(rig.cold_faults.call_log().is_empty());
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    }
}

/// Asserts that a proxy with no marks bucket is refused a plain put (a server over a `dir` hot tier
/// has none until a later release), that a key under a reserved prefix is refused as ever, and that
/// a proxy whose registration is not live writes nothing.
#[tokio::test]
async fn a_put_is_refused_without_a_marks_bucket_for_a_reserved_key_and_when_not_live() {
    let rig = Rig::with(MARKS_FORMAT, |config| config.marks = None).await;
    rig.seed_hot("k", "old").await;
    assert!(is_busy(&rig.put("k", "new").await.unwrap_err()));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));

    let rig = Rig::new().await;
    let reserved = rig.put("_tiering/format", "x").await;
    assert!(matches!(reserved, Err(Error::Reserved(_))));

    rig.seed_hot("k", "old").await;
    rig.mono.advance(Duration::from_secs(7200));
    assert!(is_busy(&rig.put("k", "new").await.unwrap_err()));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert!(rig.marks_log().is_empty());
}

/// Asserts that a proxy configured with a marks bucket other than the one the format record names
/// (or a record that names none, or no epoch, or an epoch below 3), and a proxy that has no name for
/// its marks bucket whatever the record, refuse a plain put and write to no store, and that the
/// same name as the record's is admitted.
#[tokio::test]
async fn a_proxy_with_another_marks_bucket_than_the_format_record_refuses_writes() {
    let no_bucket = r#"{"epoch":3,"phase":"active-marks"}"#;
    let no_epoch = r#"{"marks_bucket":"hot-marks","phase":"active-marks"}"#;
    let early = r#"{"epoch":2,"marks_bucket":"hot-marks","phase":"active-marks"}"#;
    let other = Some("other-marks");
    let cases = [
        (MARKS_FORMAT, other),
        (no_bucket, other),
        (no_epoch, Some("hot-marks")),
        (early, Some("hot-marks")),
        (MARKS_FORMAT, None),
        (no_bucket, None),
    ];
    for (record, name) in cases {
        let rig = Rig::with(record, |config| {
            config.marks_name = name.map(str::to_owned);
        })
        .await;
        rig.seed_hot("k", "old").await;
        let seeded = rig.hot_log().len();
        assert!(is_busy(&rig.put("k", "new").await.unwrap_err()), "{record}");
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
        assert!(rig.marks_log().is_empty(), "{record}");
        assert!(rig.cold_faults.call_log().is_empty(), "{record}");
        let after: Vec<_> = rig.hot_log().split_off(seeded);
        assert!(
            after
                .iter()
                .all(|(op, key)| *op == Op::Get && key == FORMAT_KEY),
            "{record}: {after:?}"
        );
    }
    let rig = Rig::with(MARKS_FORMAT, |config| {
        config.marks_name = Some("hot-marks".to_owned());
    })
    .await;
    rig.seed_hot("k", "old").await;
    rig.put("k", "new").await.unwrap();
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
}

/// Asserts that a plain put of a key the hot tier does not hold is the fast path of 5.1: the
/// object and a `DIRTY` marker are written, and the cold tier is not asked (I15).
#[tokio::test]
async fn a_plain_put_of_a_key_the_hot_tier_does_not_hold_lands_the_object_and_a_dirty_marker() {
    for stream in [false, true] {
        let rig = Rig::new().await;
        let put = rig.put_kind("new", "v", stream).await.unwrap();
        assert!(put.wrote);
        assert_eq!(put.tag, Some(rig.hot_tag("new").await));
        assert_eq!(rig.hot_text("new").await.as_deref(), Some("v"));
        let marker = rig.marker("new").await.unwrap();
        assert_eq!(marker.st, MarkerState::Dirty);
        assert!(!marker.gone);
        assert!(rig.cold_faults.call_log().is_empty());
    }
}

/// Asserts that a proxy whose marker settings do not hold together does not open, and leaves no
/// registration in the hot tier (the leader would count it as live until it departed).
#[tokio::test]
async fn a_proxy_whose_marker_settings_do_not_hold_together_does_not_open() {
    let hot = Arc::new(MemoryBackend::new());
    let cold = Arc::new(MemoryBackend::new());
    let mut config = ProxyConfig::new("p1", 1000, Arc::new(TestClock::new()));
    config.marks = Some(Arc::new(MemoryBackend::new()));
    config.marks_put.markers.lock_refresh = Duration::ZERO;
    let opened = Proxy::open(hot.clone(), cold, config).await;
    let error = opened.err().unwrap();
    assert_eq!(
        error,
        Error::Other("invalid marker settings: lock_refresh must be more than zero".to_owned())
    );
    let registration = hot.head(&proxy_key("p1")).await;
    assert_eq!(registration, Err(Error::NotFound(proxy_key("p1"))));
}

/// Asserts that every request of a put is refused while the marks bucket cannot be read, and that
/// the object is not changed: the markers fail closed.
#[tokio::test]
async fn an_overwrite_fails_closed_while_the_marks_bucket_is_down() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.marks_faults.set_down(true);
    let put = rig.put("k", "new").await;
    assert!(is_busy(&put.unwrap_err()));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert!(rig.cold_faults.call_log().is_empty());
}

/// Asserts that an overwrite does not touch the cold tier: with it down, a plain put of a key the
/// hot tier holds is made all the same (I15), and the cold tier is not asked.
#[tokio::test]
async fn a_plain_put_succeeds_while_the_cold_tier_is_down() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.cold_faults.set_down(true);
    let put = rig.put("k", "new").await.unwrap();
    assert!(put.wrote);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("new"));
    assert!(rig.cold_faults.call_log().is_empty());
}

/// Asserts that a put whose first look at the hot tier fails is told to retry and writes nothing.
#[tokio::test]
async fn a_put_whose_hot_head_fails_is_unavailable_and_writes_nothing() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    rig.hot_faults.fail_next(Op::Head, 1);
    assert!(is_busy(&rig.put("k", "new").await.unwrap_err()));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("old"));
    assert_eq!(rig.marker("k").await, None);
}
