//! A server on a loopback port over in-memory tiers, and the clients that reach it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use objstore_s3::Capabilities;
use objstore_s3::s3::{Credentials, PART_BYTES, S3Config, S3Store, S3Tuning};
use tierstore::server::{Drained, Server, ServerConfig};
use tierstore::{
    Backend, Clock, EntryReconciler, EntryReconcilerConfig, Error, FaultyBackend, MemoryBackend,
    Proxy, ProxyConfig, ReconcilerConfig, Sleeper, SystemClock, TestClock,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::signer::{FIXED_STAMP, Request, sign};

/// A wait that never ends: the heartbeat of a proxy then makes no call after its first beat.
struct Never;

#[async_trait]
impl Sleeper for Never {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// The format object of a store that runs on entries: what a put or a delete on an entry needs.
const ENTRIES_FORMAT: &str = r#"{"epoch":2,"phase":"active"}"#;

/// The name of the marks bucket of a harness that runs on markers.
pub const MARKS_BUCKET: &str = "hot-marks";

/// The format object of a store that runs on markers.
const MARKS_FORMAT: &str = r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;

/// Starts a proxy over `hot` and `cold` and makes it live, with the format object `format` in
/// `hot`.
async fn live_proxy(
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ProxyConfig,
    format: &'static str,
) -> Result<Proxy, Error> {
    let _ = hot
        .put("_tiering/format", Bytes::from_static(format.as_bytes()))
        .await;
    let proxy = Proxy::open(hot, cold, config).await?;
    assert!(proxy.start_heartbeat(Arc::new(Never)));
    while !proxy.is_live() {
        tokio::task::yield_now().await;
    }
    Ok(proxy)
}

/// A change to the settings of a proxy.
type Tune = Box<dyn FnOnce(&mut ProxyConfig)>;

/// A clock that stands at `FIXED_MILLIS`.
fn fixed() -> Arc<dyn Clock> {
    let clock = Arc::new(TestClock::new());
    clock.set(FIXED_MILLIS);
    clock
}

pub const KEY_ID: &str = "AKIDTIERSTORE";

pub const SECRET: &str = "tierstore-test-secret";

pub const BUCKET: &str = "tier-bucket";

/// The time, in milliseconds, of a server whose clock is set by hand: `signer::FIXED_STAMP`.
pub const FIXED_MILLIS: u64 = 1_800_000_000_000;

/// A running server and its tiers.
pub struct Harness {
    pub addr: SocketAddr,
    pub server: Arc<Server>,
    pub hot_mem: Arc<MemoryBackend>,
    pub cold_mem: Arc<MemoryBackend>,
    pub hot: Arc<FaultyBackend>,
    pub cold: Arc<FaultyBackend>,
    /// The marks bucket, which a harness that runs on entries never touches.
    pub marks_mem: Arc<MemoryBackend>,
    pub marks: Arc<FaultyBackend>,
    /// The task that serves; it ends with the drain.
    pub served: JoinHandle<Drained>,
}

impl Harness {
    /// A server that tells the time by the clock of the machine, as an `S3Store` client does.
    pub async fn start(change: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::start_with(Arc::new(SystemClock), None, change).await
    }

    /// A server whose clock stands at `FIXED_MILLIS`, so that a request can be signed for it
    /// without reading the time.
    pub async fn start_fixed(change: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::start_with(fixed(), None, change).await
    }

    /// As [`Harness::start_fixed`], over a store whose format object says `active-marks`: the proxy
    /// puts, replaces and deletes on markers in `marks_mem`. `tune` changes the settings of the
    /// proxy.
    pub async fn start_marks(
        tune: impl FnOnce(&mut ProxyConfig) + 'static,
        change: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let tune: Tune = Box::new(tune);
        Self::start_with(fixed(), Some(tune), change).await
    }

    async fn start_with(
        clock: Arc<dyn Clock>,
        marks: Option<Tune>,
        change: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let hot_mem = Arc::new(MemoryBackend::new());
        let cold_mem = Arc::new(MemoryBackend::new());
        let hot = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let marks_mem = Arc::new(MemoryBackend::new());
        let marks_faulty = Arc::new(FaultyBackend::new(marks_mem.clone()));
        let mut proxy_config = ProxyConfig::new("p1", 1 << 30, Arc::new(SystemClock));
        proxy_config.usage_refresh = Duration::ZERO;
        let format = match marks {
            Some(tune) => {
                proxy_config.marks = Some(marks_faulty.clone());
                proxy_config.marks_name = Some(MARKS_BUCKET.to_owned());
                tune(&mut proxy_config);
                MARKS_FORMAT
            }
            None => ENTRIES_FORMAT,
        };
        let opened = live_proxy(hot.clone(), cold.clone(), proxy_config, format).await;
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
        config.drain_lb_wait = Duration::from_millis(100);
        config.drain_deadline = Duration::from_secs(10);
        change(&mut config);
        let server = Server::new(Arc::new(opened.expect("a proxy")), config);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let addr = listener.local_addr().expect("an address");
        let serving = Arc::clone(&server);
        let served = tokio::spawn(async move { serving.serve(listener).await });
        Self {
            addr,
            server,
            hot_mem,
            cold_mem,
            hot,
            cold,
            marks_mem,
            marks: marks_faulty,
            served,
        }
    }

    /// Runs the entry reconciler over the tiers until a step changes nothing, as the reconciler of
    /// the node would between two requests: it removes the entries that a delete handed over, which
    /// releases what waits for them.
    pub async fn settle(&self) {
        let base = ReconcilerConfig::new("harness/reconciler", 1 << 30, Arc::new(SystemClock));
        let config = EntryReconcilerConfig::new(base);
        let mut reconciler = EntryReconciler::new(self.hot.clone(), self.cold.clone(), config);
        while reconciler.step().await.progress() {}
        reconciler.stop().await;
    }

    /// The `S3Store` client, with the key of the server.
    pub fn store(&self, capabilities: Capabilities) -> S3Store {
        self.store_with(KEY_ID, SECRET, capabilities)
    }

    /// The `S3Store` client with the settings it has by default: one streamed PUT up to 5 GiB.
    pub fn store_default(&self, capabilities: Capabilities) -> S3Store {
        let config = S3Config {
            endpoint: format!("http://{}", self.addr),
            region: "us-east-1".to_owned(),
            bucket: BUCKET.to_owned(),
            credentials: Credentials::new(KEY_ID, SECRET),
            capabilities,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            min_body_rate: None,
        };
        S3Store::new(config).expect("a store")
    }

    /// The `S3Store` client, with this key.
    pub fn store_with(&self, key_id: &str, secret: &str, capabilities: Capabilities) -> S3Store {
        let config = S3Config {
            endpoint: format!("http://{}", self.addr),
            region: "us-east-1".to_owned(),
            bucket: BUCKET.to_owned(),
            credentials: Credentials::new(key_id, secret),
            capabilities,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            min_body_rate: None,
        };
        // The threshold of the client before it was a setting, so that a 16 MiB body and a little
        // more is the multipart upload these tests are of, not 5 GiB.
        let tuning = S3Tuning {
            single_put_max: PART_BYTES,
            ..S3Tuning::default()
        };
        S3Store::new_tuned(config, tuning).expect("a store")
    }

    /// The headers that sign a request for the server at the fixed time.
    pub fn signed(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        self.signed_at(FIXED_STAMP, method, path, query, headers, body)
    }

    /// The headers that sign a request for the server at the fixed time with the key
    /// `(id, secret)`, whichever key that is.
    pub fn signed_as(
        &self,
        key: (&str, &str),
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        let host = self.addr.to_string();
        let request = Request {
            method,
            host: &host,
            path,
            query,
            headers,
            body,
            stamp: FIXED_STAMP,
        };
        sign(&request, key.0, key.1)
    }

    /// The headers that sign a request made at the time `stamp` names.
    pub fn signed_at(
        &self,
        stamp: &str,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<(String, String)> {
        let host = self.addr.to_string();
        let request = Request {
            method,
            host: &host,
            path,
            query,
            headers,
            body,
            stamp,
        };
        sign(&request, KEY_ID, SECRET)
    }
}
