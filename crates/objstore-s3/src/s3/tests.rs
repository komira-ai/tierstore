use super::*;

pub(super) fn r(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).unwrap()
}

/// Catches: accepting a 206 for different bytes than asked (a store that rounds a
/// range to its own block size), a body shorter than its header says, or a range cut
/// at the end refused.
#[test]
fn partial_answers_must_match_the_range() {
    assert!(check_partial(Some("bytes 10-19/100"), r(10, 10), 10).is_ok());
    assert!(check_partial(Some("bytes 95-99/100"), r(95, 10), 5).is_ok());
    assert!(check_partial(Some("bytes 0-19/100"), r(10, 10), 20).is_err());
    assert!(check_partial(Some("bytes 10-19/100"), r(10, 10), 9).is_err());
    assert!(check_partial(Some("bytes 10-29/100"), r(10, 10), 20).is_err());
    assert!(check_partial(Some("bytes */100"), r(10, 10), 0).is_err());
    assert!(check_partial(None, r(0, 1), 1).is_err());
}

/// Catches: a timeout that ignores the body (the flat 60 s that cut a 128 MiB PUT
/// on a slow store), one that scales a body-less request, or arithmetic that
/// overflows on a huge body or a rate of one byte a second.
#[test]
fn the_timeout_grows_with_the_body_at_the_floor_rate() {
    let mut c = config("http://127.0.0.1:9000", "test-bucket");
    c.request_timeout = Duration::from_secs(60);
    let flat = S3Store::new(c.clone()).unwrap();
    assert_eq!(flat.conn.timeout_for(128 << 20), Duration::from_secs(60));
    c.min_body_rate = NonZeroU64::new(1 << 20);
    let scaled = S3Store::new(c.clone()).unwrap();
    assert_eq!(scaled.conn.timeout_for(0), Duration::from_secs(60));
    assert_eq!(scaled.conn.timeout_for(128 << 20), Duration::from_secs(188));
    assert_eq!(
        scaled.conn.timeout_for(512 << 10),
        Duration::from_millis(60_500)
    );
    c.min_body_rate = NonZeroU64::new(1);
    let slowest = S3Store::new(c).unwrap();
    let saturated = Duration::from_secs(60) + Duration::from_nanos(u64::MAX);
    assert_eq!(slowest.conn.timeout_for(usize::MAX), saturated);
}

/// Catches: the idle pool timeout reverting to reqwest's 90 s default, which lets a
/// request reuse a connection the server has already closed.
#[test]
fn pool_idle_timeout_is_30s() {
    assert_eq!(
        S3Tuning::default().pool_idle_timeout,
        Duration::from_secs(30)
    );
}

/// Catches: the builder not applying the idle timeout it is given (the default is
/// 90 s, so a connection idle past the limit, here for a second of the pool's clock,
/// would be reused, not replaced).
#[tokio::test]
async fn idle_connection_is_dropped_after_the_timeout() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let answer = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
                    let _ = sock.write_all(answer).await;
                }
            });
        }
    });
    let limit = Duration::from_secs(5);
    // Reuse: a long idle timeout keeps the connection across two requests.
    let keeping = client_builder(limit, Duration::from_secs(60))
        .build()
        .unwrap();
    keeping.get(&url).send().await.unwrap();
    keeping.get(&url).send().await.unwrap();
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "a fresh connection is reused"
    );
    // Expiry: a short idle timeout and a jump of the clock far above it replace the
    // connection. The jump is tokio's clock, which the pool reads; nothing waits.
    let dropping = client_builder(limit, Duration::from_millis(100))
        .build()
        .unwrap();
    dropping.get(&url).send().await.unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    dropping.get(&url).send().await.unwrap();
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        3,
        "an idle connection was kept"
    );
}

/// The error reqwest gives for a response whose body delivers a few bytes and then
/// fails with `kind`, read as `get_range_once` reads one, wrapped as it wraps it.
async fn body_error(kind: std::io::ErrorKind) -> (ObjectStoreError, bool) {
    struct Failing {
        kind: std::io::ErrorKind,
        sent: bool,
    }
    impl http_body::Body for Failing {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            let sent = std::mem::replace(&mut self.sent, true);
            std::task::Poll::Ready(Some(if sent {
                Err(std::io::Error::from(self.kind))
            } else {
                Ok(http_body::Frame::data(Bytes::from_static(b"part")))
            }))
        }
    }
    let body = reqwest::Body::wrap(Failing { kind, sent: false });
    let mut resp = reqwest::Response::from(http::Response::new(body));
    let first = resp.chunk().await.unwrap();
    assert_eq!(first.as_deref(), Some(&b"part"[..]));
    let e = resp.chunk().await.unwrap_err();
    let timeout = e.is_timeout();
    (transport(e), timeout)
}

/// Catches: a read whose connection the kernel timed out mid-body (`ETIMEDOUT`,
/// which reqwest also reports as `is_timeout`) never sent again, failing a large
/// download that a fresh connection would finish; and the same for a failure that
/// is no timeout at all being refused.
#[tokio::test]
async fn a_body_cut_by_an_os_level_timeout_is_resent() {
    let (e, timeout) = body_error(std::io::ErrorKind::TimedOut).await;
    assert!(timeout, "reqwest must call this a timeout: {e:?}");
    assert!(is_retryable(&e));
    let (e, timeout) = body_error(std::io::ErrorKind::ConnectionReset).await;
    assert!(!timeout);
    assert!(is_retryable(&e));
}

/// Catches: the client's own connect limit, which ends as an `io::Error` of kind
/// `TimedOut` in the chain, taken for a dead connection and sent again; and a
/// refused connection taken for a timeout.
// Linux drops a SYN when the accept queue is full; BSD and macOS may reset instead.
#[cfg(target_os = "linux")]
#[tokio::test(start_paused = true)]
async fn a_connect_that_times_out_is_not_resent_but_a_refused_one_is() {
    // A listener whose accept queue is full drops the next SYN: the connect hangs.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    let _listener = socket.listen(1).unwrap();
    // Four connects, each polled once: the poll sends the SYN, and the kernel has
    // answered or dropped it before the poll returns. The connects are kept, finished
    // or not, so no socket closes and the queue stays full.
    let mut fillers = Vec::new();
    for _ in 0..4 {
        let filler = tokio::net::TcpSocket::new_v4().unwrap();
        let mut connecting = Box::pin(filler.connect(addr));
        let _ = futures::poll!(connecting.as_mut());
        fillers.push(connecting);
    }
    let client = client_builder(
        Duration::from_millis(100),
        S3Tuning::default().pool_idle_timeout,
    )
    .build()
    .unwrap();
    let e = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap_err();
    assert!(e.is_connect(), "{e:?}");
    assert!(e.is_timeout(), "{e:?}");
    assert!(!is_retryable(&transport(e)));

    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gone = closed.local_addr().unwrap();
    drop(closed);
    let e = client
        .get(format!("http://{gone}/"))
        .send()
        .await
        .unwrap_err();
    assert!(e.is_connect(), "{e:?}");
    assert!(!e.is_timeout(), "{e:?}");
    assert!(is_retryable(&transport(e)));
}

/// Catches: every non-connect reqwest error sent again, including the client's own
/// per-request limit, which ends a read against a store that accepts and never
/// answers and would multiply the wait of a read that cannot succeed.
#[tokio::test(start_paused = true)]
async fn a_request_that_exceeds_its_client_timeout_is_not_resent() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _silent = tokio::spawn(async move {
        let held = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(held);
    });
    let e = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .timeout(Duration::from_millis(100))
        .send()
        .await
        .unwrap_err();
    assert!(e.is_timeout(), "{e:?}");
    assert!(!e.is_connect(), "{e:?}");
    assert!(!is_retryable(&transport(e)));
}

/// Catches: this module's own stall treated as worth a resend.
#[test]
fn a_stall_is_not_resent() {
    assert!(!is_retryable(&ObjectStoreError::Transport(Arc::new(
        Stalled
    ))));
}

/// Catches: a transport error of the timeout kind from reqwest, or this module's
/// own stall, treated as worth a resend; and any other transport error as not.
#[test]
fn only_a_stall_or_timeout_is_not_resent() {
    let stalled = ObjectStoreError::Transport(Arc::new(Stalled));
    assert!(!is_retryable(&stalled));
    let reset = ObjectStoreError::Transport(Arc::new(std::io::Error::other("reset")));
    assert!(is_retryable(&reset));
}

pub(super) fn config(endpoint: &str, bucket: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: bucket.to_owned(),
        credentials: Credentials::new("a", "s"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_secs(1),
        min_body_rate: None,
    }
}

/// Catches: an endpoint whose path, scheme or credentials would make the signed
/// request differ from the one sent, and bucket names S3 refuses.
#[test]
fn config_is_checked() {
    assert!(S3Store::new(config("http://127.0.0.1:9000", "example-bucket")).is_ok());
    for bad in [
        "https://127.0.0.1:9000",
        "ftp://127.0.0.1",
        "http://127.0.0.1:9000/base",
        "http://u:p@127.0.0.1:9000",
        "http://:p@127.0.0.1:9000",
        "http://127.0.0.1:9000/#frag",
        "http://127.0.0.1:9000/?x=1",
        "127.0.0.1:9000",
    ] {
        assert!(
            matches!(
                S3Store::new(config(bad, "example-bucket")),
                Err(S3ConfigError::Endpoint { .. })
            ),
            "{bad}"
        );
    }
    for bad in [
        "ab",
        "Bucket",
        "-bucket",
        "bucket-",
        "my_bucket",
        &"a".repeat(64),
    ] {
        assert!(
            matches!(
                S3Store::new(config("http://127.0.0.1", bad)),
                Err(S3ConfigError::Bucket(_))
            ),
            "{bad}"
        );
    }
}

/// Catches: a secret printed by `Debug` into a log.
#[test]
fn debug_hides_the_secret() {
    let text = format!("{:?}", config("http://127.0.0.1", "example-bucket"));
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("\"s\""), "{text}");
}

/// Catches: the store's `Debug` (it is logged) that prints the credentials or leaves
/// out which endpoint, bucket and capabilities it addresses.
#[test]
fn the_store_debug_names_its_bucket_and_hides_its_credentials() {
    let mut c = config("http://127.0.0.1:9000", "example-bucket");
    c.credentials = Credentials::new("AKIDSECRETISH", "hunter2");
    let text = format!("{:?}", S3Store::new(c).unwrap());
    assert!(text.starts_with("S3Store {"), "{text}");
    assert!(text.contains("base: \"http://127.0.0.1:9000/\""), "{text}");
    assert!(text.contains("bucket: \"example-bucket\""), "{text}");
    assert!(text.contains("capabilities:"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("AKIDSECRETISH"), "{text}");
}

/// Catches: bucket-name edges: a dot inside, a digit at either end, and the exact
/// length limits (3 and 63) refused or accepted wrongly.
#[test]
fn bucket_names_allow_dots_digits_and_the_length_limits() {
    for ok in ["my.bucket1", "1bucket", "bucket1", "abc", &"a".repeat(63)] {
        assert!(valid_bucket(ok), "{ok}");
    }
    for bad in [
        "",
        "ab",
        "my.bucket.",
        ".bucket",
        "my bucket",
        "my/bucket",
        &"a".repeat(64),
    ] {
        assert!(!valid_bucket(bad), "{bad}");
    }
}

/// Catches: a 206 whose Content-Range is malformed in each field, or that names the
/// right first byte and wrong last (or inverted) bytes, accepted as the window asked.
#[test]
fn partial_answers_with_malformed_or_inverted_ranges_are_refused() {
    for cr in [
        "items 0-9/10",
        "bytes 0-9",
        "bytes 0/10",
        "bytes a-9/10",
        "bytes 0-b/10",
        "bytes 0-9/c",
    ] {
        let why = check_partial(Some(cr), r(0, 10), 10).unwrap_err();
        assert!(why.contains("does not parse"), "{cr}: {why}");
    }
    let why = check_partial(Some("bytes 0-4/10"), r(0, 10), 5).unwrap_err();
    assert!(why.contains("asked for bytes 0-9"), "{why}");
    let why = check_partial(Some("bytes 5-4/5"), r(5, 1), 0).unwrap_err();
    assert!(why.contains("asked for bytes 5-5"), "{why}");
    let why = check_partial(Some("bytes 0-9/10"), r(0, 10), 9).unwrap_err();
    assert!(why.contains("the body has 9 bytes"), "{why}");
}
