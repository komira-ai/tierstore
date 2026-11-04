//! A hot and a cold in-memory tier that one test clock stamps, and the helpers the golden tests
//! share.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{
    Backend, EntryReconciler, EntryReconcilerConfig, Error, MemoryBackend, Proxy, ProxyConfig,
    ReconcilerConfig, Sleeper, TestClock,
};

/// The format object of a store whose writes are made on entries.
const ACTIVE: &str = r#"{"epoch":2,"phase":"active"}"#;

/// A wait that never ends: the heartbeat of a proxy then makes no call after its first beat.
struct Never;

#[async_trait]
impl Sleeper for Never {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// The two tiers and the clock.
pub struct Tiers {
    pub clock: Arc<TestClock>,
    pub hot: Arc<MemoryBackend>,
    pub cold: Arc<MemoryBackend>,
}

impl Tiers {
    pub fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold = Arc::new(MemoryBackend::with_clock(clock.clone()));
        Self { clock, hot, cold }
    }

    /// A proxy `id` with a hot tier of `budget` bytes that reads the usage at every put. It has
    /// not beaten: it registers and nothing else.
    pub async fn proxy(&self, id: &str, budget: u64) -> Result<Proxy, Error> {
        let mut config = ProxyConfig::new(id, budget, self.clock.clone());
        config.usage_refresh = Duration::ZERO;
        Proxy::open(self.hot.clone(), self.cold.clone(), config).await
    }

    /// As [`Tiers::proxy`], with the monotonic clock of the proxy the same clock as the stores',
    /// which a test moves by hand, made live on a format that says `active` at epoch 2 (unless
    /// the hot tier has a format object already): its first beat is acknowledged and it makes no
    /// other, so it can put.
    pub async fn proxy_on_clock(&self, id: &str, budget: u64) -> Result<Proxy, Error> {
        let mut config = ProxyConfig::new(id, budget, self.clock.clone());
        config.usage_refresh = Duration::ZERO;
        config.mono = self.clock.clone();
        let _ = self
            .hot
            .put("_tiering/format", ACTIVE.as_bytes().to_vec().into())
            .await;
        let proxy = Proxy::open(self.hot.clone(), self.cold.clone(), config).await?;
        assert!(proxy.start_heartbeat(Arc::new(Never)));
        while !proxy.is_live() {
            tokio::task::yield_now().await;
        }
        Ok(proxy)
    }

    /// A reconciler `holder` for a hot tier of `budget` bytes, with the default lease of 30
    /// seconds.
    pub fn reconciler(&self, holder: &str, budget: u64) -> EntryReconciler {
        let mut base = ReconcilerConfig::new(holder, budget, self.clock.clone());
        base.mono = self.clock.clone();
        let config = EntryReconcilerConfig::new(base);
        EntryReconciler::new(self.hot.clone(), self.cold.clone(), config)
    }

    /// Puts `bytes` in the hot tier under `key`, as a writer of the stored format would.
    pub async fn seed(&self, key: &str, bytes: &[u8]) {
        let body = Bytes::copy_from_slice(bytes);
        self.hot.put(key, body).await.unwrap();
    }

    /// The bytes of the object under `key` in the hot tier.
    pub async fn stored(&self, key: &str) -> Vec<u8> {
        self.hot.get(key, None).await.unwrap().to_vec()
    }

    /// The keys of the hot tier under `prefix`, in order.
    pub async fn hot_keys(&self, prefix: &str) -> Vec<String> {
        let listed = self.hot.list(prefix).await.unwrap();
        listed.into_iter().map(|object| object.key).collect()
    }

    /// The keys of the cold tier, in order.
    pub async fn cold_keys(&self) -> Vec<String> {
        let listed = self.cold.list("").await.unwrap();
        listed.into_iter().map(|object| object.key).collect()
    }
}
