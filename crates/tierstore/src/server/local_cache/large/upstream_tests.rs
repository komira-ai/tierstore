//! The upstream of the read path over an S3 cold store ([`S3Upstream`]) against the loopback S3
//! service, which refuses a read with another entity tag (`412`) as S3 does and keeps every read
//! it took with its `Range` and its `If-Match`: what `LOCATE` finds, the condition every part
//! carries, and what each failure of the store is to the engine.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use objstore_s3::s3::fake::{Failure, FakeRead, FakeS3};
use objstore_s3::s3::{Addressing, Credentials, S3Config, S3Store};
use objstore_s3::{Capabilities, ObjectKey, ObjectStore as _};
use tierstore_fetch_api::ids::{ByteRange as PackRange, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::{
    DriveSignals, GetRequest, Located, Location, PartResponse, Report, Source, SourceKind,
    Upstream, UpstreamError,
};
use tierstore_format::ids::Generation;

use super::upstream::{RETRY_AFTER_MS, S3Upstream, STORE, generation_of};
use crate::S3Backend;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// A backend over the bucket of `s3`, signing with `key_id` and `secret`.
fn backend_as(s3: &FakeS3, key_id: &str, secret: &str) -> Arc<S3Backend> {
    let config = S3Config {
        endpoint: s3.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials: Credentials::new(key_id, secret),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        min_body_rate: None,
    };
    Arc::new(S3Backend::new(config).unwrap())
}

/// The upstream over the bucket of `s3`.
fn upstream(s3: &FakeS3) -> S3Upstream {
    S3Upstream::new(backend_as(s3, "test-key", "test-secret"))
}

fn object(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

fn pack(name: &str) -> PackKey {
    PackKey(name.to_owned())
}

/// A service holding `bytes` under `name`, a store on it, and the object's entity tag.
async fn holding(name: &str, bytes: &'static [u8]) -> (FakeS3, S3Store, String) {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let key = object(name);
    store
        .put_new(&key, Bytes::from_static(bytes), None)
        .await
        .unwrap();
    let etag = store.head(&key).await.unwrap().etag;
    (s3, store, etag)
}

/// The location `LOCATE` found, or a panic.
fn found(located: Located) -> Location {
    match located {
        Located::Found(location) => location,
        other => panic!("not found: {other:?}"),
    }
}

/// A part of `name` at `generation`: `len` bytes from `offset`.
fn part(name: &str, generation: Generation, offset: u64, len: u64) -> GetRequest {
    GetRequest {
        key: pack(name),
        source: Source {
            device: STORE,
            kind: SourceKind::Hot {
                file: "s3".to_owned(),
                offset: 0,
            },
            extent: PackRange { offset: 0, len: 8 },
        },
        range: PackRange { offset, len },
        generation,
        class: Class::Demand,
        job: JobId(1),
        rank: Rank(0),
    }
}

/// Every byte of a part, or its first error.
async fn bytes_of(response: PartResponse) -> Result<Vec<u8>, UpstreamError> {
    let mut body = response.body;
    let mut bytes = Vec::new();
    while let Some(piece) = body.next().await {
        bytes.extend_from_slice(&piece?);
    }
    Ok(bytes)
}

/// The HEADs the service took.
fn heads(s3: &FakeS3) -> usize {
    s3.requests()
        .iter()
        .filter(|request| request.starts_with("HEAD "))
        .count()
}

/// Asserts that `LOCATE` of a stored object names the generation its entity tag spells, its size,
/// and one source: the store's copy of the whole object.
#[tokio::test]
async fn locate_names_the_generation_of_the_entity_tag_and_one_whole_source() {
    let (s3, _store, etag) = holding("cas/a", b"abcdefgh").await;
    let location = found(upstream(&s3).locate(&pack("cas/a")).await.unwrap());
    let hex = etag.trim_matches('"');
    assert_eq!(hex.len(), 32);
    assert_eq!(location.generation.to_string(), hex);
    assert_eq!(location.size, Some(8));
    assert_eq!(
        location.sources,
        vec![Source {
            device: STORE,
            kind: SourceKind::Hot {
                file: "s3".to_owned(),
                offset: 0,
            },
            extent: PackRange { offset: 0, len: 8 },
        }]
    );
    assert!(location.conflicts.is_empty());
}

/// Asserts that `LOCATE` of a key the store holds no object under is a definitive miss.
#[tokio::test]
async fn locate_of_a_key_with_no_object_is_a_definitive_miss() {
    let s3 = FakeS3::start().await;
    let located = upstream(&s3).locate(&pack("cas/none")).await.unwrap();
    assert_eq!(located, Located::DefinitiveMiss);
}

/// Asserts that every part of an object carries `If-Match` with the tag `LOCATE` found, as the
/// store sent it, and the range of the part, and gives exactly the bytes of the range with the
/// generation of the answer; a second `LOCATE` of the same object changes nothing.
#[tokio::test]
async fn every_part_carries_if_match_with_the_tag_locate_found() {
    let (s3, _store, etag) = holding("cas/b", b"abcdefgh").await;
    let up = upstream(&s3);
    let g = found(up.locate(&pack("cas/b")).await.unwrap()).generation;
    assert_eq!(
        found(up.locate(&pack("cas/b")).await.unwrap()).generation,
        g
    );
    let first = up.get(part("cas/b", g, 0, 3)).await.unwrap();
    assert_eq!(first.generation, g);
    assert_eq!(first.signals, DriveSignals::default());
    assert_eq!(bytes_of(first).await.unwrap(), b"abc");
    let second = up.get(part("cas/b", g, 3, 5)).await.unwrap();
    assert_eq!(bytes_of(second).await.unwrap(), b"defgh");
    let read = |range: &str| FakeRead {
        key: "cas/b".to_owned(),
        range: Some(range.to_owned()),
        if_match: Some(etag.clone()),
    };
    assert_eq!(s3.reads(), vec![read("bytes=0-2"), read("bytes=3-7")]);
}

/// Asserts that a part of an object overwritten since `LOCATE` is refused as `412`, naming the
/// store, and that no byte of the new object is given.
#[tokio::test]
async fn a_part_of_an_object_overwritten_since_locate_is_precondition_failed() {
    let (s3, store, etag) = holding("cas/c", b"abcdefgh").await;
    let up = upstream(&s3);
    let g = found(up.locate(&pack("cas/c")).await.unwrap()).generation;
    let body = Bytes::from_static(b"ABCDEFGH");
    store
        .put_if_match(&object("cas/c"), body, &etag)
        .await
        .unwrap();
    let refused = up.get(part("cas/c", g, 0, 8)).await.unwrap_err();
    assert_eq!(
        refused,
        UpstreamError::PreconditionFailed {
            key: pack("cas/c"),
            device: STORE,
            actual: None,
        }
    );
}

/// Asserts that a part of an object deleted since `LOCATE` is not found at the store.
#[tokio::test]
async fn a_part_of_an_object_deleted_since_locate_is_not_found() {
    let (s3, store, etag) = holding("cas/d", b"abcdefgh").await;
    let up = upstream(&s3);
    let g = found(up.locate(&pack("cas/d")).await.unwrap()).generation;
    store
        .delete_if_match(&object("cas/d"), &etag)
        .await
        .unwrap();
    let refused = up.get(part("cas/d", g, 0, 8)).await.unwrap_err();
    assert_eq!(
        refused,
        UpstreamError::NotFound {
            key: pack("cas/d"),
            device: STORE,
        }
    );
}

/// Asserts that a part of a generation the upstream does not remember (an entry restored by the
/// journal after a restart) first asks HEAD, and is sent with the object's tag when the object is
/// still at that generation; and that one whose object moved to another generation is refused
/// as `412` naming the generation the store holds, with no GET sent.
#[tokio::test]
async fn a_part_of_a_generation_not_remembered_asks_head_first() {
    let (s3, store, etag) = holding("cas/e", b"abcdefgh").await;
    let g = generation_of(&etag);
    let fresh = upstream(&s3);
    let served = fresh.get(part("cas/e", g, 1, 2)).await.unwrap();
    assert_eq!(bytes_of(served).await.unwrap(), b"bc");
    assert_eq!(heads(&s3), 2);
    let moved = store
        .put_if_match(&object("cas/e"), Bytes::from_static(b"12345678"), &etag)
        .await
        .unwrap();
    let reads = s3.reads().len();
    let refused = upstream(&s3).get(part("cas/e", g, 0, 8)).await.unwrap_err();
    assert_eq!(
        refused,
        UpstreamError::PreconditionFailed {
            key: pack("cas/e"),
            device: STORE,
            actual: Some(generation_of(&moved)),
        }
    );
    assert_eq!(s3.reads().len(), reads);
}

/// Asserts that the tags of at most the limit of objects are remembered, the oldest forgotten
/// first: with room for one, a part of the first object located asks HEAD again, and one of the
/// last does not.
#[tokio::test]
async fn the_oldest_tag_is_forgotten_past_the_limit() {
    let (s3, store, _) = holding("cas/f", b"abcdefgh").await;
    let other = Bytes::from_static(b"other!!!");
    store.put_new(&object("cas/g"), other, None).await.unwrap();
    let up = S3Upstream::with_limit(backend_as(&s3, "test-key", "test-secret"), 1);
    let f = found(up.locate(&pack("cas/f")).await.unwrap()).generation;
    let g = found(up.locate(&pack("cas/g")).await.unwrap()).generation;
    let before = heads(&s3);
    bytes_of(up.get(part("cas/g", g, 0, 8)).await.unwrap())
        .await
        .unwrap();
    assert_eq!(heads(&s3), before);
    bytes_of(up.get(part("cas/f", f, 0, 8)).await.unwrap())
        .await
        .unwrap();
    assert_eq!(heads(&s3), before + 1);
}

/// Asserts that a store that answers `500` is unavailable to the engine, to be asked again
/// after [`RETRY_AFTER_MS`].
#[tokio::test]
async fn an_answer_of_the_5xx_class_is_unavailable() {
    let (s3, _store, _) = holding("cas/h", b"abcdefgh").await;
    s3.fail(Failure::Head(1));
    let refused = upstream(&s3).locate(&pack("cas/h")).await.unwrap_err();
    assert_eq!(
        refused,
        UpstreamError::Unavailable {
            key: pack("cas/h"),
            device: STORE,
            retry_after_ms: RETRY_AFTER_MS,
            signals: DriveSignals::default(),
        }
    );
}

/// Asserts that a store that refuses the request (a key it does not take: 403) is a failure of
/// the transport that carries the store's text.
#[tokio::test]
async fn a_refused_request_is_a_transport_failure_with_the_stores_text() {
    let (s3, _store, _) = holding("cas/i", b"abcdefgh").await;
    s3.require_credentials("test-key", "test-secret");
    let up = S3Upstream::new(backend_as(&s3, "other-key", "other-secret"));
    let refused = up.locate(&pack("cas/i")).await.unwrap_err();
    let UpstreamError::Transport {
        key,
        device,
        reason,
    } = refused
    else {
        panic!("not a transport failure: {refused:?}");
    };
    assert_eq!((key, device), (pack("cas/i"), STORE));
    assert!(reason.contains("403"), "{reason}");
}

/// Asserts that a key the store's client cannot send (a `..` segment) is a failure of the
/// transport for `LOCATE` and for a part, and that a part of no bytes is one too, with nothing
/// sent to the store.
#[tokio::test]
async fn a_key_or_a_range_the_store_cannot_be_sent_is_a_transport_failure() {
    let (s3, _store, etag) = holding("cas/j", b"abcdefgh").await;
    let up = upstream(&s3);
    let bad = up.locate(&pack("cas/../j")).await.unwrap_err();
    assert!(matches!(bad, UpstreamError::Transport { .. }), "{bad:?}");
    let g = found(up.locate(&pack("cas/j")).await.unwrap()).generation;
    let requests = s3.requests().len();
    let empty = up.get(part("cas/j", g, 0, 0)).await.unwrap_err();
    assert!(
        matches!(empty, UpstreamError::Transport { .. }),
        "{empty:?}"
    );
    let bad_part = up
        .get(part("cas/../j", generation_of(&etag), 0, 8))
        .await
        .unwrap_err();
    assert!(
        matches!(bad_part, UpstreamError::Transport { .. }),
        "{bad_part:?}"
    );
    assert_eq!(s3.requests().len(), requests);
}

/// Asserts that a tag of 32 hex digits is read as the 16 bytes it spells, with or without its
/// quotes, and that any other tag (a multipart upload's) is named by the first 16 bytes of the
/// SHA-256 of its text: two such tags name two generations, and a tag names one generation.
#[test]
fn a_generation_is_the_hex_of_the_tag_or_the_hash_of_its_text() {
    let hex = "0123456789abcdef0123456789abcdef";
    assert_eq!(generation_of(&format!("\"{hex}\"")).to_string(), hex);
    assert_eq!(generation_of(hex).to_string(), hex);
    let parts = generation_of("\"0123456789abcdef0123456789abcdef-3\"");
    assert_eq!(parts.to_string(), "bef743256b772bd6e5d1bbc6827eccab");
    assert_ne!(parts, generation_of("0123456789abcdef0123456789abcdef-4"));
    assert_eq!(parts, generation_of("0123456789abcdef0123456789abcdef-3"));
}

/// Asserts that a report of a bad cell sends nothing to the store.
#[tokio::test]
async fn a_report_sends_nothing_to_the_store() {
    let s3 = FakeS3::start().await;
    let up = upstream(&s3);
    let source = part("cas/k", Generation([1; 16]), 0, 1).source;
    let range = PackRange { offset: 0, len: 1 };
    up.report(&pack("cas/k"), &source, range, Report::Bitrot)
        .await;
    assert!(s3.requests().is_empty());
}
