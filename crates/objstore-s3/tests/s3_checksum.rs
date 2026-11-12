//! The create-only write that names the SHA-256 of its body, and the HEAD that reads the digest
//! back: against the loopback S3 service of `s3::fake` (what is stored and refused) and against a
//! scripted HTTP server (what each request carries, and how each answer is read).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use futures::StreamExt as _;
use objstore_s3::s3::fake::{Failure, FakeS3};
use objstore_s3::s3::{Credentials, S3Config, S3Store, S3Tuning, StoredSha256};
use objstore_s3::{BodyStream, Capabilities, ObjectKey, ObjectStore as _, ObjectStoreError};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// The size of the multipart test's body: one part of 5 MiB and one of 1 MiB.
const SIX_MIB: usize = 6 << 20;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// `bytes` as a body of one chunk.
fn body(bytes: &[u8]) -> BodyStream {
    let chunk = Bytes::copy_from_slice(bytes);
    futures::stream::once(async move { Ok(chunk) }).boxed()
}

/// `len` bytes that are not all zeros, so the fake keeps them as they are.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// The status and code of a `Service` error.
fn service(error: &ObjectStoreError) -> (u16, &str) {
    match error {
        ObjectStoreError::Service { status, code, .. } => (*status, code.as_str()),
        other => panic!("expected Service, got {other:?}"),
    }
}

/// A store that sends any body longer than 1 MiB as a multipart upload of 5 MiB parts.
fn multipart_store(s3: &FakeS3) -> S3Store {
    let tuning = S3Tuning {
        single_put_max: 1 << 20,
        part_bytes: 5 << 20,
        ..S3Tuning::default()
    };
    s3.store_tuned(CONDITIONAL, tuning)
}

/// Asserts that a create-only PUT naming the SHA-256 of its body makes the object, and that a
/// HEAD of it reads back its size, the store's checksum of the whole object and the digest its
/// metadata records, all of them the digest of the bytes.
#[tokio::test]
async fn a_put_that_names_its_digest_is_stored_with_it_and_read_back() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = b"hello world";
    let digest = sha256(bytes);
    let made = store
        .put_stream_new_sha256(&key("a"), 11, body(bytes), &digest)
        .await
        .unwrap();
    assert!(made.etag.is_some(), "the answer carries the ETag");
    let stored = store.head_sha256(&key("a")).await.unwrap();
    let want = StoredSha256 {
        size: 11,
        checksum: Some(digest),
        recorded: Some(digest),
    };
    assert_eq!(stored, want);
}

/// Asserts that a body whose bytes are not the SHA-256 the PUT names is refused by the store
/// (400 `BadDigest`) and leaves no object.
#[tokio::test]
async fn a_body_that_is_not_its_digest_is_refused_and_leaves_nothing() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let refused = store
        .put_stream_new_sha256(&key("a"), 3, body(b"abc"), &sha256(b"abd"))
        .await
        .unwrap_err();
    assert_eq!(service(&refused), (400, "BadDigest"));
    assert_eq!(s3.object_len(&key("a")), None);
}

/// Asserts that the write is create-only: a second one of a key that exists is `AlreadyExists`,
/// even with the same bytes, and the object stays as the first write made it.
#[tokio::test]
async fn a_second_put_of_a_key_is_refused_as_existing() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let digest = sha256(b"one");
    let k = key("a");
    store
        .put_stream_new_sha256(&k, 3, body(b"one"), &digest)
        .await
        .unwrap();
    let again = store
        .put_stream_new_sha256(&k, 3, body(b"one"), &digest)
        .await
        .unwrap_err();
    assert!(
        matches!(again, ObjectStoreError::AlreadyExists(_)),
        "{again}"
    );
    let other = store
        .put_stream_new_sha256(&key("a"), 3, body(b"two"), &sha256(b"two"))
        .await
        .unwrap_err();
    assert!(
        matches!(other, ObjectStoreError::AlreadyExists(_)),
        "{other}"
    );
    assert_eq!(store.get_all(&key("a")).await.unwrap(), "one");
}

/// Asserts that a store that does not claim conditional writes is refused the call before
/// anything is sent: without `If-None-Match` the PUT would replace an object.
#[tokio::test]
async fn a_store_without_conditional_writes_sends_nothing() {
    let s3 = FakeS3::start().await;
    let store = s3.store(Capabilities::default());
    let refused = store
        .put_stream_new_sha256(&key("a"), 3, body(b"abc"), &sha256(b"abc"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, ObjectStoreError::Unsupported(_)),
        "{refused}"
    );
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that a body longer than one PUT is a multipart upload whose every part the store
/// checks against its checksum, that the object records the digest of the whole body in its
/// metadata, and that the store's own checksum, which is of the parts (`COMPOSITE`), is not
/// read as the digest of the object.
#[tokio::test]
async fn a_multipart_put_checksums_its_parts_and_records_the_digest() {
    let s3 = FakeS3::start().await;
    let store = multipart_store(&s3);
    let bytes = pattern(SIX_MIB);
    let digest = sha256(&bytes);
    store
        .put_stream_new_sha256(&key("big"), SIX_MIB as u64, body(&bytes), &digest)
        .await
        .unwrap();
    let stats = s3.stats();
    assert_eq!((stats.parts, stats.completed), (2, 1));
    let stored = store.head_sha256(&key("big")).await.unwrap();
    let want = StoredSha256 {
        size: SIX_MIB as u64,
        checksum: None,
        recorded: Some(digest),
    };
    assert_eq!(stored, want);
    assert_eq!(store.get_all(&key("big")).await.unwrap(), bytes);
}

/// Asserts that a multipart write of a key that exists is refused at the completion as
/// `AlreadyExists`, and the upload it began is aborted, not left open.
#[tokio::test]
async fn a_multipart_put_of_a_key_that_exists_is_refused_and_aborted() {
    let s3 = FakeS3::start().await;
    let store = multipart_store(&s3);
    let bytes = pattern(SIX_MIB);
    let digest = sha256(&bytes);
    let (k, len) = (key("big"), SIX_MIB as u64);
    store
        .put_stream_new_sha256(&k, len, body(&bytes), &digest)
        .await
        .unwrap();
    let again = store
        .put_stream_new_sha256(&k, len, body(&bytes), &digest)
        .await
        .unwrap_err();
    assert!(
        matches!(again, ObjectStoreError::AlreadyExists(_)),
        "{again}"
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.stats().aborted, 1);
}

/// Asserts what a store that does no checksums does with the write: it takes a body that is not
/// the digest named (so a caller must check the body itself), keeps no checksum, and keeps the
/// metadata, which is then the only record of the digest.
#[tokio::test]
async fn a_store_that_ignores_checksums_keeps_only_the_recorded_digest() {
    let s3 = FakeS3::start().await;
    s3.ignore_checksums();
    let store = s3.store(CONDITIONAL);
    let claimed = sha256(b"abd");
    store
        .put_stream_new_sha256(&key("a"), 3, body(b"abc"), &claimed)
        .await
        .unwrap();
    let stored = store.head_sha256(&key("a")).await.unwrap();
    let want = StoredSha256 {
        size: 3,
        checksum: None,
        recorded: Some(claimed),
    };
    assert_eq!(stored, want);
}

/// Asserts that a HEAD of a key with no object is `NotFound`, that a failure of the store is
/// its `Service` error, and that an object written without a digest has neither a checksum nor
/// a recorded digest.
#[tokio::test]
async fn a_head_of_no_object_a_failure_and_an_object_without_a_digest() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let missing = store.head_sha256(&key("none")).await.unwrap_err();
    assert!(
        matches!(missing, ObjectStoreError::NotFound(_)),
        "{missing}"
    );
    store
        .put_new(&key("plain"), Bytes::from_static(b"abc"), None)
        .await
        .unwrap();
    s3.fail(Failure::Head(1));
    let failed = store.head_sha256(&key("plain")).await.unwrap_err();
    assert_eq!(service(&failed).0, 500);
    let stored = store.head_sha256(&key("plain")).await.unwrap();
    let want = StoredSha256 {
        size: 3,
        checksum: None,
        recorded: None,
    };
    assert_eq!(stored, want);
}

/// Serves `responses` in order, one per connection, and records each request head.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for resp in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let body_len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + body_len {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            log.lock().unwrap().push(head);
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
        }
    });
    (endpoint, seen)
}

/// An answer with exactly `headers` and no body.
fn bare(status: &str, headers: &[&str]) -> String {
    let mut s = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s
}

fn wire_store(endpoint: String) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: CONDITIONAL,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    })
    .unwrap()
}

/// Asserts that the PUT carries `If-None-Match: *`, the base64 checksum and the hex digest as
/// metadata, all three among the signed headers, and that the HEAD asks for the checksum.
#[tokio::test]
async fn the_put_and_the_head_carry_the_digest_and_ask_for_it() {
    let digest = sha256(b"abc");
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    let made = bare("200 OK", &["content-length: 0", "etag: \"e\""]);
    let headed = bare(
        "200 OK",
        &[
            "content-length: 3",
            &format!("x-amz-checksum-sha256: {b64}"),
        ],
    );
    let (endpoint, seen) = script(vec![made, headed]).await;
    let store = wire_store(endpoint);
    store
        .put_stream_new_sha256(&key("a"), 3, body(b"abc"), &digest)
        .await
        .unwrap();
    let stored = store.head_sha256(&key("a")).await.unwrap();
    assert_eq!(
        stored.checksum,
        Some(digest),
        "no type stated is a whole-object checksum"
    );
    let heads = seen.lock().unwrap();
    let put = heads[0].to_ascii_lowercase();
    assert!(put.contains("\r\nif-none-match: *\r\n"), "{put}");
    let sum = format!(
        "\r\nx-amz-checksum-sha256: {}\r\n",
        b64.to_ascii_lowercase()
    );
    assert!(put.contains(&sum), "{put}");
    let meta = format!("\r\nx-amz-meta-sha256: {}\r\n", hex::encode(digest));
    assert!(put.contains(&meta), "{put}");
    assert!(
        put.contains("x-amz-checksum-sha256;x-amz-content-sha256;x-amz-date;x-amz-meta-sha256"),
        "the checksum and the metadata are signed: {put}"
    );
    let head = heads[1].to_ascii_lowercase();
    assert!(head.starts_with("head "), "{head}");
    assert!(
        head.contains("\r\nx-amz-checksum-mode: enabled\r\n"),
        "{head}"
    );
}

/// Asserts that a checksum the store states is of the parts (`COMPOSITE`) is not read as the
/// digest of the object, while the recorded digest is, and that an answer with no size is a
/// protocol error.
#[tokio::test]
async fn a_composite_checksum_is_not_the_digest_and_a_head_needs_a_size() {
    let digest = [9u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    let composite = bare(
        "200 OK",
        &[
            "content-length: 9",
            &format!("x-amz-checksum-sha256: {b64}"),
            "x-amz-checksum-type: COMPOSITE",
            &format!("x-amz-meta-sha256: {}", hex::encode(digest)),
        ],
    );
    let sizeless = bare("200 OK", &["etag: \"e\""]);
    let (endpoint, _seen) = script(vec![composite, sizeless]).await;
    let store = wire_store(endpoint);
    let stored = store.head_sha256(&key("a")).await.unwrap();
    let want = StoredSha256 {
        size: 9,
        checksum: None,
        recorded: Some(digest),
    };
    assert_eq!(stored, want);
    let bad = store.head_sha256(&key("a")).await.unwrap_err();
    assert!(matches!(bad, ObjectStoreError::Protocol(_)), "{bad}");
}

/// Asserts that a HEAD whose first connection closes without an answer is sent again and read,
/// that a checksum type in lower case is a whole-object checksum, and that a size that is not a
/// number is a protocol error.
#[tokio::test]
async fn a_head_is_sent_again_after_a_lost_answer_and_reads_its_fields_strictly() {
    let digest = [5u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    let lower = bare(
        "200 OK",
        &[
            "content-length: 4",
            &format!("x-amz-checksum-sha256: {b64}"),
            "x-amz-checksum-type: full_object",
        ],
    );
    let garbled = bare("200 OK", &["content-length: abc"]);
    let (endpoint, seen) = script(vec![String::new(), lower, garbled]).await;
    let store = wire_store(endpoint);
    let stored = store.head_sha256(&key("a")).await.unwrap();
    assert_eq!(stored.checksum, Some(digest));
    assert_eq!(seen.lock().unwrap().len(), 2, "the HEAD was sent twice");
    let bad = store.head_sha256(&key("a")).await.unwrap_err();
    assert!(matches!(bad, ObjectStoreError::Protocol(_)), "{bad}");
}
