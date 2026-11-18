//! What a location, a part request and a part response carry (`upstream`).
//!
//! Proves which node a device belongs to, what a source charges against the client ceiling, the
//! `x-ts-drive-state` and `x-ts-report` header values (round trip for every state, nothing else
//! parses), which drive states are load signals, and the exact text of every `UpstreamError`.
//!
//! Catches:
//! - a device attributed to the wrong node;
//! - a degraded read that charges less (or more) than `k`, or a plain read that charges other than 1;
//! - a drive state header that does not round trip, collides, or parses from a miscased value;
//! - `stalled` or `indexing` counted as a load signal, or any other state not;
//! - an error message that drops the object, the device or the reason;
//! - a part response whose `Debug` prints the body.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use futures::StreamExt;
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, DriveState, GetRequest, Located, Location, NotDefinitive,
    PartResponse, Report, Source, SourceKind, UpstreamError,
};
use tierstore_format::ids::{BlockId, DriveId, Generation, NodeId};

fn range() -> ByteRange {
    ByteRange { offset: 0, len: 10 }
}

fn source(kind: SourceKind) -> Source {
    Source {
        device: DeviceRef::Hot(NodeId(1)),
        kind,
        extent: range(),
    }
}

fn block() -> BlockId {
    BlockId::new(NodeId(2), 5).expect("counter fits")
}

fn key() -> PackKey {
    PackKey("k".into())
}

#[test]
fn a_device_belongs_to_its_storage_node() {
    let d = DriveId([7; 16]);
    assert_eq!(DeviceRef::Hot(NodeId(3)).node(), NodeId(3));
    assert_eq!(DeviceRef::Drive(NodeId(4), d).node(), NodeId(4));
}

#[test]
fn devices_order_hot_before_drive_and_by_node() {
    let d = DriveId([0; 16]);
    assert!(DeviceRef::Hot(NodeId(9)) < DeviceRef::Drive(NodeId(0), d));
    assert!(DeviceRef::Drive(NodeId(1), d) < DeviceRef::Drive(NodeId(2), d));
    assert!(DeviceRef::Hot(NodeId(1)) < DeviceRef::Hot(NodeId(2)));
}

#[test]
fn a_degraded_source_charges_one_stream_per_source_drive() {
    let drives = |n: u8| -> Vec<DeviceRef> {
        (0..n)
            .map(|i| DeviceRef::Drive(NodeId(i), DriveId([i; 16])))
            .collect()
    };
    for n in [0u8, 1, 4, 6, 12] {
        let s = source(SourceKind::Degraded {
            block: block(),
            offset: 0,
            sources: drives(n),
        });
        assert_eq!(s.charge(), u32::from(n), "{n} sources");
    }
}

#[test]
fn every_other_source_charges_one_stream() {
    for kind in [
        SourceKind::Hot {
            file: "seg".into(),
            offset: 1,
        },
        SourceKind::HotClean {
            file: "cache".into(),
            offset: 2,
        },
        SourceKind::Cold {
            block: block(),
            shard: "s".into(),
            offset: 3,
        },
    ] {
        assert_eq!(source(kind).charge(), 1);
    }
}

const STATES: [(DriveState, &str); 8] = [
    (DriveState::Up, "up"),
    (DriveState::Indexing, "indexing"),
    (DriveState::Stalled, "stalled"),
    (DriveState::Suspect, "suspect"),
    (DriveState::Down, "down"),
    (DriveState::Failed, "failed"),
    (DriveState::Drain, "drain"),
    (DriveState::Out, "out"),
];

#[test]
fn every_drive_state_header_round_trips() {
    for (state, text) in STATES {
        assert_eq!(state.header(), text);
        assert_eq!(DriveState::from_header(text), Some(state));
    }
}

#[test]
fn an_unknown_or_miscased_drive_state_header_is_refused() {
    for bad in ["", "UP", "Up", "up ", "unknown", "draining", "faile"] {
        assert_eq!(DriveState::from_header(bad), None, "{bad:?}");
    }
}

#[test]
fn only_stalled_and_indexing_are_not_load_signals() {
    for (state, _) in STATES {
        let want = !matches!(state, DriveState::Stalled | DriveState::Indexing);
        assert_eq!(state.is_load_signal(), want, "{state:?}");
    }
    assert!(!DriveState::Stalled.is_load_signal());
    assert!(!DriveState::Indexing.is_load_signal());
    assert!(DriveState::Up.is_load_signal());
}

#[test]
fn the_report_header_is_bitrot() {
    assert_eq!(Report::Bitrot.header(), "bitrot");
}

#[test]
fn drive_signals_default_to_nothing_said() {
    assert_eq!(
        DriveSignals::default(),
        DriveSignals {
            queue_wait_ms: None,
            service_ms: None,
            drive_queue: None,
            drive_state: None
        }
    );
}

#[test]
fn a_located_answer_compares_by_what_it_carries() {
    let loc = Location {
        generation: Generation([1; 16]),
        size: Some(10),
        sources: vec![source(SourceKind::Hot {
            file: "seg".into(),
            offset: 0,
        })],
        conflicts: vec![Generation([2; 16])],
    };
    let found = Located::Found(loc.clone());
    assert_eq!(found, Located::Found(loc));
    assert_ne!(found, Located::DefinitiveMiss);
    assert_ne!(
        Located::NotDefinitive(NotDefinitive::Indexing),
        Located::NotDefinitive(NotDefinitive::TooFewAnswers)
    );
}

#[test]
fn a_get_request_compares_by_what_it_carries() {
    let req = GetRequest {
        key: key(),
        source: source(SourceKind::HotClean {
            file: "cache".into(),
            offset: 0,
        }),
        range: range(),
        generation: Generation([2; 16]),
        class: Class::Demand,
        job: JobId(1),
        rank: Rank(0),
    };
    let mut other = req.clone();
    assert_eq!(req, other);
    other.generation = Generation([3; 16]);
    assert_ne!(req, other);
}

#[test]
fn every_upstream_error_names_the_object_the_device_and_the_reason() {
    let k = key();
    let dev = DeviceRef::Hot(NodeId(5));
    assert_eq!(
        UpstreamError::NotFound {
            key: k.clone(),
            device: dev
        }
        .to_string(),
        "PackKey(\"k\"): not found at Hot(NodeId(5))"
    );
    assert_eq!(
        UpstreamError::PreconditionFailed {
            key: k.clone(),
            device: DeviceRef::Hot(NodeId(1)),
            actual: None
        }
        .to_string(),
        "PackKey(\"k\"): Hot(NodeId(1)) holds another generation (None)"
    );
    assert_eq!(
        UpstreamError::Unavailable {
            key: k.clone(),
            device: dev,
            retry_after_ms: 250,
            signals: DriveSignals::default()
        }
        .to_string(),
        "PackKey(\"k\"): Hot(NodeId(5)) unavailable (retry after 250 ms, DriveSignals { queue_wait_ms: None, service_ms: None, drive_queue: None, drive_state: None })"
    );
    assert_eq!(
        UpstreamError::Corrupt {
            key: k.clone(),
            device: dev,
            offset: 1_048_576
        }
        .to_string(),
        "PackKey(\"k\"): Hot(NodeId(5)) found a crc mismatch at 1048576"
    );
    assert_eq!(
        UpstreamError::Transport {
            key: k,
            device: dev,
            reason: "connection reset".into()
        }
        .to_string(),
        "PackKey(\"k\"): transport to Hot(NodeId(5)): connection reset"
    );
}

#[test]
fn a_part_response_debug_shows_the_headers_and_not_the_body() {
    let body = futures::stream::iter([Ok(Bytes::from_static(b"secret bytes"))]).boxed();
    let resp = PartResponse {
        generation: Generation([0xAB; 16]),
        signals: DriveSignals {
            service_ms: Some(4),
            ..DriveSignals::default()
        },
        body,
    };
    let text = format!("{resp:?}");
    assert!(
        text.starts_with("PartResponse { generation: Generation("),
        "{text}"
    );
    assert!(text.contains("service_ms: Some(4)"), "{text}");
    assert!(text.ends_with(", .. }"), "{text}");
    assert!(!text.contains("secret"), "{text}");
    assert!(!text.contains("body"), "{text}");
}

#[tokio::test]
async fn a_part_response_streams_its_body_in_order() {
    let parts = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
    let mut resp = PartResponse {
        generation: Generation([1; 16]),
        signals: DriveSignals::default(),
        body: futures::stream::iter(parts).boxed(),
    };
    let mut got = Vec::new();
    while let Some(part) = resp.body.next().await {
        got.extend_from_slice(&part.expect("a part"));
    }
    assert_eq!(got, b"abcd");
}
