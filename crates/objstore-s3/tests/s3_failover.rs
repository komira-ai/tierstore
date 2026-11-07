//! A request whose connection to its endpoint could not be made goes to the next endpoint of the
//! store, and a request that may have reached a store is never sent again. Against S3 services on
//! loopback, ports that refuse a connection, and servers that read a request and then answer a
//! scripted text.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::s3::{Credentials, S3Config, S3Store, S3Tuning};
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn config(endpoint: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tierstore-fake".to_owned(),
        credentials: Credentials::new("fake-key", "fake-secret"),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    }
}

fn tuning(attempts: u32) -> S3Tuning {
    S3Tuning {
        endpoint_attempts: attempts,
        read_retries: 0,
        ..S3Tuning::default()
    }
}

/// A store over `endpoints`, the first of them the main one.
fn store(endpoints: &[String], attempts: u32) -> S3Store {
    S3Store::new_rotating(config(&endpoints[0]), tuning(attempts), &endpoints[1..]).unwrap()
}

/// An endpoint on loopback that refuses connections.
fn refused() -> String {
    format!("http://{}", tierstore_testkit::port::dead_addr())
}

async fn live() -> FakeS3 {
    let fake = FakeS3::start().await;
    fake.require_credentials("fake-key", "fake-secret");
    fake
}

fn key(n: usize) -> ObjectKey {
    ObjectKey::new(format!("k{n}")).unwrap()
}

/// A server that reads each request (head and body), counts it, writes `reply` and closes the
/// connection.
async fn scripted(reply: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(0, |n| n.trim().parse::<usize>().unwrap());
            while buf.len() < head_end + length {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed inside a body");
                buf.extend_from_slice(&chunk[..n]);
            }
            seen.fetch_add(1, Ordering::SeqCst);
            sock.write_all(reply).await.unwrap();
            sock.shutdown().await.unwrap();
        }
    });
    (endpoint, count)
}

/// Asserts that a request whose connection to its endpoint is refused lands on the next endpoint,
/// every time, and that each object is stored once (a request that failed to connect leaves
/// nothing behind and is not sent twice to the one that works).
#[tokio::test]
async fn a_refused_endpoint_is_skipped_and_the_request_lands_on_the_next() {
    let (a, b) = (live().await, live().await);
    let endpoints = [refused(), a.endpoint().to_owned(), b.endpoint().to_owned()];
    let s = store(&endpoints, 3);
    for n in 0..6 {
        s.put_new(&key(n), Bytes::from_static(b"x"), None)
            .await
            .unwrap();
    }
    let mut keys: Vec<String> = a.keys().into_iter().chain(b.keys()).collect();
    keys.sort();
    assert_eq!(keys, ["k0", "k1", "k2", "k3", "k4", "k5"]);
    assert_eq!(a.requests().len() + b.requests().len(), 6);
}

/// Asserts that a read goes to the next endpoint when the first refuses the connection, with no
/// resend of the read (`read_retries` is 0).
#[tokio::test]
async fn a_read_is_sent_to_the_next_endpoint_when_the_first_refuses() {
    let fake = live().await;
    let plain = S3Store::new_tuned(config(fake.endpoint()), tuning(1)).unwrap();
    plain
        .put_new(&key(0), Bytes::from_static(b"abcd"), None)
        .await
        .unwrap();
    let s = store(&[refused(), fake.endpoint().to_owned()], 2);
    let got = s
        .get_range(&key(0), ByteRange::new(0, 4).unwrap())
        .await
        .unwrap();
    assert_eq!(&got[..], b"abcd");
}

/// Asserts that the attempts are the bound: with three endpoints that refuse and a fourth that
/// works, three sends fail with a transport error and the fourth endpoint is not reached, and four
/// sends land the request there once.
#[tokio::test]
async fn a_request_is_sent_at_most_endpoint_attempts_times() {
    let fake = live().await;
    let endpoints = [refused(), refused(), refused(), fake.endpoint().to_owned()];
    let three = store(&endpoints, 3);
    let err = three
        .put_new(&key(0), Bytes::from_static(b"x"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert!(fake.requests().is_empty());

    let four = store(&endpoints, 4);
    four.put_new(&key(0), Bytes::from_static(b"x"), None)
        .await
        .unwrap();
    assert_eq!(fake.requests().len(), 1);
}

/// Asserts that with every endpoint down the error is the transport error a single refused
/// endpoint gives, the same text apart from the address.
#[tokio::test]
async fn all_endpoints_down_fail_as_one_endpoint_down_does() {
    let (one, two) = (refused(), refused());
    let alone = store(std::slice::from_ref(&one), 3);
    let both = store(&[one, two], 3);
    let body = || Bytes::from_static(b"x");
    let e1 = alone.put_new(&key(0), body(), None).await.unwrap_err();
    let e2 = both.put_new(&key(0), body(), None).await.unwrap_err();
    assert!(matches!(e1, ObjectStoreError::Transport(_)), "{e1}");
    assert!(matches!(e2, ObjectStoreError::Transport(_)), "{e2}");
    assert_eq!(e1.to_string(), e2.to_string());
}

/// Asserts that `endpoint_attempts = 1` sends once: the request that meets the refused endpoint
/// fails, and the endpoint that works is not reached.
#[tokio::test]
async fn one_attempt_never_moves_on() {
    let fake = live().await;
    let s = store(&[refused(), fake.endpoint().to_owned()], 1);
    let err = s
        .put_new(&key(0), Bytes::from_static(b"x"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert!(fake.requests().is_empty());
}

/// Asserts that an answer with a 5xx status is the request's answer: the server saw one request,
/// the other endpoint none, for a plain and for a create-only PUT.
#[tokio::test]
async fn an_answer_of_5xx_is_not_sent_again() {
    let reply: &'static [u8] =
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    for conditional in [false, true] {
        let (bad, count) = scripted(reply).await;
        let fake = live().await;
        let mut cfg = config(&bad);
        cfg.capabilities.conditional_put = conditional;
        let s = S3Store::new_rotating(cfg, tuning(3), &[fake.endpoint().to_owned()]).unwrap();
        let err = s
            .put_new(&key(0), Bytes::from_static(b"x"), None)
            .await
            .unwrap_err();
        assert!(!matches!(err, ObjectStoreError::Transport(_)), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 1, "conditional {conditional}");
        assert!(fake.requests().is_empty());
    }
}

/// Asserts that a read whose answer is cut inside its body is not sent again, and that a
/// create-only PUT and a streamed create-only PUT read by a server that closes the connection
/// without an answer are each sent once: the server saw one request, the other endpoint none.
#[tokio::test]
async fn a_request_that_reached_a_server_is_not_sent_again() {
    let cut: &'static [u8] = b"HTTP/1.1 206 Partial Content\r\ncontent-length: 100\r\n\
        content-range: bytes 0-99/200\r\nconnection: close\r\n\r\npartial";
    let (bad, count) = scripted(cut).await;
    let fake = live().await;
    let s = store(&[bad, fake.endpoint().to_owned()], 3);
    let err = s
        .get_range(&key(0), ByteRange::new(0, 100).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(fake.requests().is_empty());

    let (bad, count) = scripted(b"").await;
    let fake = live().await;
    let s = store(&[bad, fake.endpoint().to_owned()], 3);
    let err = s
        .put_new(&key(1), Bytes::from_static(b"x"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 1, "create-only put");
    assert!(fake.requests().is_empty());

    let (bad, count) = scripted(b"").await;
    let fake = live().await;
    let s = store(&[bad, fake.endpoint().to_owned()], 3);
    let body = stream::iter([Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))]);
    let err = s.put_stream(&key(2), 4, body.boxed()).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 1, "streamed put");
    assert!(fake.requests().is_empty());
}

/// Asserts that a streamed PUT whose first endpoint refuses the connection is sent whole to the
/// next from the start of its source: the stored object is the source's bytes, once, in order.
#[tokio::test]
async fn a_streamed_put_that_never_connected_is_sent_whole_to_the_next_endpoint() {
    let fake = live().await;
    let s = store(&[refused(), fake.endpoint().to_owned()], 2);
    let pieces: Vec<Result<Bytes, ObjectStoreError>> =
        (0..8u8).map(|n| Ok(Bytes::from(vec![n; 1000]))).collect();
    s.put_stream(&key(0), 8000, stream::iter(pieces).boxed())
        .await
        .unwrap();
    let read = S3Store::new_tuned(config(fake.endpoint()), tuning(1)).unwrap();
    let got = read
        .get_range(&key(0), ByteRange::new(0, 8000).unwrap())
        .await
        .unwrap();
    let want: Vec<u8> = (0..8u8).flat_map(|n| vec![n; 1000]).collect();
    assert_eq!(&got[..], &want[..]);
}

/// Asserts that a single endpoint whose connection is refused fails with a transport error after
/// the attempts, and a streamed body that failed on its own is that error, not a connection's.
#[tokio::test]
async fn a_single_refused_endpoint_fails_with_a_transport_error() {
    let s = store(&[refused()], 3);
    let body = stream::iter([Ok(Bytes::from_static(b"ab"))]).boxed();
    let err = s.put_stream(&key(0), 2, body).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");

    let fake = live().await;
    let s = store(&[fake.endpoint().to_owned()], 3);
    let broken = stream::iter([Err(ObjectStoreError::Body("the source broke".to_owned()))]).boxed();
    let err = s.put_stream(&key(1), 2, broken).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Body(_)), "{err}");
    assert!(fake.keys().is_empty());
}
