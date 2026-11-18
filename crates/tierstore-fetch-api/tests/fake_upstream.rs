//! `FakeUpstream` serves the objects and the scripts a test gives it, and logs every request.
//!
//! Catches:
//! - a `locate` that ignores a scripted answer, answers a hit for an unknown key, or is not logged;
//! - a part whose bytes, chunking or generation differ from the range asked;
//! - an `If-Match` of another generation that is served, or a 412 that loses the actual generation;
//! - a range outside the object, or one whose end overflows, that is served;
//! - a `Hang` that yields a byte, a `Truncate` that delivers a wrong count, a `Corrupt` that flips
//!   another byte, or flips one outside the range;
//! - a request or a report that is not logged in order.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use tierstore_fetch_api::fake::{FakeObject, FakeUpstream, PartScript};
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, DriveState, GetRequest, Located, Location, NotDefinitive,
    PartResponse, Report, Source, SourceKind, Upstream, UpstreamError,
};
use tierstore_format::ids::{Generation, NodeId};

fn key(s: &str) -> PackKey {
    PackKey(s.to_owned())
}

fn generation_of(b: u8) -> Generation {
    Generation([b; 16])
}

fn source(len: u64) -> Source {
    Source {
        device: DeviceRef::Hot(NodeId(1)),
        kind: SourceKind::Hot {
            file: "seg".into(),
            offset: 0,
        },
        extent: ByteRange { offset: 0, len },
    }
}

fn location(g: Generation, size: u64) -> Location {
    Location {
        generation: g,
        size: Some(size),
        sources: vec![source(size)],
        conflicts: vec![],
    }
}

fn bytes10() -> Bytes {
    Bytes::from_static(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
}

fn upstream() -> FakeUpstream {
    let u = FakeUpstream::new();
    u.insert(
        key("a"),
        FakeObject {
            generation: generation_of(1),
            bytes: bytes10(),
            location: location(generation_of(1), 10),
        },
    );
    u
}

fn req(offset: u64, len: u64, g: Generation) -> GetRequest {
    GetRequest {
        key: key("a"),
        source: source(10),
        range: ByteRange { offset, len },
        generation: g,
        class: Class::Demand,
        job: JobId(3),
        rank: Rank(4),
    }
}

async fn chunks(r: PartResponse) -> Vec<Result<Vec<u8>, UpstreamError>> {
    r.body
        .map(|c| c.map(|b| b.to_vec()))
        .collect::<Vec<_>>()
        .await
}

fn is_transport(e: &UpstreamError) -> bool {
    matches!(e, UpstreamError::Transport { .. })
}

#[tokio::test]
async fn locate_answers_the_location_of_a_known_object() {
    let u = upstream();
    let got = u.locate(&key("a")).await.unwrap();
    assert_eq!(got, Located::Found(location(generation_of(1), 10)));
}

#[tokio::test]
async fn locate_of_an_unknown_key_is_a_definitive_miss() {
    let u = upstream();
    assert_eq!(u.locate(&key("zz")).await.unwrap(), Located::DefinitiveMiss);
}

#[tokio::test]
async fn scripted_locate_answers_come_in_order_per_key_then_fall_back() {
    let u = upstream();
    u.script_locate(
        key("a"),
        Ok(Located::NotDefinitive(NotDefinitive::Indexing)),
    );
    u.script_locate(
        key("a"),
        Err(UpstreamError::NotFound {
            key: key("a"),
            device: DeviceRef::Hot(NodeId(1)),
        }),
    );
    u.script_locate(
        key("b"),
        Ok(Located::NotDefinitive(NotDefinitive::TooFewAnswers)),
    );

    assert_eq!(
        u.locate(&key("a")).await.unwrap(),
        Located::NotDefinitive(NotDefinitive::Indexing)
    );
    assert!(matches!(
        u.locate(&key("a")).await,
        Err(UpstreamError::NotFound { .. })
    ));
    assert_eq!(
        u.locate(&key("a")).await.unwrap(),
        Located::Found(location(generation_of(1), 10))
    );
    assert_eq!(
        u.locate(&key("b")).await.unwrap(),
        Located::NotDefinitive(NotDefinitive::TooFewAnswers)
    );
    assert_eq!(u.locate(&key("b")).await.unwrap(), Located::DefinitiveMiss);
}

#[tokio::test]
async fn every_locate_is_logged_in_order() {
    let u = upstream();
    let _ = u.locate(&key("a")).await;
    let _ = u.locate(&key("q")).await;
    let _ = u.locate(&key("a")).await;
    assert_eq!(u.locates(), vec![key("a"), key("q"), key("a")]);
}

#[tokio::test]
async fn get_serves_the_exact_range_in_one_chunk_by_default() {
    let u = upstream();
    let r = u.get(req(2, 5, generation_of(1))).await.unwrap();
    assert_eq!(r.generation, generation_of(1));
    assert_eq!(r.signals, DriveSignals::default());
    assert_eq!(chunks(r).await, vec![Ok(vec![2, 3, 4, 5, 6])]);
}

#[tokio::test]
async fn get_of_the_whole_object_and_of_its_last_byte_is_served() {
    let u = upstream();
    let all = u.get(req(0, 10, generation_of(1))).await.unwrap();
    assert_eq!(chunks(all).await, vec![Ok((0..10).collect::<Vec<u8>>())]);
    let last = u.get(req(9, 1, generation_of(1))).await.unwrap();
    assert_eq!(chunks(last).await, vec![Ok(vec![9])]);
}

#[tokio::test]
async fn an_empty_range_yields_no_chunk() {
    let u = upstream();
    let r = u.get(req(10, 0, generation_of(1))).await.unwrap();
    assert!(chunks(r).await.is_empty());
}

#[tokio::test]
async fn a_serve_script_splits_the_range_in_chunks_of_its_size_and_passes_signals() {
    let u = upstream();
    let signals = DriveSignals {
        queue_wait_ms: Some(1),
        service_ms: Some(2),
        drive_queue: Some(3),
        drive_state: Some(DriveState::Suspect),
    };
    u.script_part(PartScript::Serve { chunk: 3, signals });
    let r = u.get(req(0, 10, generation_of(1))).await.unwrap();
    assert_eq!(r.signals, signals);
    assert_eq!(
        chunks(r).await,
        vec![
            Ok(vec![0, 1, 2]),
            Ok(vec![3, 4, 5]),
            Ok(vec![6, 7, 8]),
            Ok(vec![9])
        ]
    );
}

#[tokio::test]
async fn a_chunk_size_of_zero_is_the_whole_range_in_one_chunk() {
    let u = upstream();
    u.script_part(PartScript::Serve {
        chunk: 0,
        signals: DriveSignals::default(),
    });
    let r = u.get(req(4, 4, generation_of(1))).await.unwrap();
    assert_eq!(chunks(r).await, vec![Ok(vec![4, 5, 6, 7])]);
}

#[tokio::test]
async fn if_match_of_another_generation_is_a_412_naming_the_actual_one() {
    let u = upstream();
    let err = u.get(req(0, 4, generation_of(9))).await.unwrap_err();
    assert_eq!(
        err,
        UpstreamError::PreconditionFailed {
            key: key("a"),
            device: DeviceRef::Hot(NodeId(1)),
            actual: Some(generation_of(1)),
        }
    );
}

#[tokio::test]
async fn get_of_an_unknown_key_is_not_found() {
    let u = upstream();
    let mut r = req(0, 4, generation_of(1));
    r.key = key("nope");
    let err = u.get(r).await.unwrap_err();
    assert_eq!(
        err,
        UpstreamError::NotFound {
            key: key("nope"),
            device: DeviceRef::Hot(NodeId(1)),
        }
    );
}

#[tokio::test]
async fn a_range_past_the_end_of_the_object_is_a_transport_error() {
    let u = upstream();
    assert!(is_transport(
        &u.get(req(8, 3, generation_of(1))).await.unwrap_err()
    ));
    assert!(is_transport(
        &u.get(req(11, 0, generation_of(1))).await.unwrap_err()
    ));
    assert!(is_transport(
        &u.get(req(10, 1, generation_of(1))).await.unwrap_err()
    ));
}

#[tokio::test]
async fn a_range_whose_end_overflows_is_a_transport_error() {
    let u = upstream();
    assert!(is_transport(
        &u.get(req(u64::MAX, 2, generation_of(1))).await.unwrap_err()
    ));
    assert!(is_transport(
        &u.get(req(1, u64::MAX, generation_of(1))).await.unwrap_err()
    ));
}

#[tokio::test]
async fn a_scripted_failure_is_returned_as_given() {
    let u = upstream();
    let e = UpstreamError::Unavailable {
        key: key("a"),
        device: DeviceRef::Hot(NodeId(1)),
        retry_after_ms: 250,
        signals: DriveSignals::default(),
    };
    u.script_part(PartScript::Fail(e.clone()));
    assert_eq!(u.get(req(0, 1, generation_of(1))).await.unwrap_err(), e);
    // The script is consumed: the next part is served.
    assert!(u.get(req(0, 1, generation_of(1))).await.is_ok());
}

#[tokio::test]
async fn a_hung_part_answers_its_headers_and_yields_no_byte() {
    let u = upstream();
    u.script_part(PartScript::Hang);
    let mut r = u.get(req(0, 4, generation_of(1))).await.unwrap();
    assert_eq!(r.generation, generation_of(1));
    assert!(r.body.next().now_or_never().is_none());
    // Still pending on a second poll: nothing arrives.
    assert!(r.body.next().now_or_never().is_none());
}

#[tokio::test]
async fn a_hung_part_still_fails_an_if_match_of_another_generation() {
    let u = upstream();
    u.script_part(PartScript::Hang);
    assert!(matches!(
        u.get(req(0, 4, generation_of(7))).await,
        Err(UpstreamError::PreconditionFailed { .. })
    ));
}

#[tokio::test]
async fn a_truncated_part_delivers_exactly_after_bytes_then_a_transport_error() {
    let u = upstream();
    u.script_part(PartScript::Truncate { after: 3 });
    let r = u.get(req(1, 8, generation_of(1))).await.unwrap();
    let got = chunks(r).await;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0], Ok(vec![1, 2, 3]));
    assert!(is_transport(got[1].as_ref().unwrap_err()));
}

#[tokio::test]
async fn a_truncation_past_the_range_delivers_the_whole_range_then_the_error() {
    let u = upstream();
    u.script_part(PartScript::Truncate { after: 100 });
    let r = u.get(req(8, 2, generation_of(1))).await.unwrap();
    let got = chunks(r).await;
    assert_eq!(got[0], Ok(vec![8, 9]));
    assert!(is_transport(got[1].as_ref().unwrap_err()));
}

#[tokio::test]
async fn a_corrupt_script_flips_exactly_the_byte_at_the_object_offset() {
    let u = upstream();
    u.script_part(PartScript::Corrupt { offset: 5 });
    let r = u.get(req(3, 4, generation_of(1))).await.unwrap();
    assert_eq!(chunks(r).await, vec![Ok(vec![3, 4, 5 ^ 0xff, 6])]);
}

#[tokio::test]
async fn a_corrupt_offset_outside_the_range_changes_nothing() {
    let u = upstream();
    u.script_part(PartScript::Corrupt { offset: 1 });
    let before = u.get(req(3, 4, generation_of(1))).await.unwrap();
    assert_eq!(chunks(before).await, vec![Ok(vec![3, 4, 5, 6])]);
    u.script_part(PartScript::Corrupt { offset: 7 });
    let after = u.get(req(3, 4, generation_of(1))).await.unwrap();
    assert_eq!(chunks(after).await, vec![Ok(vec![3, 4, 5, 6])]);
    u.script_part(PartScript::Corrupt { offset: u64::MAX });
    let far = u.get(req(3, 4, generation_of(1))).await.unwrap();
    assert_eq!(chunks(far).await, vec![Ok(vec![3, 4, 5, 6])]);
}

#[tokio::test]
async fn gets_logs_every_request_in_order_including_failures() {
    let u = upstream();
    let a = req(0, 2, generation_of(1));
    let b = req(2, 2, generation_of(9));
    let mut c = req(0, 1, generation_of(1));
    c.key = key("nope");
    let _ = u.get(a.clone()).await;
    let _ = u.get(b.clone()).await;
    let _ = u.get(c.clone()).await;
    assert_eq!(u.gets(), vec![a, b, c]);
}

#[tokio::test]
async fn reports_are_logged_with_their_arguments() {
    let u = upstream();
    assert!(u.reports().is_empty());
    let s = source(10);
    let range = ByteRange { offset: 4, len: 2 };
    u.report(&key("a"), &s, range, Report::Bitrot).await;
    assert_eq!(u.reports(), vec![(key("a"), s, range, Report::Bitrot)]);
}

#[tokio::test]
async fn insert_replaces_an_object_under_the_same_key() {
    let u = upstream();
    u.insert(
        key("a"),
        FakeObject {
            generation: generation_of(2),
            bytes: Bytes::from_static(&[7, 7]),
            location: location(generation_of(2), 2),
        },
    );
    let r = u.get(req(0, 2, generation_of(2))).await.unwrap();
    assert_eq!(r.generation, generation_of(2));
    assert_eq!(chunks(r).await, vec![Ok(vec![7, 7])]);
}
