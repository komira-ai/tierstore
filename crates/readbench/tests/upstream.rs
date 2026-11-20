//! `SwfsUpstream` against the in-process fake store: locations from the filer's entries, parts
//! from the volume servers, and what the recorder keeps of them.

#![expect(
    clippy::unwrap_used,
    reason = "the helpers of this file are part of its tests: their failure is the failure reported"
)]

#[cfg(test)]
#[expect(dead_code, reason = "this test file uses part of the fake")]
mod support;

use std::sync::Arc;

use futures::StreamExt as _;
use readbench::clock::ManualClock;
use readbench::http::HttpClient;
use readbench::swfs::{FilerFetch, VolumeServer, load_map};
use readbench::upstream::{PartTiming, Recorder, SwfsUpstream};
use support::{Fake, Kind};
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, GetRequest, Located, NotDefinitive, Report, Source, SourceKind,
    Upstream, UpstreamError,
};
use tierstore_format::ids::{Generation, NodeId};

const BUCKET: &str = "packs";
const KEY: &str = "cas/b/p1/pack";

fn data() -> Vec<u8> {
    b"0123456789".to_vec()
}

/// An upstream over a fake with the 10-byte object `KEY` in chunks of 4 bytes on volume 7 (on
/// both servers, nodes 2 and 3, disk 1).
struct Rig {
    fake: Fake,
    up: SwfsUpstream,
    recorder: Arc<Recorder>,
    clock: Arc<ManualClock>,
}

async fn rig_with(map_servers: usize) -> Rig {
    let fake = Fake::builder()
        .volume(7, &[0, 1], 1)
        .chunk_size(4)
        .object(BUCKET, KEY, data())
        .start()
        .await;
    let http = HttpClient::new(10_000);
    let servers: Vec<VolumeServer> = (0..map_servers)
        .map(|i| VolumeServer {
            node: 2 + u8::try_from(i).unwrap(),
            addr: fake.volume(i).addr().to_owned(),
        })
        .collect();
    let (map, failed) = load_map(&http, &servers).await;
    assert!(failed.is_empty());
    let recorder = Recorder::new();
    let clock = ManualClock::new(0);
    let up = SwfsUpstream::new(
        FilerFetch {
            http: http.clone(),
            base: fake.filer(0).url("/buckets/packs"),
        },
        Arc::new(map),
        http,
        recorder.clone(),
        clock.clone(),
    );
    Rig {
        fake,
        up,
        recorder,
        clock,
    }
}

async fn rig() -> Rig {
    rig_with(2).await
}

fn key() -> PackKey {
    PackKey(KEY.to_owned())
}

async fn sources(rig: &Rig) -> Vec<Source> {
    match rig.up.locate(&key()).await.unwrap() {
        Located::Found(location) => location.sources,
        other => panic!("not found: {other:?}"),
    }
}

fn request(source: &Source, offset: u64, len: u64) -> GetRequest {
    GetRequest {
        key: key(),
        source: source.clone(),
        range: ByteRange { offset, len },
        generation: Generation([9; 16]),
        class: Class::Prepare,
        job: JobId(1),
        rank: Rank(0),
    }
}

async fn drain(
    mut body: futures::stream::BoxStream<'static, Result<bytes::Bytes, UpstreamError>>,
) -> (Vec<u8>, Option<UpstreamError>) {
    let mut all = Vec::new();
    while let Some(piece) = body.next().await {
        match piece {
            Ok(p) => all.extend_from_slice(&p),
            Err(e) => return (all, Some(e)),
        }
    }
    (all, None)
}

fn cold(source: &Source) -> (&str, u64) {
    match &source.kind {
        SourceKind::Cold { shard, offset, .. } => (shard, *offset),
        other => panic!("not cold: {other:?}"),
    }
}

#[tokio::test]
async fn a_location_has_a_source_per_replica_of_each_chunk_in_order() {
    let rig = rig().await;
    let Located::Found(location) = rig.up.locate(&key()).await.unwrap() else {
        panic!("the object is there");
    };
    assert_eq!(location.size, Some(10));
    assert!(location.conflicts.is_empty());
    let extents: Vec<(u64, u64)> = location
        .sources
        .iter()
        .map(|s| (s.extent.offset, s.extent.len))
        .collect();
    assert_eq!(extents, [(0, 4), (0, 4), (4, 4), (4, 4), (8, 2), (8, 2)]);
    let chunks = rig.fake.chunks(BUCKET, KEY);
    for (i, s) in location.sources.iter().enumerate() {
        let (shard, offset) = cold(s);
        assert_eq!(shard, chunks[i / 2].fid);
        assert_eq!(offset, 0);
        let node = 2 + u8::try_from(i % 2).unwrap();
        assert_eq!(s.device.node(), NodeId(node));
        assert!(matches!(s.device, DeviceRef::Drive(..)));
    }
    assert_eq!(rig.fake.filer(0).count(Kind::FilerMetadata), 1);
}

#[tokio::test]
async fn the_generation_of_an_entry_without_a_tag_is_stable_for_its_key() {
    let rig = rig().await;
    let gen_of = |l: Located| match l {
        Located::Found(l) => l.generation,
        other => panic!("{other:?}"),
    };
    let a = gen_of(rig.up.locate(&key()).await.unwrap());
    let b = gen_of(rig.up.locate(&key()).await.unwrap());
    assert_eq!(a, b);
}

#[tokio::test]
async fn an_object_the_filer_lacks_is_a_definitive_miss() {
    let rig = rig().await;
    let got = rig.up.locate(&PackKey("cas/b/none".into())).await.unwrap();
    assert_eq!(got, Located::DefinitiveMiss);
}

#[tokio::test]
async fn a_chunk_whose_volume_no_server_listed_is_not_definitive() {
    // Only the first server is in the map, so volume 7 has one copy; with none, it is unknown.
    let rig = rig_with(0).await;
    let got = rig.up.locate(&key()).await.unwrap();
    assert_eq!(got, Located::NotDefinitive(NotDefinitive::TooFewAnswers));
    let rig = rig_with(1).await;
    assert_eq!(sources(&rig).await.len(), 3);
}

#[tokio::test]
async fn a_filer_that_fails_is_not_definitive_and_is_counted() {
    let rig = rig().await;
    rig.fake
        .filer(0)
        .fail_status(&format!("/buckets/{BUCKET}/{KEY}"), 500, None);
    let got = rig.up.locate(&key()).await.unwrap();
    assert_eq!(got, Located::NotDefinitive(NotDefinitive::TooFewAnswers));
    assert_eq!(rig.recorder.reports().get("locate_error"), Some(&1));
}

#[tokio::test]
async fn a_part_is_the_ranged_bytes_of_its_needle_and_is_timed() {
    let rig = rig().await;
    let all = sources(&rig).await;
    // The second chunk (offset 4, 4 bytes) on node 3; ask for object bytes 5..7.
    let source = &all[3];
    let (shard, _) = cold(source);
    let part = rig.up.get(request(source, 5, 2)).await.unwrap();
    assert_eq!(part.generation, Generation([9; 16]));
    assert_eq!(part.signals, DriveSignals::default());
    assert_eq!(rig.fake.volume(1).path_count(&format!("/{shard}")), 1);
    assert_eq!(rig.fake.volume(0).count(Kind::VolumeGet), 0);
    let mut body = part.body;
    rig.clock.advance_us(300);
    let first = body.next().await.unwrap().unwrap();
    rig.clock.advance_us(200);
    assert!(body.next().await.is_none());
    assert_eq!(first.as_ref(), b"56");
    assert_eq!(
        rig.recorder.parts(),
        vec![PartTiming {
            device: "n3d1".into(),
            class: "prepare".into(),
            len: 2,
            first_byte_us: 300,
            total_us: 500,
            ok: true,
        }]
    );
    assert_eq!(rig.recorder.bytes(), 2);
    // Polled again after its end, the body stays ended and is recorded once.
    assert!(body.next().await.is_none());
    assert_eq!(rig.recorder.parts().len(), 1);
}

#[tokio::test]
async fn a_part_that_is_not_there_is_not_found_and_recorded_as_failed() {
    let rig = rig().await;
    let mut source = sources(&rig).await[0].clone();
    source.kind = SourceKind::Cold {
        block: match &source.kind {
            SourceKind::Cold { block, .. } => *block,
            _ => unreachable!(),
        },
        shard: "7,000000ff".into(),
        offset: 0,
    };
    let err = rig.up.get(request(&source, 0, 4)).await.unwrap_err();
    assert_eq!(
        err,
        UpstreamError::NotFound {
            key: key(),
            device: source.device
        }
    );
    let parts = rig.recorder.parts();
    assert_eq!(parts.len(), 1);
    assert!(!parts[0].ok);
    assert_eq!(parts[0].len, 0);
}

#[tokio::test]
async fn status_503_is_unavailable_and_any_other_status_is_a_transport_error() {
    let rig = rig().await;
    let source = sources(&rig).await[0].clone();
    let (shard, _) = cold(&source);
    let path = format!("/{shard}");
    rig.fake.volume(0).fail_status(&path, 503, None);
    let err = rig.up.get(request(&source, 0, 4)).await.unwrap_err();
    assert_eq!(
        err,
        UpstreamError::Unavailable {
            key: key(),
            device: source.device,
            retry_after_ms: 1000,
            signals: DriveSignals::default(),
        }
    );
    rig.fake.volume(0).fail_status(&path, 500, None);
    let err = rig.up.get(request(&source, 0, 4)).await.unwrap_err();
    let UpstreamError::Transport { reason, .. } = err else {
        panic!("{err:?}");
    };
    assert!(reason.ends_with("status 500"), "{reason}");
    assert_eq!(rig.recorder.parts().iter().filter(|p| !p.ok).count(), 2);
}

#[tokio::test]
async fn a_connection_cut_mid_body_is_a_transport_error_and_keeps_the_bytes_that_came() {
    let rig = rig().await;
    let source = sources(&rig).await[0].clone();
    let (shard, _) = cold(&source);
    rig.fake.volume(0).cut_after(&format!("/{shard}"), 2);
    let part = rig.up.get(request(&source, 0, 4)).await.unwrap();
    let (bytes, err) = drain(part.body).await;
    assert_eq!(bytes, b"01");
    let Some(UpstreamError::Transport { reason, .. }) = err else {
        panic!("{err:?}");
    };
    assert!(reason.contains("body"), "{reason}");
    let parts = rig.recorder.parts();
    assert_eq!(parts.len(), 1);
    assert_eq!((parts[0].len, parts[0].ok), (2, false));
}

#[tokio::test]
async fn a_body_shorter_than_asked_is_a_transport_error() {
    let rig = rig().await;
    let source = sources(&rig).await[0].clone();
    // The needle has 4 bytes; 10 are asked, and the server answers the 4 it has.
    let part = rig.up.get(request(&source, 0, 10)).await.unwrap();
    let (bytes, err) = drain(part.body).await;
    assert_eq!(bytes, b"0123");
    let Some(UpstreamError::Transport { reason, .. }) = err else {
        panic!("{err:?}");
    };
    assert_eq!(reason, "the body had 4 bytes, 10 were asked");
    assert!(!rig.recorder.parts()[0].ok);
}

#[tokio::test]
async fn a_part_dropped_unread_is_recorded_as_failed_once() {
    let rig = rig().await;
    let source = sources(&rig).await[0].clone();
    let part = rig.up.get(request(&source, 0, 4)).await.unwrap();
    assert!(rig.recorder.parts().is_empty());
    drop(part);
    let parts = rig.recorder.parts();
    assert_eq!(parts.len(), 1);
    assert_eq!((parts[0].len, parts[0].ok), (0, false));
}

#[tokio::test]
async fn a_request_that_does_not_fit_the_location_is_a_transport_error_naming_why() {
    let rig = rig().await;
    let source = sources(&rig).await[2].clone(); // extent 4..8 on node 2
    let why = |err: UpstreamError| match err {
        UpstreamError::Transport { reason, .. } => reason,
        other => panic!("{other:?}"),
    };
    let err = rig.up.get(request(&source, 3, 2)).await.unwrap_err();
    assert_eq!(why(err), "the range starts before the source's extent");
    let err = rig.up.get(request(&source, u64::MAX, 5)).await.unwrap_err();
    assert_eq!(why(err), "the range runs past 2^64");
    let mut hot = source.clone();
    hot.kind = SourceKind::Hot {
        file: "f".into(),
        offset: 0,
    };
    let err = rig.up.get(request(&hot, 4, 2)).await.unwrap_err();
    assert_eq!(why(err), "the source is not a needle of a volume");
    let SourceKind::Cold { block, .. } = source.kind.clone() else {
        unreachable!()
    };
    let mut bad = source.clone();
    bad.kind = SourceKind::Cold {
        block,
        shard: "nocomma".into(),
        offset: 0,
    };
    let err = rig.up.get(request(&bad, 4, 2)).await.unwrap_err();
    assert_eq!(why(err), "file id \"nocomma\" is not <volume id>,<needle>");
    let mut elsewhere = source.clone();
    elsewhere.device = DeviceRef::Hot(NodeId(9));
    let err = rig.up.get(request(&elsewhere, 4, 2)).await.unwrap_err();
    assert!(why(err).starts_with("node 9 holds no copy of volume 7"));
    let err = rig.up.get(request(&source, 4, 0)).await.unwrap_err();
    assert!(why(err).ends_with("empty range 0..0"));
    // None of these reached a volume server.
    assert_eq!(rig.fake.total(Kind::VolumeGet), 0);
}

#[tokio::test]
async fn reports_are_counted_by_kind_and_nothing_is_sent() {
    let rig = rig().await;
    let source = sources(&rig).await[0].clone();
    let before = rig.fake.total(Kind::VolumeGet);
    rig.up
        .report(
            &key(),
            &source,
            ByteRange { offset: 0, len: 4 },
            Report::Bitrot,
        )
        .await;
    rig.up
        .report(
            &key(),
            &source,
            ByteRange { offset: 0, len: 4 },
            Report::Bitrot,
        )
        .await;
    assert_eq!(rig.recorder.reports().get("bitrot"), Some(&2));
    assert_eq!(rig.fake.total(Kind::VolumeGet), before);
}

#[tokio::test]
async fn the_upstream_can_be_printed() {
    let rig = rig().await;
    let text = format!("{:?}", rig.up);
    assert!(
        text.contains("SwfsUpstream") && text.contains("volumes: 1"),
        "{text}"
    );
}
