//! The S3 store's ranged read that carries `If-Match` ([`S3Store::get_stream_if_match`]) against
//! the loopback S3 service of `s3::fake`, which refuses a read of an object with another entity
//! tag as S3 does and keeps every read it took ([`FakeS3::reads`]).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use futures::StreamExt;
use objstore_s3::s3::fake::{FakeRead, FakeS3};
use objstore_s3::s3::{ObjectBody, S3Store};
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

fn range(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).unwrap()
}

/// The bytes of a body that ended without an error.
async fn whole(mut body: ObjectBody) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(piece) = body.next().await {
        bytes.extend_from_slice(&piece.unwrap());
    }
    bytes
}

/// A service holding `abcdefgh` under `k`, a store on it, and the object's entity tag.
async fn holding() -> (FakeS3, S3Store, String) {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("k");
    store
        .put_new(&k, Bytes::from("abcdefgh"), None)
        .await
        .unwrap();
    let etag = store.head(&k).await.unwrap().etag;
    (s3, store, etag)
}

/// Asserts that a ranged read with the object's own tag is answered with the bytes of the range,
/// the tag and the size of the whole object.
#[tokio::test]
async fn a_read_with_the_objects_tag_gives_the_bytes_of_the_range() {
    let (_s3, store, etag) = holding().await;
    let read = store
        .get_stream_if_match(&key("k"), range(2, 3), &etag)
        .await
        .unwrap();
    assert_eq!(read.etag.as_deref(), Some(etag.as_str()));
    assert_eq!(read.size, Some(8));
    assert_eq!(whole(read.body).await, b"cde");
}

/// Asserts that a ranged read with another tag is the store's 412, a `Service` error with that
/// status and the code `PreconditionFailed`, and gives no bytes.
#[tokio::test]
async fn a_read_with_another_tag_is_refused_with_412() {
    let (_s3, store, _) = holding().await;
    let refused = store
        .get_stream_if_match(&key("k"), range(0, 4), "\"other\"")
        .await;
    match refused.err().unwrap() {
        ObjectStoreError::Service { status, code, .. } => {
            assert_eq!((status, code.as_str()), (412, "PreconditionFailed"));
        }
        other => panic!("expected a 412, got {other:?}"),
    }
}

/// Asserts that a conditional read of a key that has no object is `NotFound`.
#[tokio::test]
async fn a_read_with_a_tag_of_a_key_with_no_object_is_not_found() {
    let (_s3, store, etag) = holding().await;
    let missing = store
        .get_stream_if_match(&key("gone"), range(0, 4), &etag)
        .await;
    let error = missing.err().unwrap();
    assert!(matches!(error, ObjectStoreError::NotFound(_)), "{error}");
}

/// Asserts that the service keeps each GET of an object as it came, in order: the plain ranged
/// read with its `Range` and no `If-Match`, the conditional one with both, the tag exactly as it
/// was sent, quotes included; and that HEADs and PUTs are not reads.
#[tokio::test]
async fn the_service_keeps_every_read_with_its_range_and_its_if_match() {
    let (s3, store, etag) = holding().await;
    let plain = store
        .get_stream(&key("k"), Some(range(0, 2)))
        .await
        .unwrap();
    assert_eq!(whole(plain.body).await, b"ab");
    let conditional = store.get_stream_if_match(&key("k"), range(4, 4), &etag);
    assert_eq!(whole(conditional.await.unwrap().body).await, b"efgh");
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    assert_eq!(
        s3.reads(),
        vec![
            FakeRead {
                key: "k".to_owned(),
                range: Some("bytes=0-1".to_owned()),
                if_match: None,
            },
            FakeRead {
                key: "k".to_owned(),
                range: Some("bytes=4-7".to_owned()),
                if_match: Some(etag.clone()),
            },
        ]
    );
}

/// The status line of the answer to a raw `GET` of `/tierstore-fake/k` with `If-Match: <etag>`
/// and no `Range`.
async fn whole_read_status(s3: &FakeS3, etag: &str) -> String {
    let addr = s3.endpoint().strip_prefix("http://").unwrap();
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let request =
        format!("GET /tierstore-fake/k HTTP/1.1\r\nhost: {addr}\r\nif-match: {etag}\r\n\r\n");
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !answer.windows(2).any(|w| w == b"\r\n") {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the service closed before its status line");
        answer.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&answer).into_owned();
    text.split("\r\n").next().unwrap().to_owned()
}

/// Asserts that a GET of the whole object with `If-Match` is 412 at the service for another tag
/// and 200 for the object's own, so the service refuses a stale whole read as it does a ranged
/// one.
#[tokio::test]
async fn a_whole_read_with_another_tag_is_refused_by_the_service() {
    let (s3, _store, etag) = holding().await;
    let refused = whole_read_status(&s3, "\"other\"").await;
    assert_eq!(refused, "HTTP/1.1 412 Precondition Failed");
    let served = whole_read_status(&s3, &etag).await;
    assert_eq!(served, "HTTP/1.1 200 OK");
}
