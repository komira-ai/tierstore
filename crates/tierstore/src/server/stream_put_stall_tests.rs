//! A put through `Server::handle` to an S3 tier that takes the connection and never answers: the
//! put is answered with the upstream's error when its request is cut and its write permit is free
//! then, so a stall of the upstream does not take every permit for the look or the abort that
//! follows a failed put (`objstore-s3`'s `s3/cleanup.rs`).
//!
//! The waits are not real. The upstream is a real socket, so the clock is paused only once the
//! upstream has read every request it is to hang (it says so): from then on the server has
//! nothing to wait for but its timers, and the paused clock moves on to them by itself. The
//! outcome is forced by the upstream never answering, not by how long anything took.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hyper::StatusCode;
use objstore_s3::s3::fake::FakeS3;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::s3::{Addressing, Capabilities, Credentials, S3Config};
use crate::{Backend as _, S3Backend, TestClock};

use super::auth::sign;
use super::testing::{BUCKET, KEY_ID, NOW, Reply, SECRET};
use super::{Server, ServerConfig};

/// The request timeout of the S3 tier: the default of `server.s3_request_timeout`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest a PUT that the store read whole and never answered takes to fail: the request is
/// cut after a whole `request_timeout` with no progress, and the window in which the body was
/// sent counts as progress, so it fails between one and two request timeouts after it began. A
/// put that also waited for the look after it (1 s, then a HEAD of up to a request timeout) took
/// at least 121 s, and up to 181 s.
const PUT_GIVES_UP: Duration = Duration::from_secs(120);

/// The keys whose requests the upstream hangs.
const STALLED: &str = "stall/";

/// The requests the upstream has read whole and hangs, and the signal of each.
#[derive(Default)]
struct Held {
    count: AtomicUsize,
    changed: Notify,
}

impl Held {
    /// Waits until the upstream hangs at least `n` requests.
    async fn reach(&self, n: usize) {
        loop {
            let changed = self.changed.notified();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            if self.count.load(Ordering::SeqCst) >= n {
                return;
            }
            changed.await;
        }
    }
}

/// An upstream on loopback in front of the loopback S3 service `s3`: a connection whose first
/// request is for a key under [`STALLED`] is read whole and never answered; any other connection
/// is passed to `s3` as it is.
async fn stalling_upstream(s3: &FakeS3) -> (String, Arc<Held>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let behind = s3.endpoint().trim_start_matches("http://").to_owned();
    let held = Arc::new(Held::default());
    let counted = Arc::clone(&held);
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            let (behind, counted) = (behind.clone(), Arc::clone(&counted));
            tokio::spawn(async move { route(sock, &behind, &counted).await });
        }
    });
    (endpoint, held)
}

/// Hangs or passes on the connection `sock` (see [`stalling_upstream`]).
async fn route(mut sock: TcpStream, behind: &str, held: &Held) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    if !head.lines().next().unwrap_or_default().contains(STALLED) {
        let mut upstream = TcpStream::connect(behind).await.unwrap();
        upstream.write_all(&buf).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut upstream).await;
        return;
    }
    let length: usize = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    held.count.fetch_add(1, Ordering::SeqCst);
    held.changed.notify_waiters();
    std::future::pending::<()>().await;
}

/// A server over an S3 tier at `endpoint`, with the default permits and permit wait, and the
/// backend.
fn server(endpoint: &str) -> (Arc<Server>, ServerConfig, Arc<S3Backend>) {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
    let store = S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        addressing: Addressing::Path,
        ca_pem: None,
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: REQUEST_TIMEOUT,
        min_body_rate: None,
    };
    let backend = Arc::new(S3Backend::new(store).unwrap());
    let server = Server::new(backend.clone(), config.clone());
    (server, config, backend)
}

/// Puts `body` under `key` through `server`.
async fn put(server: &Server, config: &ServerConfig, key: &str, body: &[u8]) -> Reply {
    // An S3 tier takes a body only with its length (411 without).
    let length = body.len().to_string();
    let told = [("content-length", length.as_str())];
    let request = sign(config, "PUT", &format!("/{BUCKET}/{key}"), &told, body);
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let bytes = http_body_util::BodyExt::collect(body)
        .await
        .expect("a whole body")
        .to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Asserts that a put to an upstream that reads the whole PUT and never answers is answered
/// with a server error (not `SlowDown`) when its request is cut ([`PUT_GIVES_UP`]), not later,
/// and that its permit is free then. Catches: a put that holds its permit through the look at the key after the
/// failed PUT (a second, a HEAD of up to the request timeout again: about 121 s in all).
#[tokio::test]
async fn a_put_to_an_upstream_that_never_answers_frees_its_permit_when_its_request_is_cut() {
    let s3 = FakeS3::start().await;
    let (endpoint, held) = stalling_upstream(&s3).await;
    let (server, config, backend) = server(&endpoint);
    let putting = {
        let (server, config) = (Arc::clone(&server), config.clone());
        tokio::spawn(async move { put(&server, &config, "stall/one", b"value").await })
    };
    held.reach(1).await;
    tokio::time::pause();
    let paused = tokio::time::Instant::now();
    let failed = putting.await.unwrap();
    let waited = paused.elapsed();
    assert!(failed.status.is_server_error(), "{}", failed.status);
    assert_ne!(failed.code(), "SlowDown");
    assert!(waited <= PUT_GIVES_UP, "answered after {waited:?}");
    assert_eq!(server.put_permits_free(), config.max_concurrent_puts);
    backend.cleanups_idle().await;
    assert_eq!(
        backend.short_objects().failed,
        1,
        "the look ended at its deadline"
    );
}

/// Asserts that 64 puts at once to an upstream that hangs them (twice the 32 permits) leave the
/// permits free once their requests are cut ([`PUT_GIVES_UP`]): the 32 that got a permit are
/// answered with the upstream's error by then, the 32 that waited for one are answered `SlowDown` at the permit
/// wait, and a put of a healthy key right after is taken. Catches: one stall of the upstream
/// holding every permit through the looks after the failed puts, so that every writer, also of
/// keys the upstream would take, is answered `SlowDown`.
#[tokio::test]
async fn stalled_puts_leave_the_permits_for_a_healthy_key() {
    let s3 = FakeS3::start().await;
    let (endpoint, held) = stalling_upstream(&s3).await;
    let (server, config, _backend) = server(&endpoint);
    let permits = config.max_concurrent_puts;
    let puts: Vec<_> = (0..64)
        .map(|n| {
            let (server, config) = (Arc::clone(&server), config.clone());
            let key = format!("{STALLED}{n}");
            tokio::spawn(async move { put(&server, &config, &key, b"value").await })
        })
        .collect();
    held.reach(permits).await;
    tokio::time::pause();
    let paused = tokio::time::Instant::now();
    let mut codes = Vec::new();
    for answered in puts {
        let reply = answered.await.unwrap();
        codes.push((reply.status, reply.code()));
    }
    let waited = paused.elapsed();
    assert!(
        waited <= PUT_GIVES_UP,
        "the last put was answered after {waited:?}"
    );
    let busy = (StatusCode::SERVICE_UNAVAILABLE, "SlowDown".to_owned());
    let refused = codes.iter().filter(|c| **c == busy).count();
    assert_eq!(refused, 64 - permits, "{codes:?}");
    let failed = codes
        .iter()
        .filter(|(status, code)| status.is_server_error() && code != "SlowDown");
    assert_eq!(failed.count(), permits, "{codes:?}");
    assert_eq!(server.put_permits_free(), permits);
    tokio::time::resume();
    let healthy = put(&server, &config, "healthy", b"value").await;
    assert_eq!(healthy.status, StatusCode::OK, "{}", healthy.body);
    assert_eq!(s3.keys(), ["healthy"]);
}
