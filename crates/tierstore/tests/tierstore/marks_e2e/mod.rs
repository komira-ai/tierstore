//! The whole protocol of `docs/design/put-without-lock.md` end to end: a `Proxy` over memory tiers
//! whose format object says `active-marks`, the leader's pass over the markers, the eviction and
//! the copy-back, and the local cache in front of a proxy. Every store is in memory behind a fault
//! injector that counts the requests, the clocks are moved by hand and no test waits for time.
//!
//! The tests are in the modules below, one for each part of the protocol; this module is the rig.

mod cache;
mod crash;
mod deletes;
mod leader;
mod puts;
mod races;
mod reads;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{
    Backend, EntryReconciler, EntryReconcilerConfig, Error, FaultyBackend, Marker, MarkerState,
    Markers, MarkersConfig, MemoryBackend, Mono, Op, Proxy, ProxyConfig, ReconcilerConfig, Sleeper,
    TestClock, decode_marker,
};

/// The name of the marks bucket: the proxies are configured with it and the format object records it.
pub const BUCKET: &str = "hot-marks";

/// The format object of a store that runs on markers.
pub const FORMAT: &str = r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;

/// The key of the format object.
const FORMAT_KEY: &str = "_tiering/format";

/// Which store of the hot tier a request went to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    /// The hot bucket, where the objects are.
    Hot,
    /// The marks bucket, where the markers are.
    Marks,
    /// The cold tier.
    Cold,
}

/// How many requests of each tier a test has looked at already.
#[derive(Default)]
pub struct Cursor {
    hot: usize,
    marks: usize,
    cold: usize,
}

/// A request a store received: where, what, and for which key.
pub type Request = (Store, Op, String);

/// A wait that moves the monotonic clock by the time it was asked for and gives the other tasks a
/// turn: a lock wait or a retry then ends by the clock, as a real one ends by time, and no test
/// sleeps.
pub struct Ticking {
    mono: Arc<Mono>,
}

#[async_trait]
impl Sleeper for Ticking {
    async fn sleep(&self, duration: Duration) {
        self.mono.advance(duration);
        tokio::task::yield_now().await;
    }
}

/// A wait on the timer of the runtime that then moves the monotonic clock by the time waited: for a
/// test that runs on a paused clock, whose stores are slowed by delays on the same timer, so that a
/// waiting proxy lets the runtime go idle and the virtual time pass.
pub struct Sleeping {
    mono: Arc<Mono>,
}

#[async_trait]
impl Sleeper for Sleeping {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
        self.mono.advance(duration);
    }
}

/// A wait that writes down that it was asked for and gives the other tasks a turn, without moving
/// any clock: for a race in which a test lets the waited-for party go on by what it sees, and not
/// by time.
#[derive(Default)]
pub struct Counting {
    asked: std::sync::atomic::AtomicUsize,
}

impl Counting {
    /// How many waits were asked for.
    pub fn count(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Sleeper for Counting {
    async fn sleep(&self, _duration: Duration) {
        let _ = self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::task::yield_now().await;
    }
}

/// A wait that never ends: the heartbeat of a proxy then makes no call after its first beat.
struct Never;

#[async_trait]
impl Sleeper for Never {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// The three tiers of a store, in memory, each behind a fault injector that counts its requests,
/// with the wall clock they stamp their objects with and the monotonic clock of the proxies and
/// the leader. The memory backends are kept so that a test can read or change a tier without
/// going through the store.
pub struct World {
    pub clock: Arc<TestClock>,
    pub mono: Arc<Mono>,
    pub hot_mem: Arc<MemoryBackend>,
    pub marks_mem: Arc<MemoryBackend>,
    pub cold_mem: Arc<MemoryBackend>,
    pub hot: Arc<FaultyBackend>,
    pub marks: Arc<FaultyBackend>,
    pub cold: Arc<FaultyBackend>,
}

impl World {
    /// Empty tiers whose format object says `active-marks`.
    pub async fn new() -> Self {
        let world = Self::bare();
        world.set_format(FORMAT).await;
        world
    }

    /// Empty tiers with no format object.
    pub fn bare() -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        let tier = || Arc::new(MemoryBackend::with_clock(clock.clone()));
        let (hot_mem, marks_mem, cold_mem) = (tier(), tier(), tier());
        Self {
            clock,
            mono: Arc::new(Mono::new()),
            hot: Arc::new(FaultyBackend::new(hot_mem.clone())),
            marks: Arc::new(FaultyBackend::new(marks_mem.clone())),
            cold: Arc::new(FaultyBackend::new(cold_mem.clone())),
            hot_mem,
            marks_mem,
            cold_mem,
        }
    }

    /// Replaces the format object.
    pub async fn set_format(&self, format: &str) {
        self.hot_mem.delete(FORMAT_KEY).await.ok();
        let body = Bytes::copy_from_slice(format.as_bytes());
        self.hot_mem.put(FORMAT_KEY, body).await.unwrap();
    }

    /// The settings of a proxy `id` over the marks bucket, whose format object is read once and
    /// whose waits move the monotonic clock of the world.
    pub fn config(&self, id: &str) -> ProxyConfig {
        let mut config = ProxyConfig::new(id, 1 << 30, self.clock.clone());
        config.mono = self.mono.clone();
        config.usage_refresh = Duration::from_secs(3600);
        config.format_refresh = Duration::from_secs(3600);
        config.sleeper = Arc::new(Ticking {
            mono: self.mono.clone(),
        });
        config.marks = Some(self.marks.clone());
        config.marks_name = Some(BUCKET.to_owned());
        // The clocks of a test move by minutes at a time and the heartbeat of a proxy is not run.
        config.departure = Duration::from_secs(24 * 3600);
        config
    }

    /// The settings of a proxy `id` whose waits are those of the runtime's timer, for a test on a
    /// paused clock.
    pub fn config_sleeping(&self, id: &str) -> ProxyConfig {
        let mut config = self.config(id);
        config.sleeper = Arc::new(Sleeping {
            mono: self.mono.clone(),
        });
        config
    }

    /// The settings of a proxy `id` whose waits are counted and move no clock.
    pub fn config_counting(&self, id: &str) -> (ProxyConfig, Arc<Counting>) {
        let waits = Arc::new(Counting::default());
        let mut config = self.config(id);
        config.sleeper = waits.clone();
        (config, waits)
    }

    /// A live proxy `id` over the three tiers.
    pub async fn proxy(&self, id: &str) -> Arc<Proxy> {
        self.proxy_with(self.config(id)).await
    }

    /// A live proxy over the three tiers with these settings.
    pub async fn proxy_with(&self, config: ProxyConfig) -> Arc<Proxy> {
        self.proxy_over(self.hot.clone(), config).await
    }

    /// A live proxy whose hot tier is `hot`, a wrapper of the world's, with these settings.
    pub async fn proxy_over(&self, hot: Arc<dyn Backend>, config: ProxyConfig) -> Arc<Proxy> {
        let proxy = Proxy::open(hot, self.cold.clone(), config).await.unwrap();
        assert!(proxy.start_heartbeat(Arc::new(Never)));
        while !proxy.is_live() {
            tokio::task::yield_now().await;
        }
        Arc::new(proxy)
    }

    /// A leader `holder` over the three tiers, given the marks bucket.
    pub fn leader(&self, holder: &str) -> EntryReconciler {
        let mut base = ReconcilerConfig::new(holder, 1 << 30, self.clock.clone());
        base.mono = self.mono.clone();
        base.sleeper = Arc::new(Never);
        let config = EntryReconcilerConfig::new(base);
        EntryReconciler::new(self.hot.clone(), self.cold.clone(), config)
            .with_marks(self.marks.clone(), BUCKET)
            .unwrap()
    }

    /// Runs the leader's steps, moving the clocks on two minutes before each, until `key` has no
    /// marker: the lock reaped, the copy made and the tombstone held and let go.
    pub async fn converge(&self, leader: &mut EntryReconciler, key: &str) {
        for _ in 0..8 {
            self.pass(Duration::from_secs(121));
            let step = leader.step().await;
            assert!(step.failed.is_empty(), "{step:?}");
            if self.marker(key).await.is_none() {
                return;
            }
        }
        panic!(
            "the marker of {key} never went: {:?}",
            self.marker(key).await
        );
    }

    /// The calls on markers of an actor `id` that is not a proxy: a leader's pass, a test.
    pub fn markers(&self, id: &str) -> Markers {
        let sleeper = Arc::new(Ticking {
            mono: self.mono.clone(),
        });
        Markers::new(
            self.marks.clone(),
            id,
            MarkersConfig::default(),
            self.mono.clone(),
            sleeper,
        )
        .unwrap()
    }

    /// Moves both clocks on.
    pub fn pass(&self, span: Duration) {
        self.mono.advance(span);
        self.clock.advance(u64::try_from(span.as_millis()).unwrap());
    }

    /// The requests the three tiers received after `cursor`, a tier at a time and each tier in
    /// order, without those for the keys the format, the registrations and the usage live under;
    /// `cursor` then stands after them.
    pub fn since(&self, cursor: &mut Cursor) -> Vec<Request> {
        let tiers = [
            (Store::Hot, &self.hot, &mut cursor.hot),
            (Store::Marks, &self.marks, &mut cursor.marks),
            (Store::Cold, &self.cold, &mut cursor.cold),
        ];
        let mut all = Vec::new();
        for (store, tier, seen) in tiers {
            let log = tier.call_log();
            for (op, key) in log.iter().skip(*seen) {
                if !key.starts_with("_tiering/") {
                    all.push((store, *op, key.clone()));
                }
            }
            *seen = log.len();
        }
        all
    }

    /// A cursor after every request made so far.
    pub fn cursor(&self) -> Cursor {
        let mut cursor = Cursor::default();
        let _ = self.since(&mut cursor);
        cursor
    }

    /// The marker of `key` as stored, if any.
    pub async fn marker(&self, key: &str) -> Option<Marker> {
        let body = self.marks_mem.get(key, None).await.ok()?;
        Some(decode_marker(&body).unwrap())
    }

    /// The state and `gone` of the marker of `key`.
    pub async fn shape(&self, key: &str) -> Option<(MarkerState, bool)> {
        let marker = self.marker(key).await?;
        Some((marker.st, marker.gone))
    }

    /// The text of the object under `key` in the hot tier, if any.
    pub async fn hot_text(&self, key: &str) -> Option<String> {
        text_of(&*self.hot_mem, key).await
    }

    /// The text of the object under `key` in the cold tier, if any.
    pub async fn cold_text(&self, key: &str) -> Option<String> {
        text_of(&*self.cold_mem, key).await
    }
}

async fn text_of(backend: &dyn Backend, key: &str) -> Option<String> {
    let body = backend.get(key, None).await.ok()?;
    Some(String::from_utf8(body.to_vec()).unwrap())
}

/// A body with this text.
pub fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// The text of the object a proxy serves under `key`, or the error it answers.
pub async fn served(proxy: &Proxy, key: &str) -> Result<String, Error> {
    let body = proxy.get(key, None).await?;
    Ok(String::from_utf8(body.to_vec()).unwrap())
}

/// Counts the requests of one kind to one store.
pub fn count(requests: &[Request], store: Store, op: Op) -> usize {
    requests
        .iter()
        .filter(|(s, o, _)| *s == store && *o == op)
        .count()
}

/// A request.
pub fn req(store: Store, op: Op, key: &str) -> Request {
    (store, op, key.to_owned())
}

/// Lets the other tasks run until the marker of `key` stands: the ask for a copy-back that a cold
/// read makes in a task of its own. It waits on that progress and on no clock.
pub async fn marked(world: &World, key: &str) {
    for _ in 0..100_000 {
        if world.marker(key).await.is_some() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the marker of {key} never appeared");
}

/// Lets the other tasks run until `waits` has been asked for a wait: the proxy that owns it has met
/// a lock and is waiting for it.
pub async fn waiting(waits: &Counting) {
    for _ in 0..100_000 {
        if waits.count() > 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the proxy never waited");
}

/// Waits until no proxy holds a lock on the marker of `key`: the release after a put or a delete.
pub async fn released(world: &World, key: &str) {
    for _ in 0..100_000 {
        let held = world.marker(key).await.is_some_and(|m| m.st.is_held());
        if !held {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the lock on {key} was never released");
}
