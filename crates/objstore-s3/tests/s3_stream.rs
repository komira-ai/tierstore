//! The S3 store's streamed read and streamed conditional replacement against a scripted HTTP
//! server on loopback: what each request carries, and how each answer, and each piece of a body,
//! is read.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use objstore_s3::s3::{Credentials, S3Config, S3Store, S3Tuning};
use objstore_s3::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStoreError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves `responses` in order, one per connection, and records each request head.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let (told, _) = tokio::sync::mpsc::unbounded_channel();
    script_heads(responses, told).await
}

/// As [`script`], and also gives each request head on `told` as soon as the request is complete.
async fn script_heads(
    responses: Vec<String>,
    told: tokio::sync::mpsc::UnboundedSender<String>,
) -> (String, Arc<Mutex<Vec<String>>>) {
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
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            // A request whose body was cut is not a request the store took: not logged, not answered.
            if buf.len() < head_end + body_len {
                continue;
            }
            log.lock().unwrap().push(head.clone());
            let _ = told.send(head);
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
    S3Store::new(config(endpoint)).unwrap()
}

fn config(endpoint: String) -> S3Config {
    S3Config {
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
    }
}

/// A store that sends an object over [`OVER_ONE_PUT`] bytes less one as a multipart upload (the
/// default sends it as one PUT up to 5 GiB), so that the tests of the upload write 16 MiB, not 5 GiB.
fn store_cut_at_a_part(endpoint: String) -> S3Store {
    let tuning = S3Tuning {
        single_put_max: OVER_ONE_PUT - 1,
        ..S3Tuning::default()
    };
    S3Store::new_tuned(config(endpoint), tuning).unwrap()
}

fn key() -> &'static ObjectKey {
    static KEY: OnceLock<ObjectKey> = OnceLock::new();
    KEY.get_or_init(|| ObjectKey::new("cas/seg-1").unwrap())
}

fn range(offset: u64, len: u64) -> Option<ByteRange> {
    ByteRange::new(offset, len)
}

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
/// given as it arrives, and that the tag, the size and the time of last write are those of the
/// answer's own headers: present on one answer (the size is its `Content-Length`, the time its
/// `Last-Modified`), absent on the next (no time).
#[tokio::test]
async fn a_streamed_read_sends_a_get_and_gives_the_body_and_the_tag_of_its_answer() {
    let stamp = "last-modified: Tue, 03 Nov 2026 08:49:37 GMT";
    let tagged = answer("200 OK", &["etag: \"one\"", stamp], "abcdefg");
    let plain = answer("200 OK", &[], "xyz");
    let (endpoint, seen) = script(vec![tagged, plain]).await;
    let s = store(endpoint);
    let read = s.get_stream(key(), None).await.unwrap();
    assert_eq!(read.etag.as_deref(), Some("\"one\""));
    assert_eq!(read.size, Some(7));
    assert_eq!(read.modified_millis, Some(1_793_695_777_000));
    assert_eq!(whole(read.body).await, b"abcdefg");
    let read = s.get_stream(key(), None).await.unwrap();
    assert_eq!(read.etag, None);
    assert_eq!(read.modified_millis, None);
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
/// `Content-Range` and the length of the 206 against it, and gives the tag of that answer and the
/// size of the whole object, which is the total of the `Content-Range` and not the length of the
/// bytes read.
#[tokio::test]
async fn a_streamed_ranged_read_sends_the_range_and_gives_the_tag_of_its_answer() {
    let part = answer(
        "206 Partial Content",
        &["content-range: bytes 2-4/10", "etag: \"two\""],
        "cde",
    );
    let (endpoint, seen) = script(vec![part]).await;
    let read = store(endpoint)
        .get_stream(key(), range(2, 3))
        .await
        .unwrap();
    assert_eq!(read.etag.as_deref(), Some("\"two\""));
    assert_eq!(read.size, Some(10));
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

/// Asserts that a streamed read is sent again after a failure in transport before the response
/// headers arrived, as a read is.
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

    // The request is begun and its body cut: the server never has the whole of it, and answers nothing.
    let (endpoint, seen_cut) = script(vec![answer("200 OK", &[], "")]).await;
    let s = store(endpoint);
    let cut = ObjectStoreError::Body("cut".to_owned());
    let failing = zeros(2 << 20, Some(cut.clone()));
    let failed = s.put_stream_if_match(key(), 2 << 20, failing, "\"old\"");
    assert_eq!(failed.await.unwrap_err().to_string(), cut.to_string());
    let (endpoint, seen_short) = script(vec![answer("200 OK", &[], "")]).await;
    let s = store(endpoint);
    let short = s.put_stream_if_match(key(), 100, zeros(10, None), "\"old\"");
    assert!(matches!(short.await, Err(ObjectStoreError::Body(_))));
    assert_eq!(seen_cut.lock().unwrap().len(), 0);
    assert_eq!(seen_short.lock().unwrap().len(), 0);

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
    let s = store_cut_at_a_part(endpoint);
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

/// Asserts that a read whose answer has valid headers and then fails before its first body byte is
/// not sent again: the retry boundary is the response headers. The read is given, its body ends
/// with a transport error as the first piece, and the store saw one request.
#[tokio::test]
async fn a_failure_after_valid_headers_and_before_the_first_body_byte_is_not_retried() {
    let headers_only = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 10\r\n\r\n";
    let hello = answer("200 OK", &[], "hello");
    let (endpoint, seen) = script(vec![headers_only.to_owned(), hello]).await;
    let read = store(endpoint).get_stream(key(), None).await.unwrap();
    let (bytes, error) = drained(read.body).await;
    assert!(bytes.is_empty(), "{bytes:?}");
    let error = error.unwrap();
    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// The completion of a two-part upload that names `joined-2`.
fn two_part_completion() -> String {
    let ok =
        "<CompleteMultipartUploadResult><ETag>\"joined-2\"</ETag></CompleteMultipartUploadResult>";
    answer("200 OK", &[], ok)
}

/// Asserts that a streamed replacement of a body of unknown length that is longer than one part
/// sends the first part while the producer still withholds the rest: the part's request arrives
/// before the producer is released, nothing else has been sent and the call has not returned, and
/// after the release the second part and the completion (which alone carries `If-Match`) follow
/// and the call gives the completion's tag. The producer is gated on an event, never on time.
#[tokio::test]
async fn an_unsized_streamed_replace_sends_a_part_while_the_producer_withholds_the_rest() {
    let (told, mut heads) = tokio::sync::mpsc::unbounded_channel();
    let (endpoint, _) = script_heads(upload_answers(two_part_completion(), false), told).await;
    let s = store(endpoint);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let first = Bytes::from(vec![0_u8; (16 << 20) + 1]);
    let rest = stream::once(async move {
        released.await.unwrap();
        Ok(Bytes::from_static(b"tail"))
    });
    let body: BodyStream = stream::iter([Ok(first)]).chain(rest).boxed();
    let put =
        tokio::spawn(async move { s.put_stream_if_match_unsized(key(), body, "\"old\"").await });

    let create = heads.recv().await.unwrap();
    assert!(
        create.starts_with("POST /test-bucket/cas/seg-1?uploads= "),
        "{create}"
    );
    let part = heads.recv().await.unwrap().to_ascii_lowercase();
    let first_part = "put /test-bucket/cas/seg-1?partnumber=1&uploadid=u1 ";
    assert!(part.starts_with(first_part), "{part}");
    assert!(part.contains("\r\ncontent-length: 16777216\r\n"), "{part}");
    assert!(
        !put.is_finished(),
        "the call returned before the producer was released"
    );

    release.send(()).unwrap();
    let part = heads.recv().await.unwrap().to_ascii_lowercase();
    let second_part = "put /test-bucket/cas/seg-1?partnumber=2&uploadid=u1 ";
    assert!(part.starts_with(second_part), "{part}");
    assert!(part.contains("\r\ncontent-length: 5\r\n"), "{part}");
    let complete = heads.recv().await.unwrap().to_ascii_lowercase();
    assert!(
        complete.starts_with("post /test-bucket/cas/seg-1?uploadid=u1 "),
        "{complete}"
    );
    assert!(complete.contains("\r\nif-match: \"old\"\r\n"), "{complete}");
    assert_eq!(put.await.unwrap().unwrap(), "\"joined-2\"");
}

/// Asserts that an unsized body of at most one part is one PUT with `If-Match` and no multipart
/// request, whether it is empty, short, or exactly one part, and that it gives the answer's tag.
#[tokio::test]
async fn an_unsized_streamed_replace_of_at_most_one_part_is_one_put_with_if_match() {
    for len in [0_u64, 3, 16 << 20] {
        let done = answer("200 OK", &["etag: \"new\""], "");
        let (endpoint, seen) = script(vec![done]).await;
        let s = store(endpoint);
        let put = s.put_stream_if_match_unsized(key(), zeros(len, None), "\"old\"");
        assert_eq!(put.await.unwrap(), "\"new\"", "{len}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{len}");
        let put = seen[0].to_ascii_lowercase();
        assert!(put.starts_with("put /test-bucket/cas/seg-1 "), "{put}");
        assert!(put.contains("\r\nif-match: \"old\"\r\n"), "{put}");
        // An empty body states no length; any other states its own.
        let stated = put.contains(&format!("\r\ncontent-length: {len}\r\n"));
        assert_eq!(stated, len > 0, "{put}");
    }
}

/// Asserts that an unsized body that fails before its first part is full sends no request, and
/// one that fails after the first part was sent aborts the upload and gives the body's error.
#[tokio::test]
async fn an_unsized_streamed_replace_whose_body_fails_replaces_nothing() {
    let cut = ObjectStoreError::Body("cut".to_owned());
    let (endpoint, seen) = script(Vec::new()).await;
    let s = store(endpoint);
    let failing = zeros(2 << 20, Some(cut.clone()));
    let failed = s.put_stream_if_match_unsized(key(), failing, "\"old\"");
    assert_eq!(failed.await.unwrap_err().to_string(), cut.to_string());
    assert_eq!(seen.lock().unwrap().len(), 0);

    let id =
        "<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>";
    let answers = vec![
        answer("200 OK", &[], id),
        answer("200 OK", &["etag: \"p1\""], ""),
        answer("204 No Content", &[], ""),
    ];
    let (endpoint, seen) = script(answers).await;
    let s = store(endpoint);
    let first = Bytes::from(vec![0_u8; (16 << 20) + 1]);
    let failing: BodyStream = stream::iter(vec![Ok(first), Err(cut.clone())]).boxed();
    let failed = s.put_stream_if_match_unsized(key(), failing, "\"old\"");
    assert_eq!(failed.await.unwrap_err().to_string(), cut.to_string());
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[2].starts_with("DELETE "), "{seen:?}");
}

/// Asserts that a completion the store refuses (412, or a 200 that names no tag) is typed and the
/// upload is aborted after a refusal.
#[tokio::test]
async fn an_unsized_streamed_replace_refused_at_completion_is_typed_and_aborted() {
    let refused = answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code></Error>",
    );
    let (endpoint, seen) = script(upload_answers(refused, true)).await;
    let s = store(endpoint);
    let body = zeros(OVER_ONE_PUT, None);
    let put = s.put_stream_if_match_unsized(key(), body, "\"old\"");
    let err = put.await.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 412, .. }),
        "{err}"
    );
    assert!(seen.lock().unwrap()[4].starts_with("DELETE "));

    let unnamed = "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>";
    let (endpoint, _) = script(upload_answers(answer("200 OK", &[], unnamed), false)).await;
    let s = store(endpoint);
    let body = zeros(OVER_ONE_PUT, None);
    let err = s
        .put_stream_if_match_unsized(key(), body, "\"old\"")
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
}

/// Asserts that a streamed create of at most one PUT's size is one PUT with `If-None-Match: *`
/// and no `If-Match`, that it gives the `ETag` of the answer, that a body that fails or has
/// another length sends no request, and that an object that is already there is typed.
#[tokio::test]
async fn a_small_streamed_create_is_one_put_with_if_none_match_and_gives_its_tag() {
    let done = answer("200 OK", &["etag: \"made\""], "");
    let (endpoint, seen) = script(vec![done]).await;
    let s = store(endpoint);
    let put = s.put_stream_new_tagged(key(), 3 << 20, zeros(3 << 20, None));
    assert_eq!(put.await.unwrap(), "\"made\"");
    let put = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(put.starts_with("put /test-bucket/cas/seg-1 "), "{put}");
    assert!(put.contains("\r\nif-none-match: *\r\n"), "{put}");
    assert!(!put.contains("if-match: "), "{put}");

    // The request is begun and its body cut: the server never has the whole of it, and answers nothing.
    let (endpoint, seen_cut) = script(vec![answer("200 OK", &[], "")]).await;
    let s = store(endpoint);
    let cut = ObjectStoreError::Body("cut".to_owned());
    let failing = zeros(2 << 20, Some(cut.clone()));
    let failed = s.put_stream_new_tagged(key(), 2 << 20, failing);
    assert_eq!(failed.await.unwrap_err().to_string(), cut.to_string());
    let (endpoint, seen_short) = script(vec![answer("200 OK", &[], "")]).await;
    let s = store(endpoint);
    let short = s.put_stream_new_tagged(key(), 100, zeros(10, None));
    assert!(matches!(short.await, Err(ObjectStoreError::Body(_))));
    assert_eq!(seen_cut.lock().unwrap().len(), 0);
    assert_eq!(seen_short.lock().unwrap().len(), 0);

    let refused = answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code></Error>",
    );
    let (endpoint, _) = script(vec![refused]).await;
    let s = store(endpoint);
    let put = s.put_stream_new_tagged(key(), 3, zeros(3, None));
    let err = put.await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");
}

/// Asserts that a streamed create of more than one PUT's size is a multipart upload whose
/// completion alone carries `If-None-Match: *`, that it gives the `ETag` the completion names,
/// that a completion with no `ETag` is a protocol error, and that the upload is aborted when the
/// completion fails.
#[tokio::test]
async fn a_large_streamed_create_is_completed_create_only_and_gives_its_tag() {
    let ok =
        "<CompleteMultipartUploadResult><ETag>\"joined-2\"</ETag></CompleteMultipartUploadResult>";
    let (endpoint, seen) = script(upload_answers(answer("200 OK", &[], ok), false)).await;
    let s = store_cut_at_a_part(endpoint);
    let put = s.put_stream_new_tagged(key(), OVER_ONE_PUT, zeros(OVER_ONE_PUT, None));
    assert_eq!(put.await.unwrap(), "\"joined-2\"");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    for (at, request) in requests.iter().enumerate() {
        let request = request.to_ascii_lowercase();
        assert_eq!(
            request.contains("\r\nif-none-match: *\r\n"),
            at == 3,
            "{request}"
        );
        assert!(!request.contains("if-match: "), "{request}");
    }

    let unnamed = "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>";
    let (endpoint, _) = script(upload_answers(answer("200 OK", &[], unnamed), false)).await;
    let s = store_cut_at_a_part(endpoint);
    let put = s.put_stream_new_tagged(key(), OVER_ONE_PUT, zeros(OVER_ONE_PUT, None));
    let err = put.await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");

    let refused = answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code></Error>",
    );
    let (endpoint, seen) = script(upload_answers(refused, true)).await;
    let s = store_cut_at_a_part(endpoint);
    let put = s.put_stream_new_tagged(key(), OVER_ONE_PUT, zeros(OVER_ONE_PUT, None));
    let err = put.await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");
    assert!(seen.lock().unwrap()[4].starts_with("DELETE "));
}

/// Asserts that a create-only streamed write that is answered, not tagged, gives what the answer of
/// the PUT says: the `ETag` and the `Last-Modified` it carries, each absent when the answer has
/// none (an answer with no `ETag` is a made object, not a protocol error as it is for the tagged
/// write), that the request is the create-only PUT, that an object that exists is typed, and that a
/// store without conditional writes refuses it before any request.
#[tokio::test]
async fn a_create_only_write_gives_the_etag_and_time_of_the_answer_when_it_has_them() {
    let stamp = "last-modified: Tue, 03 Nov 2026 08:49:37 GMT";
    let both = answer("200 OK", &["etag: \"made\"", stamp], "");
    let (endpoint, seen) = script(vec![both]).await;
    let s = store(endpoint);
    let put = s.put_stream_new_answered(key(), 3, zeros(3, None));
    let got = put.await.unwrap();
    assert_eq!(got.etag.as_deref(), Some("\"made\""));
    assert_eq!(got.modified_millis, Some(1_793_695_777_000));
    let head = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.contains("\r\nif-none-match: *\r\n"), "{head}");

    let (endpoint, _) = script(vec![answer("200 OK", &[], "")]).await;
    let s = store(endpoint);
    let put = s.put_stream_new_answered(key(), 3, zeros(3, None));
    let got = put.await.unwrap();
    assert_eq!((got.etag, got.modified_millis), (None, None));

    let refused = answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code></Error>",
    );
    let (endpoint, _) = script(vec![refused]).await;
    let s = store(endpoint);
    let put = s.put_stream_new_answered(key(), 3, zeros(3, None));
    let err = put.await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");

    let plain = S3Store::new(S3Config {
        endpoint: "http://127.0.0.1:1".to_owned(),
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    })
    .unwrap();
    let err = plain
        .put_stream_new_answered(key(), 3, zeros(3, None))
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Unsupported(_)), "{err}");
}

/// Asserts that the completion of a multipart upload gives its `ETag` as the answer of the write,
/// no time (a completion's answer has none), and that a completion whose answer names no `ETag` is
/// a made object with no tag, not an error.
#[tokio::test]
async fn a_multipart_create_gives_the_etag_of_the_completion_and_no_time() {
    let ok =
        "<CompleteMultipartUploadResult><ETag>\"joined-2\"</ETag></CompleteMultipartUploadResult>";
    let (endpoint, _) = script(upload_answers(answer("200 OK", &[], ok), false)).await;
    let s = store_cut_at_a_part(endpoint);
    let put = s.put_stream_new_answered(key(), OVER_ONE_PUT, zeros(OVER_ONE_PUT, None));
    let got = put.await.unwrap();
    assert_eq!(got.etag.as_deref(), Some("\"joined-2\""));
    assert_eq!(got.modified_millis, None);

    let bare = "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>";
    let (endpoint, _) = script(upload_answers(answer("200 OK", &[], bare), false)).await;
    let s = store_cut_at_a_part(endpoint);
    let put = s.put_stream_new_answered(key(), OVER_ONE_PUT, zeros(OVER_ONE_PUT, None));
    let got = put.await.unwrap();
    assert_eq!((got.etag, got.modified_millis), (None, None));
}
