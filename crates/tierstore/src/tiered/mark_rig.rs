//! A proxy with a marks bucket over memory tiers, for the tests of the reads and listings that read
//! markers: the hot tier is a memory backend behind a fault injector, and the cold tier and the
//! marks bucket are one behind a gate and then a fault injector, so a test counts the calls of a
//! tier (the injector is the outer layer, which counts a call once, whatever calls the gate passes
//! on) and holds a call of the cold tier or of the marks bucket.
//!
//! The format and the usage are read at every use (`format_refresh` and `usage_refresh` are zero),
//! so a test changes the phase by writing the format object, and nothing waits for a time: the
//! monotonic clock is a [`Mono`] the test moves.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use super::entry_put_tests::{ACTIVE_FORMAT, BUDGET, body};
use crate::journal::{FORMAT_KEY, USAGE_KEY, Usage};
use crate::{
    Backend, FaultyBackend, GateBackend, MARKER_VERSION, Marker, MarkerState, MarkerWhy,
    MemoryBackend, Mono, Proxy, ProxyConfig, RecordingSleeper, TestClock, decode_marker,
    encode_marker,
};

/// The format object of a store whose markers are in force.
pub(super) const ACTIVE_MARKS: &str =
    r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;
/// The format object of a store whose entries are drained, as it is between the roll of the
/// release and the activation of the markers.
pub(super) const DRAINED: &str = r#"{"phase":"drained"}"#;
/// The format object of a store that runs the entry protocol.
pub(super) const ENTRIES: &str = ACTIVE_FORMAT;

pub(super) struct MarkRig {
    pub(super) mono: Arc<Mono>,
    pub(super) hot_mem: Arc<MemoryBackend>,
    pub(super) cold_mem: Arc<MemoryBackend>,
    pub(super) marks_mem: Arc<MemoryBackend>,
    pub(super) hot_faults: Arc<FaultyBackend>,
    pub(super) cold_faults: Arc<FaultyBackend>,
    pub(super) marks_faults: Arc<FaultyBackend>,
    pub(super) cold: Arc<GateBackend>,
    pub(super) marks: Arc<GateBackend>,
    pub(super) sleeper: Arc<RecordingSleeper>,
    pub(super) proxy: Arc<Proxy>,
}

impl MarkRig {
    /// A proxy `p1` whose format object says `format` (none if empty), with the default settings.
    pub(super) async fn new(format: &str) -> Self {
        Self::tuned(format, |_| {}).await
    }

    /// As [`MarkRig::new`], with `tune` applied to the settings of the proxy.
    pub(super) async fn tuned(format: &str, tune: impl FnOnce(&mut ProxyConfig)) -> Self {
        let clock = Arc::new(TestClock::new());
        let mono = Arc::new(Mono::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let marks_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let hot_faults = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(GateBackend::new(cold_mem.clone()));
        let marks = Arc::new(GateBackend::new(marks_mem.clone()));
        let cold_faults = Arc::new(FaultyBackend::new(cold.clone()));
        let marks_faults = Arc::new(FaultyBackend::new(marks.clone()));
        let sleeper = Arc::new(RecordingSleeper::new());
        let mut config = ProxyConfig::new("p1", BUDGET, clock);
        config.mono = mono.clone();
        config.sleeper = sleeper.clone();
        config.marks = Some(marks_faults.clone());
        config.marks_name = Some("hot-marks".to_owned());
        config.usage_refresh = Duration::ZERO;
        config.format_refresh = Duration::ZERO;
        tune(&mut config);
        let proxy = Proxy::open(hot_faults.clone(), cold_faults.clone(), config)
            .await
            .unwrap();
        let rig = Self {
            mono,
            hot_mem,
            cold_mem,
            marks_mem,
            hot_faults,
            cold_faults,
            marks_faults,
            cold,
            marks,
            sleeper,
            proxy: Arc::new(proxy),
        };
        rig.set_format(format).await;
        rig
    }

    /// Writes the format object (removes it if `format` is empty). A put is create-only, so the
    /// object that is there is removed first.
    pub(super) async fn set_format(&self, format: &str) {
        // The object may not be there, which is what a removal wants.
        let _ = self.hot_mem.delete(FORMAT_KEY).await;
        if !format.is_empty() {
            self.hot_mem.put(FORMAT_KEY, body(format)).await.unwrap();
        }
    }

    /// Says the hot tier is full.
    pub(super) async fn fill_hot(&self) {
        let usage = Usage {
            hot_bytes: BUDGET,
            copy_lag: 0,
            time: 0,
        };
        let json = serde_json::to_vec(&usage).unwrap();
        let put = self.hot_mem.put(USAGE_KEY, Bytes::from(json));
        put.await.unwrap();
    }

    pub(super) async fn put_hot(&self, key: &str, text: &str) {
        self.hot_mem.put(key, body(text)).await.unwrap();
    }

    pub(super) async fn put_cold(&self, key: &str, text: &str) {
        self.cold_mem.put(key, body(text)).await.unwrap();
    }

    /// Stores the marker of `key` in the state `st`, with `gone`, as a writer of it would.
    pub(super) async fn mark(&self, key: &str, st: MarkerState, gone: bool) {
        let marker = Marker {
            v: MARKER_VERSION,
            key: key.to_owned(),
            st,
            gone,
            dirty: true,
            why: None,
            owner: None,
            nonce: "seed".to_owned(),
            extra: serde_json::Map::new(),
        };
        let put = self.marks_mem.put(key, encode_marker(&marker));
        put.await.unwrap();
    }

    /// Stores the marker of `key` as a lock for `why`, owned by `p2`, not `gone`.
    pub(super) async fn mark_lock(&self, key: &str, why: MarkerWhy) {
        let marker = Marker {
            v: MARKER_VERSION,
            key: key.to_owned(),
            st: MarkerState::Lock,
            gone: false,
            dirty: false,
            why: Some(why),
            owner: Some("p2".to_owned()),
            nonce: "seed".to_owned(),
            extra: serde_json::Map::new(),
        };
        let put = self.marks_mem.put(key, encode_marker(&marker));
        put.await.unwrap();
    }

    /// The marker of `key` as the marks bucket holds it.
    pub(super) async fn marker(&self, key: &str) -> Option<Marker> {
        let body = self.marks_mem.get(key, None).await.ok()?;
        Some(decode_marker(&body).unwrap())
    }
}
