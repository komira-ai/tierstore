//! The write of an object under a key bound to its content (`put_stream_content_bound`): against
//! the loopback S3 service of `s3::fake`, made to keep a PUT cut short as SeaweedFS 4.54 can (what
//! is kept, refused, replaced and left), and against a scripted HTTP server (what the replacing
//! write carries, and what an answer that loses a race to another writer gives).

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
use objstore_s3::{
    BodyStream, Capabilities, ContentBoundPut, ObjectKey, ObjectStore as _, ObjectStoreError,
};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// The size of a multipart test's body: one part of 5 MiB and one of 1 MiB.
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

/// A body that gives `pieces` and then fails, as the client of an upload that went away: each
/// item waits a turn of the runtime first, so the HTTP client sends what it has (the head, and
/// every piece but the one it holds back) before the failure ends the request.
fn cut(pieces: &[&'static [u8]]) -> BodyStream {
    let mut items: Vec<Result<Bytes, ObjectStoreError>> = pieces
        .iter()
        .map(|piece| Ok(Bytes::from_static(piece)))
        .collect();
    items.push(Err(ObjectStoreError::Body("the client went away".into())));
    futures::stream::iter(items)
        .then(|item| async move {
            tokio::task::yield_now().await;
            item
        })
        .boxed()
}

/// A body that fails if it is ever read: for a write that must send nothing.
fn unread() -> BodyStream {
    futures::stream::once(async { Err(ObjectStoreError::Body("the body was read".into())) }).boxed()
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

/// How many requests the service took with this method, of any key.
fn taken(s3: &FakeS3, method: &str) -> usize {
    let prefix = format!("{method} ");
    s3.requests()
        .iter()
        .filter(|line| line.starts_with(&prefix))
        .count()
}

/// Makes `k` an object of the bytes that came of a PUT cut short before its first byte, as
/// SeaweedFS 4.54 keeps one: 0 bytes, no checksum, no recorded digest.
async fn cut_to_nothing(s3: &FakeS3, store: &S3Store, k: &ObjectKey, len: u64) {
    s3.keep_cut_bodies();
    let failed = store.put_stream(k, len, cut(&[])).await.unwrap_err();
    assert!(matches!(failed, ObjectStoreError::Body(_)), "{failed}");
    s3.wait_for(|s| s.object_len(k).is_some()).await;
    assert_eq!(s3.object_len(k), Some(0));
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

/// Asserts that a store that keeps the bytes of a PUT cut short keeps them for a write that names
/// no digest (a 0-byte object for a body cut before its first byte, a short one for a body cut
/// after some), and refuses them for the content-bound write, whose PUT carries the digest: no
/// object is made.
/// Catches: a content-bound write whose PUT carries no checksum of the body, so a store that does
/// not take a request only whole keeps a truncated object under the key.
#[tokio::test]
async fn a_cut_put_is_kept_without_the_digest_and_refused_with_it() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    cut_to_nothing(&s3, &store, &key("plain-empty"), 10).await;

    let short = key("plain-short");
    let failed = store.put_stream(&short, 10, cut(&[b"ab", b"cd"])).await;
    assert!(
        matches!(failed, Err(ObjectStoreError::Body(_))),
        "{failed:?}"
    );
    s3.wait_for(|s| s.object_len(&short).is_some()).await;
    let kept = s3.object_len(&short).unwrap();
    assert!(kept < 10, "the object is the bytes that came: {kept}");

    let digest = sha256(&pattern(10));
    for (name, pieces) in [
        ("bound-empty", &[][..]),
        ("bound-short", &[&b"ab"[..], b"cd"]),
    ] {
        let k = key(name);
        let cuts = s3.cut_requests();
        let failed = store.put_stream_content_bound(&k, 10, cut(pieces), digest);
        let failed = failed.await.unwrap_err();
        assert!(matches!(failed, ObjectStoreError::Body(_)), "{failed}");
        s3.wait_for(|s| s.cut_requests() == cuts + 1).await;
        assert_eq!(s3.object_len(&k), None, "{name}: nothing is kept");
    }
}

/// Asserts that a part of a multipart upload cut short is never taken, even by a service that
/// keeps a PUT of a whole object cut short: the request is counted as cut, and the upload holds no
/// part.
#[tokio::test]
async fn a_cut_part_is_never_taken() {
    let s3 = FakeS3::start().await;
    s3.keep_cut_bodies();
    let store = s3.store(CONDITIONAL);
    let k = key("parts");
    let id = store.create_multipart(&k).await.unwrap();
    let failed = store.upload_part_stream(&k, &id, 1, 10, cut(&[b"ab", b"cd"]));
    assert!(matches!(failed.await, Err(ObjectStoreError::Body(_))));
    s3.wait_for(|s| s.cut_requests() == 1).await;
    assert_eq!(s3.stats().parts, 0);
    assert_eq!(s3.incomplete_uploads(), 1);
}

/// Asserts that by default the service takes nothing of a PUT cut short, as a store that takes a
/// request only whole: the request is counted as cut and no object is made.
#[tokio::test]
async fn a_cut_put_reaches_nothing_by_default() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("plain");
    let failed = store.put_stream(&k, 10, cut(&[b"ab", b"cd"])).await;
    assert!(
        matches!(failed, Err(ObjectStoreError::Body(_))),
        "{failed:?}"
    );
    s3.wait_for(|s| s.cut_requests() == 1).await;
    assert_eq!(s3.object_len(&k), None);
    assert_eq!(s3.stats().single_puts, 0);
}

/// Asserts that a key with no object is given one by a create-only PUT that names the digest: it
/// is `Created`, after one HEAD, and the store keeps the digest as the checksum and the metadata.
#[tokio::test]
async fn a_key_with_no_object_is_created_with_its_digest() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(100);
    let digest = sha256(&bytes);
    let made = store
        .put_stream_content_bound(&key("a"), 100, body(&bytes), digest)
        .await;
    assert_eq!(made.unwrap(), ContentBoundPut::Created);
    assert_eq!((taken(&s3, "HEAD"), taken(&s3, "PUT")), (1, 1));
    let stored = store.head_sha256(&key("a")).await.unwrap();
    let want = StoredSha256 {
        size: 100,
        checksum: Some(digest),
        recorded: Some(digest),
    };
    assert_eq!(stored, want);
}

/// Asserts that a 0-byte object under the key (what a PUT cut short leaves) is replaced once by
/// the object written, which then has the bytes and the digest, and that the next write finds it
/// whole and sends nothing.
/// Catches: a write that answers a wrong-size object with a refusal for ever instead of replacing
/// it, and one that replaces a whole object again on every write.
#[tokio::test]
async fn a_wrong_size_object_is_replaced_once() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("cas/l/blob");
    let bytes = pattern(1000);
    let digest = sha256(&bytes);
    cut_to_nothing(&s3, &store, &k, 1000).await;

    let replaced = store.put_stream_content_bound(&k, 1000, body(&bytes), digest);
    assert_eq!(
        replaced.await.unwrap(),
        ContentBoundPut::Replaced { size: 0 }
    );
    assert_eq!(store.get_all(&k).await.unwrap(), bytes);
    let stored = store.head_sha256(&k).await.unwrap();
    assert_eq!(
        (stored.size, stored.checksum, stored.recorded),
        (1000, Some(digest), Some(digest))
    );

    let (puts, singles) = (taken(&s3, "PUT"), s3.stats().single_puts);
    let again = store.put_stream_content_bound(&k, 1000, unread(), digest);
    assert_eq!(again.await.unwrap(), ContentBoundPut::Existed);
    assert_eq!((taken(&s3, "PUT"), s3.stats().single_puts), (puts, singles));
}

/// Asserts that a whole object is left as it is and its body never read, both one written with no
/// digest (only its length can be judged) and one whose stored checksum is the digest.
/// Catches: a write that replaces an object that is whole, or reads the client's body to find out.
#[tokio::test]
async fn a_whole_object_is_untouched() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(64);
    let digest = sha256(&bytes);
    store
        .put_stream(&key("plain"), 64, body(&bytes))
        .await
        .unwrap();
    store
        .put_stream_new_sha256(&key("named"), 64, body(&bytes), &digest)
        .await
        .unwrap();
    for name in ["plain", "named"] {
        let k = key(name);
        let before = store.head(&k).await.unwrap();
        let puts = taken(&s3, "PUT");
        let kept = store.put_stream_content_bound(&k, 64, unread(), digest);
        assert_eq!(kept.await.unwrap(), ContentBoundPut::Existed, "{name}");
        assert_eq!(taken(&s3, "PUT"), puts, "{name}: nothing is sent");
        assert_eq!(store.head(&k).await.unwrap(), before, "{name}");
    }
}

/// Asserts that an object of the right length whose stored checksum is another digest is damaged
/// and replaced, and so is one whose recorded digest is another on a store that keeps no
/// checksum.
#[tokio::test]
async fn an_object_naming_another_digest_is_replaced() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("other");
    store
        .put_stream_new_sha256(&k, 3, body(b"abd"), &sha256(b"abd"))
        .await
        .unwrap();
    let replaced = store.put_stream_content_bound(&k, 3, body(b"abc"), sha256(b"abc"));
    assert_eq!(
        replaced.await.unwrap(),
        ContentBoundPut::Replaced { size: 3 }
    );
    assert_eq!(store.get_all(&k).await.unwrap(), "abc");

    let plain = FakeS3::start().await;
    plain.ignore_checksums();
    let store = plain.store(CONDITIONAL);
    store
        .put_stream_new_sha256(&k, 3, body(b"abd"), &sha256(b"abd"))
        .await
        .unwrap();
    let replaced = store.put_stream_content_bound(&k, 3, body(b"abc"), sha256(b"abc"));
    assert_eq!(
        replaced.await.unwrap(),
        ContentBoundPut::Replaced { size: 3 }
    );
    let stored = store.head_sha256(&k).await.unwrap();
    assert_eq!(stored.recorded, Some(sha256(b"abc")));
}

/// Asserts that an object longer than one PUT replaces a damaged one by a multipart upload: one
/// upload completed, the object is the bytes and the digest is recorded; and that the next write
/// finds it whole and sends nothing, though the store keeps for it only a checksum of its parts
/// (`COMPOSITE`), so the recorded digest is what judges it.
/// Catches: a large object replaced again on every write because its composite checksum is taken
/// for another digest.
#[tokio::test]
async fn a_damaged_object_is_replaced_by_a_multipart_upload() {
    let s3 = FakeS3::start().await;
    let store = multipart_store(&s3);
    let k = key("large");
    let bytes = pattern(SIX_MIB);
    let digest = sha256(&bytes);
    cut_to_nothing(&s3, &s3.store(CONDITIONAL), &k, SIX_MIB as u64).await;
    let replaced = store.put_stream_content_bound(&k, SIX_MIB as u64, body(&bytes), digest);
    assert_eq!(
        replaced.await.unwrap(),
        ContentBoundPut::Replaced { size: 0 }
    );
    assert_eq!(s3.stats().completed, 1);
    assert_eq!(s3.object_len(&k), Some(SIX_MIB as u64));
    assert_eq!(store.head_sha256(&k).await.unwrap().recorded, Some(digest));

    let (puts, posts) = (taken(&s3, "PUT"), taken(&s3, "POST"));
    let again = store
        .put_stream_content_bound(&k, SIX_MIB as u64, unread(), digest)
        .await;
    assert_eq!(again.unwrap(), ContentBoundPut::Existed);
    assert_eq!((taken(&s3, "PUT"), taken(&s3, "POST")), (puts, posts));
    assert_eq!(s3.stats().completed, 1);
}

/// Asserts that the write is refused before anything is sent on a store that does not claim
/// conditional writes, and that a HEAD that fails fails the write with nothing written.
#[tokio::test]
async fn no_conditional_writes_or_a_failed_head_writes_nothing() {
    let s3 = FakeS3::start().await;
    let plain = s3.store(Capabilities::default());
    let refused = plain
        .put_stream_content_bound(&key("a"), 3, unread(), sha256(b"abc"))
        .await;
    assert!(matches!(
        refused.unwrap_err(),
        ObjectStoreError::Unsupported(_)
    ));
    assert_eq!(s3.requests(), Vec::<String>::new());

    let store = s3.store(CONDITIONAL);
    s3.fail(Failure::Head(1));
    let failed = store
        .put_stream_content_bound(&key("a"), 3, unread(), sha256(b"abc"))
        .await;
    assert_eq!(service(&failed.unwrap_err()).0, 500);
    assert_eq!(taken(&s3, "PUT"), 0);
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

/// An S3 error answer with `code`.
fn refusal(status: &str, code: &str) -> String {
    let body = format!("<Error><Code>{code}</Code><Message>{code}</Message></Error>");
    format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// The HEAD answer for a 0-byte object with the empty MD5 as its tag, as SeaweedFS gives one.
fn empty_head() -> String {
    bare(
        "200 OK",
        &[
            "content-length: 0",
            "etag: \"d41d8cd98f00b204e9800998ecf8427e\"",
        ],
    )
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

/// Asserts that the HEAD asks for the stored checksum, and that the write replacing a damaged
/// object is a PUT that carries `If-Match` on the tag the HEAD gave, the base64 checksum and the
/// hex digest as metadata, all signed, and no `If-None-Match`.
#[tokio::test]
async fn the_replacing_put_is_conditional_on_the_judged_tag_and_names_the_digest() {
    let digest = sha256(b"abc");
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    let made = bare("200 OK", &["content-length: 0", "etag: \"new\""]);
    let (endpoint, seen) = script(vec![empty_head(), made]).await;
    let store = wire_store(endpoint);
    let replaced = store
        .put_stream_content_bound(&key("k"), 3, body(b"abc"), digest)
        .await;
    assert_eq!(replaced.unwrap(), ContentBoundPut::Replaced { size: 0 });
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    let head = seen[0].to_ascii_lowercase();
    assert!(head.starts_with("head /test-bucket/k "), "{head}");
    assert!(head.contains("x-amz-checksum-mode: enabled\r\n"), "{head}");
    let put = seen[1].to_ascii_lowercase();
    assert!(put.starts_with("put /test-bucket/k "), "{put}");
    assert!(
        put.contains("if-match: \"d41d8cd98f00b204e9800998ecf8427e\"\r\n"),
        "{put}"
    );
    assert!(!put.contains("if-none-match"), "{put}");
    let b64 = b64.to_ascii_lowercase();
    assert!(
        put.contains(&format!("x-amz-checksum-sha256: {b64}\r\n")),
        "{put}"
    );
    let hex = hex::encode(digest);
    assert!(
        put.contains(&format!("x-amz-meta-sha256: {hex}\r\n")),
        "{put}"
    );
    for name in ["if-match", "x-amz-checksum-sha256", "x-amz-meta-sha256"] {
        let signed = put.split("signedheaders=").nth(1).unwrap();
        let signed = signed.split(',').next().unwrap();
        assert!(signed.split(';').any(|n| n == name), "{name} in {signed}");
    }
}

/// Asserts what a write gives when another writer changed the key between its HEAD and its PUT:
/// the damaged object replaced first (412 on `If-Match`) is that store error, one deleted first
/// (404) is `NotFound`, and a key given an object first (412 on the create-only PUT) is `Existed`,
/// as a create-only write that finds the key taken is.
#[tokio::test]
async fn a_write_that_loses_a_race_to_another_writer_says_so() {
    let digest = sha256(b"abc");
    let changed = refusal("412 Precondition Failed", "PreconditionFailed");
    let (endpoint, _) = script(vec![empty_head(), changed]).await;
    let lost = wire_store(endpoint)
        .put_stream_content_bound(&key("k"), 3, body(b"abc"), digest)
        .await;
    assert_eq!(service(&lost.unwrap_err()), (412, "PreconditionFailed"));

    let gone = refusal("404 Not Found", "NoSuchKey");
    let (endpoint, _) = script(vec![empty_head(), gone]).await;
    let lost = wire_store(endpoint)
        .put_stream_content_bound(&key("k"), 3, body(b"abc"), digest)
        .await;
    assert!(matches!(lost.unwrap_err(), ObjectStoreError::NotFound(_)));

    let absent = bare("404 Not Found", &["content-length: 0"]);
    let taken = refusal("412 Precondition Failed", "PreconditionFailed");
    let (endpoint, _) = script(vec![absent, taken]).await;
    let found = wire_store(endpoint)
        .put_stream_content_bound(&key("k"), 3, body(b"abc"), digest)
        .await;
    assert_eq!(found.unwrap(), ContentBoundPut::Existed);
}

/// Asserts that a damaged object whose HEAD gives no entity tag is not replaced (no write can be
/// made conditional on it): the write fails with a protocol error and sends no PUT.
#[tokio::test]
async fn a_damaged_object_without_a_tag_is_not_replaced() {
    let untagged = bare("200 OK", &["content-length: 0"]);
    let (endpoint, seen) = script(vec![untagged]).await;
    let store = wire_store(endpoint);
    let failed = store
        .put_stream_content_bound(&key("k"), 3, unread(), sha256(b"abc"))
        .await;
    let failed = failed.unwrap_err();
    assert!(
        matches!(&failed, ObjectStoreError::Protocol(m) if m.contains("no ETag")),
        "{failed}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}
