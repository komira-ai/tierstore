//! The proxy under test for the puts made on markers: a hot tier, a cold tier and a marks bucket,
//! each of memory behind a fault injector (and, for the hot tier and the marks bucket, a gate), a
//! format object that says `active-marks`, a monotonic clock the test moves and a sleeper that
//! moves it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;

use crate::journal::FORMAT_KEY;
use crate::{
    Backend, Error, FaultyBackend, GateBackend, MARKER_VERSION, Marker, MarkerState, MarkerWhy,
    MemoryBackend, Mono, Op, PartBody, Proxy, ProxyConfig, Sleeper, TestClock, decode_marker,
    encode_marker,
};

/// The format object of a store whose puts are made on markers, in the bucket `hot-marks`.
pub(super) const MARKS_FORMAT: &str =
    r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;
/// The name of the marks bucket the proxy of the rig has.
pub(super) const MARKS_NAME: &str = "hot-marks";
/// The hot tier's budget, in bytes.
pub(super) const BUDGET: u64 = 1000;

pub(super) fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// A body of the chunks `parts`.
pub(super) fn chunks(parts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts.iter().map(|part| Ok(body(part))).collect();
    stream::iter(items).boxed()
}

pub(super) fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Something done to the stores once the sleeper has been asked for so many pauses.
pub(super) enum Effect {
    /// Replace the marker of the key by this one, or remove it.
    Marker(String, Option<Marker>),
    /// Remove the hot object of the key.
    DeleteHot(String),
    /// Fail the next HEAD of the hot tier with this error.
    FailHotHead(Error),
}

/// A sleeper that moves the monotonic clock by the pause it is asked for, as a real wait would,
/// writes the pause down, and does an effect to the stores once it has been asked that many
/// times.
pub(super) struct Ticking {
    mono: Arc<Mono>,
    hot: Arc<MemoryBackend>,
    hot_faults: Arc<FaultyBackend>,
    marks: Arc<MemoryBackend>,
    asked: Mutex<Vec<Duration>>,
    effects: Mutex<Vec<(usize, Effect)>>,
}

impl Ticking {
    /// The pauses asked for so far, in order.
    pub(super) fn asked(&self) -> Vec<Duration> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Do `effect` when the sleeper has been asked `after` times.
    pub(super) fn then(&self, after: usize, effect: Effect) {
        let mut effects = self.effects.lock().unwrap_or_else(PoisonError::into_inner);
        effects.push((after, effect));
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
        let due: Vec<Effect> = {
            let mut effects = self.effects.lock().unwrap_or_else(PoisonError::into_inner);
            let (due, rest) = std::mem::take(&mut *effects)
                .into_iter()
                .partition(|(after, _)| *after == pauses);
            *effects = rest;
            due.into_iter().map(|(_, effect)| effect).collect()
        };
        for effect in due {
            match effect {
                Effect::Marker(key, marker) => {
                    self.marks.delete(&key).await.unwrap();
                    if let Some(marker) = marker {
                        self.marks.put(&key, encode_marker(&marker)).await.unwrap();
                    }
                }
                Effect::DeleteHot(key) => self.hot.delete(&key).await.unwrap(),
                Effect::FailHotHead(error) => self.hot_faults.fail_next_with(Op::Head, error),
            }
        }
    }
}

/// A live proxy `p1` over the three stores, whose format says what the test wants.
pub(super) struct MarkRig {
    pub(super) mono: Arc<Mono>,
    pub(super) hot_mem: Arc<MemoryBackend>,
    pub(super) cold_mem: Arc<MemoryBackend>,
    pub(super) marks_mem: Arc<MemoryBackend>,
    pub(super) hot_faults: Arc<FaultyBackend>,
    pub(super) hot: Arc<GateBackend>,
    pub(super) cold_faults: Arc<FaultyBackend>,
    pub(super) cold: Arc<GateBackend>,
    pub(super) marks_faults: Arc<FaultyBackend>,
    pub(super) marks: Arc<GateBackend>,
    pub(super) proxy: Arc<Proxy>,
    pub(super) sleeper: Arc<Ticking>,
}

impl MarkRig {
    /// A proxy whose format says `active-marks` with the bucket the proxy has.
    pub(super) async fn new() -> Self {
        Self::with_format(MARKS_FORMAT).await
    }

    /// A proxy whose format object is `format` (none if empty).
    pub(super) async fn with_format(format: &str) -> Self {
        Self::tuned(format, |_| {}).await
    }

    /// As [`MarkRig::with_format`], with `tune` applied to the settings first.
    pub(super) async fn tuned(format: &str, tune: impl FnOnce(&mut ProxyConfig)) -> Self {
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
        let cold_faults = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let cold = Arc::new(GateBackend::new(cold_faults.clone()));
        let marks_faults = Arc::new(FaultyBackend::new(marks_mem.clone()));
        let marks = Arc::new(GateBackend::new(marks_faults.clone()));
        let sleeper = Arc::new(Ticking {
            mono: mono.clone(),
            hot: hot_mem.clone(),
            hot_faults: hot_faults.clone(),
            marks: marks_mem.clone(),
            asked: Mutex::new(Vec::new()),
            effects: Mutex::new(Vec::new()),
        });
        let mut config = ProxyConfig::new("p1", BUDGET, clock.clone());
        config.mono = mono.clone();
        config.usage_refresh = Duration::ZERO;
        config.format_refresh = Duration::ZERO;
        config.marks = Some(marks.clone());
        config.marks_name = Some(MARKS_NAME.to_owned());
        config.sleeper = sleeper.clone();
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
            hot,
            cold_faults,
            cold,
            marks_faults,
            marks,
            proxy: Arc::new(proxy),
            sleeper,
        };
        rig.live();
        rig
    }

    /// Moves `live_until` of the proxy to ten minutes from now.
    pub(super) fn live(&self) {
        let until = crate::Clock::now(&*self.mono) + 600_000;
        self.proxy.inner.heartbeat.deadline.extend_to(until);
    }

    /// The marker of `key` as the marks bucket holds it, with its tag.
    pub(super) async fn stored(&self, key: &str) -> Option<(Marker, String)> {
        let (body, tag) = self.marks_mem.get_tagged(key, None).await.ok()?;
        Some((decode_marker(&body).unwrap(), tag))
    }

    /// The marker of `key`.
    pub(super) async fn marker(&self, key: &str) -> Option<Marker> {
        self.stored(key).await.map(|(marker, _)| marker)
    }

    /// Stores `marker` under its key as another actor wrote it.
    pub(super) async fn seed(&self, marker: &Marker) {
        let body = encode_marker(marker);
        self.marks_mem.put(&marker.key, body).await.unwrap();
    }

    pub(super) async fn hot_has(&self, key: &str) -> bool {
        self.hot_mem.head(key).await.is_ok()
    }

    pub(super) async fn cold_has(&self, key: &str) -> bool {
        self.cold_mem.head(key).await.is_ok()
    }

    /// Says the hot tier is full.
    pub(super) async fn fill_hot(&self) {
        let usage = crate::journal::Usage {
            hot_bytes: BUDGET,
            copy_lag: 0,
            time: 0,
        };
        let json = serde_json::to_vec(&usage).unwrap();
        let key = crate::journal::USAGE_KEY;
        self.hot_mem.put(key, Bytes::from(json)).await.unwrap();
    }

    /// The kinds of the calls the marks bucket has had on `key`, in order.
    pub(super) fn marks_ops(&self, key: &str) -> Vec<Op> {
        ops(&self.marks_faults, key)
    }

    /// The kinds of the calls the hot tier has had on `key`, in order.
    pub(super) fn hot_ops(&self, key: &str) -> Vec<Op> {
        ops(&self.hot_faults, key)
    }

    /// The kinds of the calls the cold tier has had on `key`, in order.
    pub(super) fn cold_ops(&self, key: &str) -> Vec<Op> {
        ops(&self.cold_faults, key)
    }
}

fn ops(faults: &FaultyBackend, key: &str) -> Vec<Op> {
    let log = faults.call_log();
    log.into_iter()
        .filter(|(_, called)| called == key)
        .map(|(op, _)| op)
        .collect()
}

/// A marker as another actor leaves it.
pub(super) fn marker(key: &str, st: MarkerState, gone: bool, dirty: bool) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone,
        dirty,
        why: None,
        owner: None,
        nonce: "other".to_owned(),
        extra: serde_json::Map::new(),
    }
}

/// A `DIRTY` marker of `key`, `gone` as told.
pub(super) fn dirty(key: &str, gone: bool) -> Marker {
    marker(key, MarkerState::Dirty, gone, true)
}

/// A lock on `key` held by another proxy for `why`.
pub(super) fn locked(key: &str, why: MarkerWhy, gone: bool) -> Marker {
    Marker {
        why: Some(why),
        owner: Some("other".to_owned()),
        ..marker(key, MarkerState::Lock, gone, true)
    }
}
