//! The S3 store's streamed read and streamed conditional replacement against a scripted HTTP
//! server on loopback: what each request carries, and how each answer, and each piece of a body,
//! is read.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStoreError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
            let mut chunk = [0u8; 65536];
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

/// An answer with `headers` and a body whose length it states.
fn answer(status: &str, headers: &[&str], body: &str) -> String {
    let mut s = format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s.push_str(body);
    s
}

fn store(endpoint: String) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    })
    .unwrap()
}

fn key() -> &'static ObjectKey {
    static KEY: OnceLock<ObjectKey> = OnceLock::new();
    KEY.get_or_init(|| ObjectKey::new("cas/seg-1").unwrap())
}

fn range(offset: u64, len: u64) -> Option<ByteRange> {
    ByteRange::new(offset, len)
}

const MARK: &str = "x-amz-meta-tierstore-reservation: 1";

/// The bytes of the body of a read, and the error that ended it if one did.
async fn drained(mut body: objstore_s3::s3::ObjectBody) -> (Vec<u8>, Option<ObjectStoreError>) {
    let mut bytes = Vec::new();
    while let Some(piece) = body.next().await {
        match piece {
            Ok(piece) => bytes.extend_from_slice(&piece),
            Err(error) => return (bytes, Some(error)),
        }
    }
    (bytes, None)
}

/// The bytes of a body that ended without an error.
async fn whole(body: objstore_s3::s3::ObjectBody) -> Vec<u8> {
    let (bytes, error) = drained(body).await;
    assert!(error.is_none(), "{error:?}");
    bytes
}

/// Asserts that a streamed read of the whole object is a GET with no `Range`, that its body is
/// given as it arrives, and that the mark is the one of the answer's own header: present on one
/// answer, absent on the next.
#[tokio::test]
async fn a_streamed_read_sends_a_get_and_gives_the_body_and_the_mark_of_its_answer() {
    let marked = answer("200 OK", &[MARK], "abcdefg");
    let plain = answer("200 OK", &[], "xyz");
    let (endpoint, seen) = script(vec![marked, plain]).await;
    let s = store(endpoint);
    let read = s.get_stream(key(), None).await.unwrap();
    assert!(read.reservation);
    assert_eq!(whole(read.body).await, b"abcdefg");
    let read = s.get_stream(key(), None).await.unwrap();
    assert!(!read.reservation);
    assert_eq!(whole(read.body).await, b"xyz");

    let heads = seen.lock().unwrap();
    let get = heads[0].to_ascii_lowercase();
    assert!(
        get.starts_with("get /test-bucket/cas/seg-1 http/1.1\r\n"),
        "{get}"
    );
    assert!(!get.contains("\r\nrange:"), "{get}");
}

/// Asserts that a streamed ranged read sends the `Range` of the bytes asked for, checks the
/// `Content-Range` and the length of the 206 against it, and gives the mark of that answer.
#[tokio::test]
async fn a_streamed_ranged_read_sends_the_range_and_gives_the_mark_of_its_answer() {
    let part = answer(
        "206 Partial Content",
        &["content-range: bytes 2-4/10", MARK],
        "cde",
    );
    let (endpoint, seen) = script(vec![part]).await;
    let read = store(endpoint)
        .get_stream(key(), range(2, 3))
        .await
        .unwrap();
    assert!(read.reservation);
    assert_eq!(whole(read.body).await, b"cde");
    let get = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(get.contains("\r\nrange: bytes=2-4\r\n"), "{get}");
}

/// Asserts that each answer that is not the body asked for is typed when the read is opened: 404
/// is `NotFound`, 416 is `InvalidRange`, another status is a `Service` error, and a ranged read
/// answered with the whole object, a whole read answered with a range, a 206 whose
/// `Content-Range` is another range or is missing, or a 206 of no stated length is a `Protocol`
/// error.
#[tokio::test]
async fn a_streamed_read_that_is_not_answered_with_what_was_asked_is_typed() {
    let missing = answer(
        "404 Not Found",
        &[],
        "<Error><Code>NoSuchKey</Code></Error>",
    );
    let (endpoint, _) = script(vec![missing]).await;
    let err = store(endpoint).get_stream(key(), None).await.err().unwrap();
    assert!(matches!(err, ObjectStoreError::NotFound(_)), "{err}");

    let unsatisfiable = answer("416 Range Not Satisfiable", &[], "");
    let (endpoint, _) = script(vec![unsatisfiable]).await;
    let err = store(endpoint).get_stream(key(), range(20, 4)).await;
    let err = err.err().unwrap();
    assert!(
        matches!(err, ObjectStoreError::InvalidRange { .. }),
        "{err}"
    );

    let broken = "<Error><Code>InternalError</Code><Message>m</Message></Error>";
    let (endpoint, _) = script(vec![answer("500 Oops", &[], broken)]).await;
    let err = store(endpoint).get_stream(key(), None).await.err().unwrap();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 500, .. }),
        "{err}"
    );

    let whole = answer("200 OK", &[], "0123456789");
    let wrong_range = answer(
        "206 Partial Content",
        &["content-range: bytes 0-2/10"],
        "cde",
    );
    let no_range = answer("206 Partial Content", &[], "cde");
    let unsized_body = "HTTP/1.1 206 Partial Content\r\nconnection: close\r\n\
                        content-range: bytes 2-4/10\r\ntransfer-encoding: chunked\r\n\r\n\
                        3\r\ncde\r\n0\r\n\r\n";
    let partial = answer(
        "206 Partial Content",
        &["content-range: bytes 2-4/10"],
        "cde",
    );
    let cases = [
        (whole, range(2, 3), "ignored Range"),
        (wrong_range, range(2, 3), "Content-Range"),
        (no_range, range(2, 3), "Content-Range"),
        (unsized_body.to_owned(), range(2, 3), "Content-Length"),
        (partial, None, "206 to a read of the whole object"),
    ];
    for (answered, asked, said) in cases {
        let (endpoint, _) = script(vec![answered]).await;
        let err = store(endpoint)
            .get_stream(key(), asked)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
        assert!(err.to_string().contains(said), "{said}: {err}");
    }
}

/// Asserts that a streamed read is sent again after a failure in transport before any of the body
/// was given, as a read is.
#[tokio::test]
async fn a_streamed_read_is_sent_again_after_a_transport_failure_before_its_body() {
    let hello = answer("200 OK", &[], "hello");
    let (endpoint, seen) = script(vec![String::new(), hello]).await;
    let read = store(endpoint).get_stream(key(), None).await.unwrap();
    assert_eq!(whole(read.body).await, b"hello");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// Asserts that a body the store cuts short gives the bytes that came and then a transport error as
/// its last piece, and no piece after it, and that the read is not sent again.
#[tokio::test]
async fn a_streamed_body_cut_short_ends_with_a_transport_error() {
    let cut = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 10\r\n\r\nabc".to_owned();
    let (endpoint, seen) = script(vec![cut]).await;
    let read = store(endpoint).get_stream(key(), None).await.unwrap();
    let (bytes, error) = drained(read.body).await;
    assert_eq!(bytes, b"abc");
    let error = error.unwrap();
    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// A body of `len` zero bytes as one-MiB pieces, ending with `error` in place of the last piece if
/// there is one.
fn zeros(len: u64, error: Option<ObjectStoreError>) -> BodyStream {
    let mebibyte = 1_u64 << 20;
    let mut items: Vec<Result<Bytes, ObjectStoreError>> = Vec::new();
    let mut left = len;
    while left > 0 {
        let piece = left.min(mebibyte);
        items.push(Ok(Bytes::from(vec![0_u8; piece as usize])));
        left -= piece;
    }
    if let Some(error) = error {
        items.pop();
        items.push(Err(error));
    }
    stream::iter(items).boxed()
}

/// Asserts that a streamed replacement of at most one PUT's size is one PUT with `If-Match` and no
/// `If-None-Match`, that it gives the `ETag` of the answer, that a body that fails or has another
/// length sends no request, and that the 412 and the 404 of the store are typed.
#[tokio::test]
async fn a_small_streamed_replace_is_one_put_with_if_match() {
    let done = answer("200 OK", &["etag: \"new\""], "");
    let (endpoint, seen) = script(vec![done]).await;
    let s = store(endpoint);
    let put = s.put_stream_if_match(key(), 3 << 20, zeros(3 << 20, None), "\"old\"");
    assert_eq!(put.await.unwrap(), "\"new\"");
    let put = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(put.starts_with("put /test-bucket/cas/seg-1 "), "{put}");
    assert!(put.contains("\r\nif-match: \"old\"\r\n"), "{put}");
    assert!(!put.contains("if-none-match"), "{put}");

    let (endpoint, seen) = script(Vec::new()).await;
    let s = store(endpoint);
    let cut = ObjectStoreError::Body("cut".to_owned());
    let failing = zeros(2 << 20, Some(cut.clone()));
    let failed = s.put_stream_if_match(key(), 2 << 20, failing, "\"old\"");
    assert_eq!(failed.await.unwrap_err().to_string(), cut.to_string());
    let short = s.put_stream_if_match(key(), 100, zeros(10, None), "\"old\"");
    assert!(matches!(short.await, Err(ObjectStoreError::Body(_))));
    assert_eq!(seen.lock().unwrap().len(), 0);

    let refused = answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code></Error>",
    );
    let missing = answer(
        "404 Not Found",
        &[],
        "<Error><Code>NoSuchKey</Code></Error>",
    );
    let (endpoint, _) = script(vec![refused, missing]).await;
    let s = store(endpoint);
    let put = s.put_stream_if_match(key(), 3, zeros(3, None), "\"old\"");
    let err = put.await.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 412, .. }),
        "{err}"
    );
    let put = s.put_stream_if_match(key(), 3, zeros(3, None), "\"old\"");
    let err = put.await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::NotFound(_)), "{err}");
}

const OVER_ONE_PUT: u64 = (16 << 20) + 1;

/// The answers of a store to a multipart upload of two parts whose completion is answered
/// `completion`, and, if the completion is a failure, to the abort that follows.
fn upload_answers(completion: String, aborted: bool) -> Vec<String> {
    let id =
        "<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>";
    let mut answers = vec![
        answer("200 OK", &[], id),
        answer("200 OK", &["etag: \"p1\""], ""),
        answer("200 OK", &["etag: \"p2\""], ""),
        completion,
    ];
    if aborted {
        answers.push(answer("204 No Content", &[], ""));
    }
    answers
}

/// Runs a streamed replacement of more than one PUT's size against a store that gives
/// `upload_answers(completion, aborted)`, and gives its outcome and the requests it made.
async fn big_replace(
    completion: String,
    aborted: bool,
) -> (Result<String, ObjectStoreError>, Vec<String>) {
    let (endpoint, seen) = script(upload_answers(completion, aborted)).await;
    let s = store(endpoint);
    let put = s.put_stream_if_match(key(), OVER_ONE_PUT, zeros(OVER_ONE_PUT, None), "\"old\"");
    let outcome = put.await;
    let requests = seen.lock().unwrap().clone();
    (outcome, requests)
}

/// Asserts that a streamed replacement of more than one PUT's size is a multipart upload whose
/// completion alone carries `If-Match` (and no `If-None-Match`), that it gives the `ETag` the
/// completion names, and that a completion the store refuses with 412 or 404 `NoSuchKey` is
/// typed, any other 404 is a `Service` error, and the upload is aborted after each failure.
#[tokio::test]
async fn a_large_streamed_replace_is_completed_with_if_match_and_aborted_on_failure() {
    let ok =
        "<CompleteMultipartUploadResult><ETag>\"joined-2\"</ETag></CompleteMultipartUploadResult>";
    let (outcome, requests) = big_replace(answer("200 OK", &[], ok), false).await;
    assert_eq!(outcome.unwrap(), "\"joined-2\"");
    assert_eq!(requests.len(), 4);
    for (at, request) in requests.iter().enumerate() {
        let request = request.to_ascii_lowercase();
        let expected = at == 3;
        assert_eq!(
            request.contains("\r\nif-match: \"old\"\r\n"),
            expected,
            "{request}"
        );
        assert!(!request.contains("if-none-match"), "{request}");
    }

    let refused = answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code></Error>",
    );
    let (outcome, requests) = big_replace(refused, true).await;
    let err = outcome.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 412, .. }),
        "{err}"
    );
    assert!(requests[4].starts_with("DELETE "), "{requests:?}");

    let gone = answer(
        "404 Not Found",
        &[],
        "<Error><Code>NoSuchKey</Code></Error>",
    );
    let (outcome, requests) = big_replace(gone, true).await;
    assert!(
        matches!(outcome, Err(ObjectStoreError::NotFound(_))),
        "{outcome:?}"
    );
    assert_eq!(requests.len(), 5);

    let no_upload = answer(
        "404 Not Found",
        &[],
        "<Error><Code>NoSuchUpload</Code></Error>",
    );
    let (outcome, _) = big_replace(no_upload, true).await;
    let err = outcome.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 404, .. }),
        "{err}"
    );

    let unnamed = "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>";
    let (outcome, _) = big_replace(answer("200 OK", &[], unnamed), false).await;
    let err = outcome.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
}
