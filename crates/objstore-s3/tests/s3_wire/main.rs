//! The S3 backend against a scripted HTTP server on loopback: what it sends, and what it
//! refuses to believe. The real-store runs (MinIO, RustFS) are in CI; these pin the
//! cases a real store does not produce on demand.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _};
use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{
    ByteRange, Capabilities, KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError,
    PageSize, listing,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

mod timeouts;
mod transport;

/// Serves `responses` in order, one per connection, and records each request head.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let endpoint = format!("http://{addr}");
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

fn store(endpoint: String, capabilities: Capabilities) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    })
    .unwrap()
}

fn key() -> ObjectKey {
    ObjectKey::new("cas/seg-1").unwrap()
}

/// Catches: a client that accepts a 200 to a ranged GET (a server or proxy that ignores
/// Range), or a 206 whose Content-Range names other bytes, and hands the wrong bytes to
/// the caller as if they were the window asked for.
#[tokio::test]
async fn a_server_ignoring_range_is_refused() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], "0123456789"),
        answer(
            "206 Partial Content",
            &["content-range: bytes 0-3/10"],
            "0123",
        ),
        answer(
            "206 Partial Content",
            &["content-range: bytes 2-5/10"],
            "2345",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(2, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Protocol(m) if m.contains("ignored Range")),
        "{err}"
    );
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
    assert_eq!(s.get_range(&key(), r).await.unwrap(), Bytes::from("2345"));

    let heads = seen.lock().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(
        first.starts_with("get /test-bucket/cas/seg-1 http/1.1\r\n"),
        "path-style: {first}"
    );
    assert!(first.contains("\r\nrange: bytes=2-5\r\n"), "{first}");
    assert!(
        first.contains("signedheaders=host;range;x-amz-content-sha256;x-amz-date"),
        "{first}"
    );
}

/// Serves `head_and_body` to the first connection and then keeps the connection open without
/// ending the body, so that a client that waits for more of the body does not get an answer.
async fn script_open(head_and_body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut seen = Vec::new();
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "client closed before sending a request");
            seen.extend_from_slice(&buf[..n]);
        }
        sock.write_all(head_and_body.as_bytes()).await.unwrap();
        std::future::pending::<()>().await;
    });
    endpoint
}

/// Catches: a ranged read whose answer says it has more bytes than were asked for being read
/// anyway (the answer here gives 4 of the 40 bytes it announces and holds the connection, so a
/// client that reads the body waits for the rest): the read fails on the head, as a protocol
/// error that names the length.
#[tokio::test]
async fn a_content_length_past_the_range_is_refused_before_the_body() {
    let head = "HTTP/1.1 206 Partial Content\r\ncontent-length: 40\r\n\
                content-range: bytes 0-3/100\r\n\r\n0123";
    let endpoint = script_open(head.to_owned()).await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(0, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Protocol(m) if m.contains("longer than")),
        "{err}"
    );
}

/// Catches: a ranged read whose body has no length (chunked) and goes on past the bytes asked for
/// being read to its end: the answer here sends 5 bytes of a range of 4 and holds the connection
/// without ending the body, so the read fails as soon as the fifth byte arrives, as a protocol
/// error that names the length.
#[tokio::test]
async fn a_chunked_body_past_the_range_is_refused_at_the_byte_past_it() {
    let head = "HTTP/1.1 206 Partial Content\r\ntransfer-encoding: chunked\r\n\
                content-range: bytes 0-3/100\r\n\r\n5\r\n01234\r\n";
    let endpoint = script_open(head.to_owned()).await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(0, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Protocol(m) if m.contains("longer than")),
        "{err}"
    );
}

/// Catches: a put that does not ask for a conditional write when the store claims it,
/// or reads 412 as anything but `AlreadyExists`; and a retained put without the lock
/// headers and the checksum S3 requires.
#[tokio::test]
async fn put_new_sends_conditions_and_retention() {
    let (endpoint, seen) = script(vec![
        answer("412 Precondition Failed", &[], ""),
        answer("200 OK", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: true,
            object_lock: true,
        },
    );
    let err = s
        .put_new(&key(), Bytes::from("abc"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");
    let until = std::time::UNIX_EPOCH + Duration::from_secs(4_107_542_400);
    s.put_new(&key(), Bytes::from("abc"), Some(until))
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    let put = heads[1].to_ascii_lowercase();
    assert!(put.contains("\r\nif-none-match: *\r\n"), "{put}");
    assert!(
        put.contains("\r\nx-amz-object-lock-mode: compliance\r\n"),
        "{put}"
    );
    assert!(
        put.contains("\r\nx-amz-object-lock-retain-until-date: 2100-03-01t00:00:00z\r\n"),
        "{put}"
    );
    // MD5("abc"), base64.
    assert!(
        put.contains(
            "\r\ncontent-md5: kAFQmDzST7DWlj99KOF/cg==\r\n"
                .to_ascii_lowercase()
                .as_str()
        ),
        "{put}"
    );
    // SHA-256("abc"), the signed payload hash.
    assert!(
        put.contains(
            "x-amz-content-sha256: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ),
        "{put}"
    );
}

/// Catches: a delete on an Object Lock store sent without the version id, which S3
/// answers by adding a delete marker (success) while keeping the bytes; and a delete of
/// an absent key reported as an error.
#[tokio::test]
async fn delete_on_a_locked_store_targets_the_version() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &["x-amz-version-id: v+1/x"], ""),
        answer(
            "403 Forbidden",
            &[],
            "<Error><Code>AccessDenied</Code><Message>locked</Message></Error>",
        ),
        answer("404 Not Found", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    let err = s.delete(&key()).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 403, code, .. } if code == "AccessDenied"),
        "{err}"
    );
    s.delete(&key()).await.unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("HEAD /test-bucket/cas/seg-1 HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with("DELETE /test-bucket/cas/seg-1?versionId=v%2B1%2Fx HTTP/1.1"),
        "{}",
        heads[1]
    );
    assert!(heads[2].starts_with("HEAD "), "{}", heads[2]);
}

/// Catches: a list request that is not ListObjectsV2, loses the continuation token or
/// encodes the query differently from how it was signed.
#[tokio::test]
async fn list_sends_the_token_and_reads_the_next_one() {
    let page = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                <NextContinuationToken>t2</NextContinuationToken>\
                <Contents><Key>p/a</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], page),
        answer("200 OK", &[], page),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let first = s
        .list(&prefix, None, PageSize::new(3).unwrap())
        .await
        .unwrap();
    let token = first.next.expect("a truncated page has a token");
    s.list(&prefix, Some(&token), PageSize::new(3).unwrap())
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("GET /test-bucket?list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with(
            "GET /test-bucket?continuation-token=t2&list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}

/// Catches: a listing [`ListToken::after`] a key that does not send it as
/// `start-after` (S3 would read it as a continuation token and refuse it, or list from
/// the start), or sends it as well as a continuation token on the pages that follow.
#[tokio::test]
async fn list_after_a_key_sends_start_after_then_the_token() {
    let page = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                <NextContinuationToken>t2</NextContinuationToken>\
                <Contents><Key>p/c</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], page),
        answer("200 OK", &[], page),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let after = ListToken::after(&ObjectKey::new("p/b").unwrap());
    let first = s
        .list(&prefix, Some(&after), PageSize::new(3).unwrap())
        .await
        .unwrap();
    s.list(&prefix, first.next.as_ref(), PageSize::new(3).unwrap())
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with(
            "GET /test-bucket?list-type=2&max-keys=3&prefix=p%2F&start-after=p%2Fb HTTP/1.1"
        ),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with(
            "GET /test-bucket?continuation-token=t2&list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}

/// Catches: a listing stream over S3 that does not carry the continuation token from
/// page to page, or stops at a truncated page: two pages, each asked for once, the
/// second with the first's token, every key in order.
#[tokio::test]
async fn the_listing_stream_follows_continuation_tokens() {
    let first = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                 <NextContinuationToken>t2</NextContinuationToken>\
                 <Contents><Key>p/a</Key><Size>1</Size></Contents>\
                 <Contents><Key>p/b/c</Key><Size>2</Size></Contents></ListBucketResult>";
    let last = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                <Contents><Key>p/c</Key><Size>3</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], first),
        answer("200 OK", &[], last),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let got: Vec<(String, u64)> = listing::objects(&s, &prefix, None, PageSize::new(2).unwrap())
        .map_ok(|o| (o.key.as_str().to_owned(), o.size))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        got,
        [("p/a".into(), 1), ("p/b/c".into(), 2), ("p/c".into(), 3)]
    );
    let heads = seen.lock().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(
        heads[1].starts_with(
            "GET /test-bucket?continuation-token=t2&list-type=2&max-keys=2&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}

/// Catches: a listing stream resumed [`ListToken::after`] a key over S3 that restarts at
/// the start of the prefix (no `start-after`), sends the key again with the
/// continuation token, or believes a server that lists the key itself (an inclusive
/// `start-after`, which would hand a rebuild a segment twice): the stream sends
/// `start-after`, then only the token, and yields exactly the keys after the key; a
/// server answering with the key itself ends the stream with `Protocol`.
#[tokio::test]
async fn a_listing_resumed_after_a_key_lists_only_the_rest_over_s3() {
    let first = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                 <NextContinuationToken>t2</NextContinuationToken>\
                 <Contents><Key>p/c</Key><Size>1</Size></Contents>\
                 <Contents><Key>p/d</Key><Size>2</Size></Contents></ListBucketResult>";
    let last = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                <Contents><Key>p/e</Key><Size>3</Size></Contents></ListBucketResult>";
    let inclusive = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                     <Contents><Key>p/b</Key><Size>1</Size></Contents>\
                     <Contents><Key>p/c</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], first),
        answer("200 OK", &[], last),
        answer("200 OK", &[], inclusive),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let from = ListToken::after(&ObjectKey::new("p/b").unwrap());
    let size = PageSize::new(2).unwrap();
    let got: Vec<String> = listing::objects(&s, &prefix, Some(from.clone()), size)
        .map_ok(|o| o.key.as_str().to_owned())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(got, ["p/c", "p/d", "p/e"]);
    {
        let heads = seen.lock().unwrap();
        assert!(
            heads[0].starts_with(
                "GET /test-bucket?list-type=2&max-keys=2&prefix=p%2F&start-after=p%2Fb HTTP/1.1"
            ),
            "{}",
            heads[0]
        );
        assert!(
            heads[1].starts_with(
                "GET /test-bucket?continuation-token=t2&list-type=2&max-keys=2&prefix=p%2F HTTP/1.1"
            ),
            "{}",
            heads[1]
        );
    }
    let refused: Result<Vec<_>, _> = listing::objects(&s, &prefix, Some(from), size)
        .try_collect()
        .await;
    match refused {
        Err(ObjectStoreError::Protocol(m)) => assert!(m.contains("p/b after p/b"), "{m}"),
        other => panic!("expected Protocol, got {other:?}"),
    }
}

/// Catches: a listing failure mapped to the wrong error, or to an empty page (a rebuild
/// would take an unreadable bucket for an empty one): an S3 error is `Service` with its
/// status and code; an answer that is not a ListObjectsV2 result, or that lists a key
/// the crate cannot name (non-ASCII here: an object it did not write), is `Protocol`.
#[tokio::test]
async fn listing_failures_are_typed() {
    let denied = "<Error><Code>AccessDenied</Code><Message>no</Message></Error>";
    let missing = "<Error><Code>NoSuchBucket</Code><Message>gone</Message></Error>";
    let foreign = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                   <Contents><Key>p/caf\u{e9}</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, _) = script(vec![
        answer("403 Forbidden", &[], denied),
        answer("404 Not Found", &[], missing),
        answer("200 OK", &[], "<html>not s3</html>"),
        answer("200 OK", &[], foreign),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(s.list(&prefix, None, PageSize::MAX).await.unwrap_err());
    }
    assert!(
        matches!(&answers[0], ObjectStoreError::Service { status: 403, code, .. } if code == "AccessDenied"),
        "{:?}",
        answers[0]
    );
    assert!(
        matches!(&answers[1], ObjectStoreError::Service { status: 404, code, .. } if code == "NoSuchBucket"),
        "{:?}",
        answers[1]
    );
    assert!(
        matches!(&answers[2], ObjectStoreError::Protocol(why) if why.contains("root element is <html>")),
        "{:?}",
        answers[2]
    );
    assert!(
        matches!(&answers[3], ObjectStoreError::Protocol(why) if why.contains("listed key") && why.contains('\u{e9}')),
        "{:?}",
        answers[3]
    );
}

/// Catches: `create_bucket` that does not ask for Object Lock when the store claims it
/// (a retained put would then fail) or asks when it does not; that takes a bucket this
/// key pair already owns as an error (setup is not repeatable); or that hides any other
/// refusal, such as a bucket name another account holds.
#[tokio::test]
async fn create_bucket_asks_for_object_lock_and_accepts_its_own_bucket() {
    let owned = "<Error><Code>BucketAlreadyOwnedByYou</Code><Message>yours</Message></Error>";
    let taken = "<Error><Code>BucketAlreadyExists</Code><Message>taken</Message></Error>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], ""),
        answer("409 Conflict", &[], owned),
        answer("409 Conflict", &[], taken),
    ])
    .await;
    let locked = store(
        endpoint.clone(),
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    locked.create_bucket().await.unwrap();
    let plain = store(endpoint, Capabilities::default());
    plain.create_bucket().await.unwrap();
    let err = plain.create_bucket().await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 409, code, .. } if code == "BucketAlreadyExists"),
        "{err}"
    );

    let heads = seen.lock().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(
        first.starts_with("put /test-bucket http/1.1\r\n"),
        "{first}"
    );
    assert!(
        first.contains("\r\nx-amz-bucket-object-lock-enabled: true\r\n"),
        "{first}"
    );
    let second = heads[1].to_ascii_lowercase();
    assert!(
        second.starts_with("put /test-bucket http/1.1\r\n"),
        "{second}"
    );
    assert!(!second.contains("object-lock"), "{second}");
}

/// A loopback port on which connecting is refused, and which no other test can take: its
/// socket is kept bound (see [`tierstore_testkit::port`]).
fn held_port() -> tierstore_testkit::port::Reserved {
    tierstore_testkit::port::Reserved::new()
}

/// Catches a held port that is free to take (the old bind-and-drop helper): a thief
/// binding the number succeeds, and the store would be the thief.
#[test]
fn a_held_port_cannot_be_taken_by_another_listener() {
    let held = held_port();
    assert!(tierstore_testkit::port::is_taken(held.addr()));
}

/// Catches: `wait_answering` returning before the store answers (no wait at all, or a
/// connection cut with no answer taken for an answer), and taking an error status for
/// "not ready" (a running S3 service answers the unsigned `GET /` with 403, so such a wait
/// would never end against a real store). The store cuts its first two connections with no
/// answer; the wait must still be running when the second has been cut, and end once the
/// store answers 403. The test waits for those events, never for a time: the pace of the
/// retries is the wait's own.
///
/// It spends about 0.5 s of real time in the wait's two `WAIT_RETRY` sleeps, and asserts
/// nothing about it.
#[tokio::test]
async fn wait_answering_waits_for_a_late_store_and_takes_403_as_an_answer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cut_twice = Arc::new(Notify::new());
    let told = Arc::clone(&cut_twice);
    let server = tokio::spawn(async move {
        let mut cut = 0;
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&chunk[..n]).into_owned();
            if cut < 2 {
                cut += 1;
                if cut == 2 {
                    told.notify_one();
                }
                continue;
            }
            let resp = answer(
                "403 Forbidden",
                &[],
                "<Error><Code>AccessDenied</Code></Error>",
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
            return head;
        }
    });
    let s = store(format!("http://{addr}"), Capabilities::default());
    // An hour is no bound on the test: the wait ends when the store answers.
    let mut wait = tokio::spawn(async move { s.wait_answering(Duration::from_secs(3600)).await });
    tokio::select! {
        ended = &mut wait => panic!("the wait ended while the store answered nothing: {ended:?}"),
        () = cut_twice.notified() => {}
    }
    wait.await.unwrap().unwrap();
    let head = server.await.unwrap().to_ascii_lowercase();
    assert!(head.starts_with("get / http/1.1\r\n"), "{head}");
}

/// Catches: `wait_answering` ignoring its deadline (looping for ever on a store that
/// never starts, which hangs the CI job until its timeout) or reporting success, and giving
/// up before the deadline. The clock is paused from the start, so the deadline passes in
/// virtual time: the wait must end with the last attempt's error once exactly `within` has
/// passed on that clock. A wait that ignores its deadline never ends, and the hang guard of
/// the test runner (`.config/nextest.toml`) fails the test.
#[tokio::test(start_paused = true)]
async fn wait_answering_gives_up_at_its_deadline() {
    let addr = tierstore_testkit::port::dead_addr();
    let s = store(format!("http://{addr}"), Capabilities::default());
    let within = Duration::from_millis(700);
    let started = tokio::time::Instant::now();
    let err = s.wait_answering(within).await.unwrap_err();
    let waited = started.elapsed();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert!(
        waited >= within && waited < within + objstore_s3::s3::WAIT_RETRY,
        "{waited:?}"
    );
}

/// Catches a request timeout reported without its cause: a PUT to a store that takes
/// the connection and never reads the body or answers must fail at the request timeout
/// with an error whose text says it timed out, so the failure a lease reports tells a
/// slow store from a reset one ("request to the store failed" alone did not). Also
/// catches the request URL in that text: it reaches REAPI clients, and must not name the
/// store's address, bucket or key.
///
/// The request timeout passes in virtual time: the clock is paused once the store has the
/// connection, from then on the client waits for nothing but its timer, and the paused clock
/// moves on to it. The outcome is forced by the store never reading or answering.
#[tokio::test]
async fn a_put_the_store_never_answers_fails_saying_it_timed_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let endpoint = format!("http://{addr}");
    // Holds each connection open, unread, until the test ends.
    let accepted = Arc::new(Notify::new());
    let told = Arc::clone(&accepted);
    let held = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
            told.notify_one();
        }
    });
    let s = S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    })
    .unwrap();
    let putting = tokio::spawn(async move {
        let k = key();
        s.put_new(&k, Bytes::from(vec![0u8; 4 << 20]), None).await
    });
    accepted.notified().await;
    tokio::time::pause();
    let err = putting.await.unwrap().unwrap_err();
    held.abort();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    let text = err.to_string();
    assert!(
        text.starts_with("request to the store failed: ") && text.contains("timed out"),
        "{text}"
    );
    for private in [addr.as_str(), "test-bucket", "cas/seg-1", "url"] {
        assert!(!text.contains(private), "{private:?} in {text}");
    }
}

fn timed_store(endpoint: String, base: Duration, rate: Option<u64>) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "test-bucket".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: base,
        min_body_rate: rate.and_then(std::num::NonZeroU64::new),
    })
    .unwrap()
}

/// Catches: a store that reports other capabilities than it was configured with (the
/// conformance suite and the writer pick their paths from them).
#[tokio::test]
async fn a_store_reports_the_capabilities_it_was_configured_with() {
    let claimed = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let s = store("http://127.0.0.1:9".into(), claimed);
    assert_eq!(s.capabilities(), claimed);
}

/// Catches: a retained put on a store that did not claim Object Lock sent anyway (the
/// retention header silently ignored by a store that cannot honour it, so the object is
/// deletable while the caller believes it is locked); nothing may reach the wire.
#[tokio::test]
async fn a_retained_put_without_object_lock_is_refused_before_the_wire() {
    let (endpoint, seen) = script(vec![]).await;
    let s = store(endpoint, Capabilities::default());
    let until = std::time::UNIX_EPOCH + Duration::from_secs(4_107_542_400);
    let err = s
        .put_new(&key(), Bytes::from("abc"), Some(until))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Unsupported("Object Lock retention")),
        "{err}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// Catches: a 412 on a store with no conditional puts read as "already exists" (it is
/// a service error: nothing sent a condition, so nothing can have failed one).
#[tokio::test]
async fn a_412_on_an_unconditional_put_is_a_service_error() {
    let (endpoint, _) = script(vec![answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code><Message>m</Message></Error>",
    )])
    .await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .put_new(&key(), Bytes::from("abc"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 412, code, .. } if code == "PreconditionFailed"),
        "{err}"
    );
}

/// Catches: a ranged read that turns an unexpected status (here 500) into data or into
/// NotFound: it is a `Service` error carrying the status and code.
#[tokio::test]
async fn a_ranged_read_failing_with_a_server_error_is_a_service_error() {
    let (endpoint, _) = script(vec![answer(
        "500 Internal Server Error",
        &[],
        "<Error><Code>InternalError</Code><Message>boom</Message></Error>",
    )])
    .await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .get_range(&key(), ByteRange::new(0, 4).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 500, code, message } if code == "InternalError" && message == "boom"),
        "{err}"
    );
}

/// Catches: a ranged read that reads a 404 as a service error (the caller could not
/// tell a lost object from a failing store), or a 416 as anything but `InvalidRange`
/// naming the key and the range asked for.
#[tokio::test]
async fn a_ranged_read_of_a_missing_key_or_past_the_end_is_typed() {
    let (endpoint, _) = script(vec![
        answer(
            "404 Not Found",
            &[],
            "<Error><Code>NoSuchKey</Code><Message>gone</Message></Error>",
        ),
        answer(
            "416 Range Not Satisfiable",
            &[],
            "<Error><Code>InvalidRange</Code><Message>past</Message></Error>",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(50, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::NotFound(k) if *k == key()),
        "{err}"
    );
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::InvalidRange { key: k, range } if *k == key() && *range == r),
        "{err}"
    );
}

/// Catches: a delete on an Object Lock store whose HEAD failed (here 500) carried on to
/// a DELETE blind (it may delete the wrong version, or report success for an object the
/// store refused to describe); the HEAD's error is the answer and no DELETE is sent.
#[tokio::test]
async fn a_failed_head_before_a_locked_delete_stops_the_delete() {
    let (endpoint, seen) = script(vec![answer("500 Internal Server Error", &[], "")]).await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    let err = s.delete(&key()).await.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 500, .. }),
        "{err}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Catches: the `null` version id (an unversioned object) or an empty one sent as
/// `versionId=...`, which S3 refuses; the DELETE must carry no version query then.
#[tokio::test]
async fn a_locked_delete_of_a_null_or_empty_version_sends_no_version_id() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &["x-amz-version-id: null"], ""),
        answer("204 No Content", &[], ""),
        answer("200 OK", &["x-amz-version-id:"], ""),
        answer("204 No Content", &[], ""),
        answer("200 OK", &[], ""),
        answer("204 No Content", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    for _ in 0..3 {
        s.delete(&key()).await.unwrap();
    }
    let heads = seen.lock().unwrap();
    for i in [1, 3, 5] {
        assert!(
            heads[i].starts_with("DELETE /test-bucket/cas/seg-1 HTTP/1.1"),
            "{}",
            heads[i]
        );
    }
}
