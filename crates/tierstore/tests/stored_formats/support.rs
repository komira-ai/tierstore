//! A hot and a cold in-memory tier that one test clock stamps, and the helpers the golden tests
//! share.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore::{
    Backend, Error, MemoryBackend, Proxy, ProxyConfig, Reconciler, ReconcilerConfig, TestClock,
};

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

    /// A proxy `id` with a hot tier of `budget` bytes that reads the usage at every put.
    pub async fn proxy(&self, id: &str, budget: u64) -> Result<Proxy, Error> {
        let mut config = ProxyConfig::new(id, budget, self.clock.clone());
        config.linger = Duration::from_millis(1);
        config.usage_refresh = Duration::ZERO;
        Proxy::open(self.hot.clone(), self.cold.clone(), config).await
    }

    /// A reconciler `holder` for a hot tier of `budget` bytes, with the default lease of 30
    /// seconds.
    pub fn reconciler(&self, holder: &str, budget: u64) -> Reconciler {
        let config = ReconcilerConfig::new(holder, budget, self.clock.clone());
        Reconciler::new(self.hot.clone(), self.cold.clone(), config)
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
