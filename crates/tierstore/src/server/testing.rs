//! Fixtures of the unit tests of the server: a server over in-memory tiers, and requests
//! signed for it.

use std::collections::VecDeque;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;

use hyper::body::{Body, Frame};
use hyper::header::HeaderMap;
use hyper::{Request, StatusCode};
use md5::Md5;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest as sha1_of};
use roxmltree::Document;
use sha2::Sha256;
use tokio::time::{Sleep, sleep};

use crate::{
    Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Proxy, ProxyConfig,
    Sleeper, TestClock,
};

use super::auth::{sign, sign_as, sign_on};
use super::request::BoxError;
use super::{Server, ServerConfig};
use http_body_util::BodyExt;
use sha2::Digest;
use std::future::Future;

pub(super) const KEY_ID: &str = "AKIDEXAMPLE";

pub(super) const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

pub(super) const BUCKET: &str = "tier-bucket";

/// The time of the clock of the rig, which is 2027-01-15 08:00:00 UTC.
pub(super) const NOW: u64 = 1_800_000_000_000;

/// The headers in which a request may carry a checksum of its body.
pub(super) const CHECKSUMS: [&str; 6] = [
    "content-md5",
    "x-amz-checksum-crc32",
    "x-amz-checksum-crc32c",
    "x-amz-checksum-sha1",
    "x-amz-checksum-sha256",
    "x-amz-checksum-crc64nvme",
];

/// CRC-64/NVME of `body`, bit by bit from its definition (reflected polynomial
/// `0x9a6c9329ac4bc9b5`, all ones in, all ones out), so that the tests do not trust the
/// implementation they check.
pub(super) fn crc64nvme(body: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for byte in body {
        crc ^= u64::from(*byte);
        for _ in 0..8 {
            let low = crc & 1;
            crc >>= 1;
            if low == 1 {
                crc ^= 0x9a6c_9329_ac4b_c9b5;
            }
        }
    }
    !crc
}

/// The value of the checksum header `name` for `body`, worked out apart from the server.
pub(super) fn checksum(name: &str, body: &[u8]) -> String {
    let hash: Vec<u8> = match name {
        "content-md5" => Md5::digest(body).to_vec(),
        "x-amz-checksum-crc32" => crc32fast::hash(body).to_be_bytes().to_vec(),
        "x-amz-checksum-crc32c" => crc32c::crc32c(body).to_be_bytes().to_vec(),
        "x-amz-checksum-sha1" => sha1_of(&SHA1_FOR_LEGACY_USE_ONLY, body).as_ref().to_vec(),
        "x-amz-checksum-crc64nvme" => crc64nvme(body).to_be_bytes().to_vec(),
        _ => Sha256::digest(body).to_vec(),
    };
    STANDARD.encode(hash)
}

/// A key of segments (text between `/`) of `lens` characters of `ch` each, joined by `/`. The
/// tests of the limits of a key use it to make a key of exact length with no long segment.
pub(super) fn segmented(ch: char, lens: &[usize]) -> String {
    let segments: Vec<String> = lens.iter().map(|len| ch.to_string().repeat(*len)).collect();
    segments.join("/")
}

/// The text of every element called `name` in `xml`, in document order.
pub(super) fn texts(xml: &str, name: &str) -> Vec<String> {
    let doc = Document::parse(xml).expect("well-formed XML");
    doc.descendants()
        .filter(|node| node.tag_name().name() == name)
        .map(|node| node.text().unwrap_or_default().to_owned())
        .collect()
}

/// What an answer was: its status, headers and body text.
pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: String,
}

impl Reply {
    /// The value of a header, or the empty text.
    pub(super) fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    /// The code of the error document in the body.
    pub(super) fn code(&self) -> String {
        texts(&self.body, "Code").concat()
    }
}

/// The format object of a store whose writes are made on entries.
pub(super) const ACTIVE_FORMAT: &str = r#"{"epoch":2,"phase":"active"}"#;

/// A wait that never ends: the heartbeat of a proxy then makes no call after its first beat, so the
/// only calls are the ones a test is about.
pub(super) struct Never;

#[async_trait]
impl Sleeper for Never {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// Starts a proxy over `hot` and `cold` that is live (its first beat is acknowledged and it makes
/// no other) over a hot tier whose format says `active` at epoch 2, unless it has a format object
/// already: what a put or a delete on an entry needs.
pub(super) async fn live_proxy(
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ProxyConfig,
) -> Result<Proxy, Error> {
    let _ = hot
        .put(
            "_tiering/format",
            Bytes::from_static(ACTIVE_FORMAT.as_bytes()),
        )
        .await;
    let proxy = Proxy::open(hot, cold, config).await?;
    assert!(proxy.start_heartbeat(Arc::new(Never)));
    while !proxy.is_live() {
        tokio::task::yield_now().await;
    }
    Ok(proxy)
}

/// A server over a proxy whose tiers are in memory, each behind a fault injector, with a
/// clock set by hand.
pub(super) struct Rig {
    pub(super) hot_mem: Arc<MemoryBackend>,
    pub(super) cold_mem: Arc<MemoryBackend>,
    pub(super) hot: Arc<FaultyBackend>,
    pub(super) cold: Arc<FaultyBackend>,
    /// The monotonic clock of the proxy, which judges how long the usage object has stood
    /// unchanged; moved by hand.
    pub(super) mono: Arc<crate::Mono>,
    pub(super) config: ServerConfig,
    pub(super) server: Arc<Server>,
}

impl Rig {
    /// A rig whose server serves the proxy `p1` with a hot tier of `budget` bytes.
    pub(super) async fn new(budget: u64) -> Self {
        Self::with(budget, |_| {}).await
    }

    /// The same, with the settings of the server changed by `change`.
    pub(super) async fn with(budget: u64, change: impl FnOnce(&mut ServerConfig)) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(NOW);
        let hot_mem = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
        let cold_mem = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
        let hot = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let mut proxy_config = ProxyConfig::new("p1", budget, clock.clone());
        proxy_config.usage_refresh = Duration::ZERO;
        // Live for as long as a test moves the monotonic clock.
        proxy_config.departure = Duration::from_secs(1_000_000);
        let mono = Arc::new(crate::Mono::new());
        proxy_config.mono = mono.clone();
        let proxy = live_proxy(hot.clone(), cold.clone(), proxy_config).await;
        let proxy = Arc::new(proxy.expect("a proxy"));
        let backend: Arc<dyn Backend> = proxy.clone();
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock.clone());
        config.rate_clock = clock;
        config.proxy = Some(proxy);
        change(&mut config);
        let server = Server::new(backend, config.clone());
        Self {
            hot_mem,
            cold_mem,
            hot,
            cold,
            mono,
            config,
            server,
        }
    }

    /// Runs the entry reconciler over the rig's tiers until a step changes nothing, as the
    /// reconciler of the node would between two requests: it copies what the puts left, finishes
    /// the deletes handed over and removes the entries, which releases what waits for them.
    pub(super) async fn settle(&self) {
        let clock = self.hot_mem.clock();
        let mut base = crate::ReconcilerConfig::new("rig/reconciler", 1 << 30, clock);
        base.mono = self.mono.clone();
        let config = crate::EntryReconcilerConfig::new(base);
        let mut reconciler =
            crate::EntryReconciler::new(self.hot.clone(), self.cold.clone(), config);
        while reconciler.step().await.progress() {}
        reconciler.stop().await;
    }

    /// Sends a request signed for the server and reads the answer.
    pub(super) async fn send(
        &self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let request = sign(&self.config, method, uri, extra, body);
        self.send_raw(request).await
    }

    /// Sends a request signed for the host `host` (the `Host` header, which the signature covers)
    /// and reads the answer.
    pub(super) async fn send_on(
        &self,
        host: &str,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let key = (
            self.config.access_key_id.as_str(),
            self.config.secret_access_key.as_str(),
        );
        let payload = hex::encode(Sha256::digest(body));
        let request = sign_on(
            host,
            &self.config,
            key,
            (method, uri),
            extra,
            &payload,
            body,
        );
        self.send_raw(request).await
    }

    /// Sends a request signed with the key `(id, secret)`, whichever key that is, and reads the
    /// answer.
    pub(super) async fn send_as(
        &self,
        key: (&str, &str),
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let request = sign_as(&self.config, key, method, uri, extra, body);
        self.send_raw(request).await
    }

    /// Sends a request signed for the server whose body is the one `played` plays, in place of
    /// the bytes `signed`, which are what the signature covers.
    pub(super) async fn send_played(
        &self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        signed: &[u8],
        played: Scripted,
    ) -> Reply {
        let (parts, _) = sign(&self.config, method, uri, extra, signed).into_parts();
        self.send_raw(Request::from_parts(parts, played)).await
    }

    /// Sends a request as it is and reads the answer.
    pub(super) async fn send_raw<B>(&self, request: Request<B>) -> Reply
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        let response = self.server.handle(request).await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.expect("a whole body").to_bytes();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// Sends a request for an object of the bucket (`key` may carry a query).
    pub(super) async fn call(&self, method: &str, key: &str, body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.send(method, &uri, &[], body).await
    }

    /// Puts an object through the server, create-only.
    pub(super) async fn put(&self, key: &str, body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.send("PUT", &uri, &[("if-none-match", "*")], body)
            .await
    }

    /// Gets an object through the server, with a `Range` header if there is one.
    pub(super) async fn get(&self, key: &str, range: Option<&str>) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        let extra: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        self.send("GET", &uri, &extra, b"").await
    }

    /// Lets the runtime's other tasks run until the hot tier holds the entry of `key`: the ask for
    /// the copy-back of a cold read is a task of its own, beside the response. It waits on that
    /// progress and on no clock.
    pub(super) async fn copy_back_asked(&self, key: &str) {
        while self.hot_mem.head(&crate::entry_key(key)).await.is_err() {
            tokio::task::yield_now().await;
        }
    }

    /// A listing of the bucket (`query` is what follows `list-type=2&`).
    pub(super) async fn list(&self, query: &str) -> Reply {
        let uri = format!("/{BUCKET}?list-type=2&{query}");
        self.send("GET", &uri, &[], b"").await
    }
}

/// One step of the body that a [`Scripted`] plays.
pub(super) enum Step {
    /// A piece of the body.
    Data(&'static [u8]),
    /// Nothing happens for this long (tokio time).
    Wait(Duration),
    /// A frame of trailers, which carries no byte of the body.
    Trailers,
    /// The body fails.
    Fail,
}

/// A request body that plays steps, so that a test says when its pieces arrive.
pub(super) struct Scripted {
    steps: VecDeque<Step>,
    waiting: Option<Pin<Box<Sleep>>>,
}

impl Scripted {
    pub(super) fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into(),
            waiting: None,
        }
    }
}

impl Body for Scripted {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        loop {
            if let Some(timer) = this.waiting.as_mut() {
                if timer.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.waiting = None;
            }
            let Some(step) = this.steps.pop_front() else {
                return Poll::Ready(None);
            };
            match step {
                Step::Data(bytes) => {
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(bytes)))));
                }
                Step::Wait(time) => this.waiting = Some(Box::pin(sleep(time))),
                Step::Trailers => {
                    return Poll::Ready(Some(Ok(Frame::trailers(HeaderMap::new()))));
                }
                Step::Fail => return Poll::Ready(Some(Err(std::io::Error::other("cut")))),
            }
        }
    }
}

/// A backend that does what the trait requires of it and no multipart upload: it holds nothing.
pub(super) struct Plain;

#[async_trait]
impl Backend for Plain {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Full(key.to_owned()))
    }

    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}

/// How many of `tries` connections to the listener at `at` are established while nothing accepts
/// them. The kernel keeps a listener's queue of established connections to its backlog (and lets
/// one more in) and drops the rest of the handshakes, which stay pending; so the count is at most
/// the backlog plus one, and a listener with a larger backlog shows more. The first connection is
/// waited for, so that the count is never zero for a listener that works; the others are given a
/// number of turns of the runtime, and a handshake that had not come by then is not counted.
pub(crate) async fn established_unaccepted(at: std::net::SocketAddr, tries: usize) -> usize {
    let attempts: Vec<_> = (0..tries)
        .map(|_| tokio::spawn(tokio::net::TcpStream::connect(at)))
        .collect();
    let (first, rest) = attempts.split_first().expect("at least one try");
    let mut established = 0;
    while !first.is_finished() {
        tokio::task::yield_now().await;
    }
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    for attempt in rest {
        if attempt.is_finished() {
            established += 1;
        }
    }
    for attempt in &attempts {
        attempt.abort();
    }
    established + 1
}
