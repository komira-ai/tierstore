//! The work after a write that failed (`s3/cleanup.rs`): the write gives its own error as soon as
//! it has it, and the look at the key of a single PUT that got no answer, or the abort of a
//! multipart upload, runs in a task of its own; an abort that fails is kept and sent again when
//! the store answers again.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use objstore_s3::s3::fake::{Failure, FakeS3};
use objstore_s3::s3::{
    Addressing, Cleanups, Credentials, CutPuts, PART_BYTES, S3Config, S3Store, S3Tuning,
};
use objstore_s3::{BodyStream, Capabilities, ObjectKey, ObjectStore as _, ObjectStoreError};
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// The request timeout of the store that never answers: the default of a tierstore cache.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/{name}")).unwrap()
}

/// A store at `endpoint` with this request timeout.
fn store_at(endpoint: &str, request_timeout: Duration, tuning: S3Tuning) -> S3Store {
    let config = S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "test-bucket".to_owned(),
        addressing: Addressing::Path,
        ca_pem: None,
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: CONDITIONAL,
        connect_timeout: Duration::from_secs(5),
        request_timeout,
        min_body_rate: None,
    };
    S3Store::new_tuned(config, tuning).unwrap()
}

/// A store of the loopback service that sends a body of more than one byte as a multipart
/// upload, with this request timeout.
fn uploading(s3: &FakeS3, request_timeout: Duration) -> S3Store {
    let tuning = S3Tuning {
        single_put_max: 1,
        ..S3Tuning::default()
    };
    let config = S3Config {
        request_timeout,
        ..s3.config(CONDITIONAL)
    };
    S3Store::new_tuned(config, tuning).unwrap()
}

/// `bytes` as a body of one chunk.
fn body(bytes: &'static [u8]) -> BodyStream {
    futures::stream::once(async move { Ok(Bytes::from_static(bytes)) }).boxed()
}

/// A body that gives `bytes` and then fails, as the client of an upload that went away.
fn failing(bytes: &'static [u8]) -> BodyStream {
    let items = vec![
        Ok(Bytes::from_static(bytes)),
        Err(ObjectStoreError::Body("the client went away".into())),
    ];
    futures::stream::iter(items).boxed()
}

/// A store on loopback that takes every connection, reads the request on it whole, says so,
/// and never answers: an upstream that hangs. Gives its endpoint, the first lines of the
/// requests it read, and the signal of each.
async fn never_answers() -> (String, Arc<Mutex<Vec<String>>>, Arc<Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let read = Arc::new(Mutex::new(Vec::new()));
    let heard = Arc::new(Notify::new());
    let (log, told) = (Arc::clone(&read), Arc::clone(&heard));
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (log, told) = (Arc::clone(&log), Arc::clone(&told));
            tokio::spawn(async move {
                if let Some(line) = read_request(&mut sock).await {
                    log.lock().unwrap().push(line);
                    told.notify_one();
                }
                std::future::pending::<()>().await;
                drop(sock);
            });
        }
    });
    (endpoint, read, heard)
}

/// Reads a request's head and its `Content-Length` body; gives its first line, or `None` if
/// the client closed first.
async fn read_request(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    head.lines().next().map(str::to_owned)
}

/// Asserts that a put to a store that takes the connection, reads the whole PUT and never
/// answers gives its error at the request timeout, with nothing more: the look at the key that
/// follows runs after the error is given, and ends at its own deadline (counted a failure), not
/// at the HEAD's request timeout. Catches: a failed put that holds its caller (and the write
/// permit a server above holds for it) through the look, about 121 s in all.
///
/// The wait is not real. The store is a real socket, so the clock is paused only once the store
/// has read the whole PUT (it says so): from then on the client has nothing to wait for but its
/// timer, and the paused clock moves on to it by itself.
#[tokio::test]
async fn a_put_to_a_store_that_never_answers_gives_its_error_at_the_request_timeout() {
    let (endpoint, read, heard) = never_answers().await;
    let store = Arc::new(store_at(&endpoint, REQUEST_TIMEOUT, S3Tuning::default()));
    let putting = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.put_stream(&key("hung"), 4, body(b"abcd")).await })
    };
    heard.notified().await;
    tokio::time::pause();
    let paused = tokio::time::Instant::now();
    let error = putting.await.unwrap().unwrap_err();
    let waited = paused.elapsed();
    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error:?}");
    assert!(waited <= REQUEST_TIMEOUT, "the put gave its error after {waited:?}");
    assert_eq!(read.lock().unwrap()[0], "PUT /test-bucket/cas/hung HTTP/1.1");
    store.cleanups_idle().await;
    assert_eq!(
        store.cut_puts(),
        CutPuts {
            removed: 0,
            left: 0,
            failed: 1
        }
    );
    assert_eq!(store.cleanups(), Cleanups::default());
}

/// Asserts that a PUT that never reached a store (every connect refused) is not looked after:
/// nothing is counted, and no task runs. Catches: a look sent after a request the store never
/// saw, which can find nothing of it and only costs a request to a store that is down.
#[tokio::test]
async fn a_put_that_never_reached_the_store_is_not_looked_after() {
    let endpoint = format!("http://{}", tierstore_testkit::port::dead_addr());
    let store = store_at(&endpoint, REQUEST_TIMEOUT, S3Tuning::default());
    let error = store
        .put_stream(&key("unsent"), 4, body(b"abcd"))
        .await
        .unwrap_err();
    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error:?}");
    store.cleanups_idle().await;
    assert_eq!(store.cut_puts(), CutPuts::default());
    assert_eq!(store.cleanups(), Cleanups::default());
}

/// Asserts that a multipart upload whose body fails gives the body's error while its abort is
/// still held, unanswered, by the store. Catches: a failed upload that holds its caller until
/// the abort is answered (here the request timeout, an hour: the test would hang).
#[tokio::test]
async fn a_failed_upload_gives_its_error_before_its_abort_is_answered() {
    let s3 = FakeS3::start().await;
    let store = uploading(&s3, Duration::from_secs(3600));
    s3.hold_next_abort();
    let error = store
        .put_stream(&key("held"), 8, failing(b"abcd"))
        .await
        .unwrap_err();
    assert!(matches!(error, ObjectStoreError::Body(_)), "{error:?}");
    s3.wait_for(|s3| s3.held_aborts() == 1).await;
    assert_eq!(s3.incomplete_uploads(), 1);
    assert_eq!(s3.stats().aborted, 0);
}

/// Asserts that an abort the store fails is kept (the gauge is 1, the upload still open), and
/// is sent again, and done, when the store next answers a request, whatever that answer is (a
/// 404 here); the list is then empty. Catches: a failed abort that is only logged and leaves the
/// upload to the lifecycle rule, and a list that is never sent again.
#[tokio::test]
async fn an_abort_that_fails_is_kept_and_sent_again_when_the_store_answers() {
    let s3 = FakeS3::start().await;
    let store = uploading(&s3, REQUEST_TIMEOUT);
    s3.fail(Failure::Abort);
    let error = store
        .put_stream(&key("kept"), 8, failing(b"abcd"))
        .await
        .unwrap_err();
    assert!(matches!(error, ObjectStoreError::Body(_)), "{error:?}");
    store.cleanups_idle().await;
    let kept = Cleanups {
        failed_aborts: 1,
        ..Cleanups::default()
    };
    assert_eq!(store.cleanups(), kept);
    assert_eq!(s3.incomplete_uploads(), 1);
    let absent = store.head(&key("absent")).await;
    assert!(matches!(absent, Err(ObjectStoreError::NotFound(_))), "{absent:?}");
    store.cleanups_idle().await;
    assert_eq!(store.cleanups(), Cleanups::default());
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.stats().aborted, 1);
}

/// Asserts that an abort that fails again when it is sent again stays on the list, its upload
/// open, and that a write that completes is not held up by the list. Catches: a failed retry
/// that drops the upload, or one that loops on a store that fails it.
#[tokio::test]
async fn an_abort_that_fails_again_stays_kept() {
    let s3 = FakeS3::start().await;
    let store = uploading(&s3, REQUEST_TIMEOUT);
    s3.fail(Failure::Abort);
    s3.fail(Failure::Abort);
    let failed = store.put_stream(&key("twice"), 8, failing(b"abcd")).await;
    assert!(failed.is_err());
    store.cleanups_idle().await;
    let whole = store.put_stream(&key("whole"), 1, body(b"a")).await;
    whole.unwrap();
    store.cleanups_idle().await;
    let kept = Cleanups {
        failed_aborts: 1,
        ..Cleanups::default()
    };
    assert_eq!(store.cleanups(), kept);
    assert_eq!(s3.incomplete_uploads(), 1);
    let aborts = s3
        .requests()
        .iter()
        .filter(|r| r.starts_with("DELETE ") && r.contains("uploadId="))
        .count();
    assert_eq!(aborts, 2, "the abort, and one more when the store answered");
}

/// Asserts that a large write still goes whole through a store with the work after failed
/// writes: its parts, its completion, no upload left and nothing kept.
#[tokio::test]
async fn a_write_that_completes_leaves_nothing_to_clean_up() {
    let s3 = FakeS3::start().await;
    let store = uploading(&s3, REQUEST_TIMEOUT);
    let bytes = vec![7_u8; PART_BYTES as usize + 1];
    let len = bytes.len() as u64;
    let one = futures::stream::once(async move { Ok(Bytes::from(bytes)) }).boxed();
    store.put_stream(&key("whole"), len, one).await.unwrap();
    store.cleanups_idle().await;
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(store.cleanups(), Cleanups::default());
}
