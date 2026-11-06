//! The rounds of a node's reconciler under the format `active-marks`: the markers are worked only
//! if the node has the marks bucket the record names. The tiers are in memory; no test reads a
//! clock or waits for a time.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use super::{EntryTuning, Rounds};
use crate::journal::FORMAT_KEY;
use crate::{
    Backend, MARKER_VERSION, Marker, MarkerState, MemoryBackend, ReconcilerConfig, TestClock,
    encode_marker,
};

const BUCKET: &str = "tierstore-marks";
const DEPARTURE: Duration = Duration::from_secs(60);

/// A hot, a cold and a marks tier in memory, with a key `k` in hot and a `DIRTY` marker for it.
struct Rig {
    clock: Arc<TestClock>,
    hot: Arc<MemoryBackend>,
    cold: Arc<MemoryBackend>,
    marks: Arc<MemoryBackend>,
}

impl Rig {
    async fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(6_000_000);
        let tier = || Arc::new(MemoryBackend::with_clock(clock.clone()));
        let rig = Self {
            hot: tier(),
            cold: tier(),
            marks: tier(),
            clock,
        };
        rig.hot.put("k", Bytes::from_static(b"v")).await.unwrap();
        let marker = Marker {
            v: MARKER_VERSION,
            key: "k".to_owned(),
            st: MarkerState::Dirty,
            gone: false,
            dirty: true,
            why: None,
            owner: None,
            nonce: "planted".to_owned(),
            extra: serde_json::Map::new(),
        };
        rig.marks.put("k", encode_marker(&marker)).await.unwrap();
        rig
    }

    fn rounds(&self, marks: Option<&str>, tuning: &EntryTuning) -> Result<Rounds, String> {
        let config = ReconcilerConfig::new("node/reconciler", 1 << 20, self.clock.clone());
        let marks = marks.map(|name| (name.to_owned(), self.marks.clone() as Arc<dyn Backend>));
        let hot: Arc<dyn Backend> = self.hot.clone();
        Rounds::tuned(hot, self.cold.clone(), marks, config, DEPARTURE, tuning)
    }

    async fn format(&self, bucket: &str) {
        let body = format!(r#"{{"epoch":3,"marks_bucket":"{bucket}","phase":"active-marks"}}"#);
        self.hot.delete(FORMAT_KEY).await.ok();
        self.hot.put(FORMAT_KEY, Bytes::from(body)).await.unwrap();
    }
}

/// Asserts that a round works the markers under `active-marks` when the node has the marks bucket
/// and the record names it, and does nothing when the record names another bucket or the node has
/// none: the hot object is copied and the marker removed only in the first case.
#[tokio::test]
async fn a_round_works_the_markers_only_where_the_record_names_the_nodes_marks_bucket() {
    let tuning = EntryTuning::default();
    for (node, record, works) in [
        (Some(BUCKET), BUCKET, true),
        (Some(BUCKET), "another-marks", false),
        (None, BUCKET, false),
    ] {
        let rig = Rig::new().await;
        rig.format(record).await;
        let mut rounds = rig.rounds(node, &tuning).unwrap();
        let round = rounds.round().await;
        assert!(round.failed.is_empty(), "{:?}", round.failed);
        assert_eq!(
            rig.cold.get("k", None).await.is_ok(),
            works,
            "{node:?} {record}"
        );
        assert_eq!(
            rig.marks.get("k", None).await.is_err(),
            works,
            "{node:?} {record}"
        );
        rounds.stop().await;
    }
}

/// Asserts that the settings of the markers that do not hold together are refused with the text of
/// the refusal (the loader has checked them, so this is the guard of a caller that did not).
#[tokio::test]
async fn markers_settings_that_do_not_hold_together_are_refused_when_the_rounds_are_made() {
    let rig = Rig::new().await;
    let tuning = EntryTuning {
        lock_lease: Duration::from_secs(1),
        ..EntryTuning::default()
    };
    let refused = rig.rounds(Some(BUCKET), &tuning).err();
    assert_eq!(
        refused.as_deref(),
        Some("lock_lease (1s) is below 2 x lock_refresh (30s) + queued_call_margin (60s)")
    );
    assert!(rig.rounds(None, &tuning).is_ok());
}
