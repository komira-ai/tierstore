//! The local cache in front of a proxy over two tiers whose tags are different functions of the
//! content (the hot tier's is a SHA-256, the cold tier's an S3-style ETag): what the cache reads
//! and writes through the proxy.
//!
//! The test that reads what it wrote waits for the cache's own notice that a keep has put its
//! object in the index, and runs on a paused clock: the keep's wait and deadline are timers that
//! only the test could advance, and it does not, so the file work on blocking threads can take
//! as long as it takes. The others run on the real clock and read nothing of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt as _, stream};
use tierstore::server::local_cache::{CacheConfig, KeepObserver, LocalCache, SystemDisk};
use tierstore::{
    Backend, ByteRange, Clock, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op, Proxy,
    ProxyConfig, ReadBody, TestClock,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::support::{Retagged, Rig, assert_absent, body, chunks, read, set_usage};

/// Tiers that tag differently, behind fault injectors that count the calls.
struct Tiers {
    clock: Arc<TestClock>,
    hot_mem: Arc<MemoryBackend>,
    cold_mem: Arc<MemoryBackend>,
    hot: Arc<FaultyBackend>,
    cold: Arc<FaultyBackend>,
}

impl Tiers {
    fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let retagged = Arc::new(Retagged::over(cold_mem.clone()));
        Self {
            clock,
            hot: Arc::new(FaultyBackend::new(hot_mem.clone())),
            cold: Arc::new(FaultyBackend::new(retagged)),
            hot_mem,
            cold_mem,
        }
    }

    async fn proxy(&self) -> Arc<Proxy> {
        let config = ProxyConfig::new("p1", 1 << 20, self.clock.clone());
        let proxy = Proxy::open(self.hot.clone(), self.cold.clone(), config);
        Arc::new(proxy.await.unwrap())
    }

    /// The calls that read an object, of both tiers.
    fn reads(&self) -> u64 {
        [Op::Get, Op::Head]
            .iter()
            .map(|op| self.hot.calls(*op) + self.cold.calls(*op))
            .sum()
    }
}

/// A cache over `upstream` that keeps what is under `img/` and takes writes, in a scratch
/// directory kept as long as the returned guard.
fn cache_over(
    upstream: Arc<dyn Backend>,
    tiered: bool,
    change: impl FnOnce(&mut CacheConfig),
) -> (TestDir, LocalCache) {
    cache_observed(upstream, tiered, change, None)
}

/// [`cache_over`] with an observer of the keeps.
fn cache_observed(
    upstream: Arc<dyn Backend>,
    tiered: bool,
    change: impl FnOnce(&mut CacheConfig),
    observer: Option<KeepObserver>,
) -> (TestDir, LocalCache) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-cache-over-proxy");
    let policies = vec![("img/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
    config.write_through = true;
    config.tiered_upstream = tiered;
    change(&mut config);
    let clock: Arc<dyn Clock> = Arc::new(tierstore::SystemClock);
    let disk = Arc::new(SystemDisk);
    let cache = LocalCache::open_observed(config, upstream, disk, clock, observer).unwrap();
    (dir, cache)
}

/// Asserts that an object only the cold tier holds, whose tag there is not the tag the hot tier
/// gives it once the proxy has copied it up, is read through a cache over the two tiers: the
/// bytes come back, the proxy promoted the object, and the next read is a hit that makes no call
/// to either tier.
#[tokio::test]
async fn a_cold_only_object_with_another_tag_function_is_filled_and_served() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers
        .cold_mem
        .put("img/a", body("cold bytes"))
        .await
        .unwrap();
    let (_dir, cache) = cache_over(proxy, true, |_| {});

    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(cache.held(), (10, 1));
    assert_eq!(read(&*tiers.hot_mem, "img/a").await, body("cold bytes"));
    let before = tiers.reads();
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(tiers.reads(), before);
}

/// Asserts that the same read through a cache that compares tags is answered, but the object is
/// kept nowhere: the tag the first HEAD gave (the cold tier's) is not the one the proxy gives
/// after it has copied the object up, so the fill ends as changed, and the next read goes to the
/// tiers again. This is what `tiered_upstream` removes.
#[tokio::test]
async fn without_the_tiered_option_a_cold_only_object_is_not_kept() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers
        .cold_mem
        .put("img/a", body("cold bytes"))
        .await
        .unwrap();
    let (_dir, cache) = cache_over(proxy, false, |_| {});

    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(cache.held(), (0, 0));
    let before = tiers.reads();
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert!(tiers.reads() > before);
}

/// A proxy as the upstream of a cache, whose streamed reads after the first wait until the hot
/// tier holds the key: the copy of a cold read is a task beside the response, and the windows of
/// a read are made one after the other, so this makes the second window start after the copy of
/// the first, as the test needs. It waits on that progress (a yield loop), on no clock.
struct AfterTheCopy {
    proxy: Arc<Proxy>,
    hot: Arc<MemoryBackend>,
    streams: AtomicUsize,
}

#[async_trait]
impl Backend for AfterTheCopy {
    fn needs_length(&self) -> bool {
        self.proxy.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.proxy.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.proxy.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.proxy.get(key, range).await
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        if self.streams.fetch_add(1, Ordering::SeqCst) > 0 {
            while self.hot.head(key).await.is_err() {
                tokio::task::yield_now().await;
            }
        }
        self.proxy.get_stream(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.proxy.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.proxy.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.proxy.list(prefix).await
    }
}

/// Asserts the same for an object larger than the whole-fill limit, which is read in windows and
/// checked against the version the read began with: served with the option, refused without it.
#[tokio::test]
async fn a_windowed_read_of_a_cold_only_object_succeeds_only_with_the_tiered_option() {
    let data = "0123456789abcdef0123456789abcdef";
    let asked = Some(ByteRange::new(0, 24));
    for (tiered, served) in [(true, true), (false, false)] {
        let tiers = Tiers::new();
        let proxy = Arc::new(AfterTheCopy {
            proxy: tiers.proxy().await,
            hot: tiers.hot_mem.clone(),
            streams: AtomicUsize::new(0),
        });
        tiers.cold_mem.put("img/big", body(data)).await.unwrap();
        let (_dir, cache) = cache_over(proxy, tiered, |config| {
            config.whole_limit = 4;
            config.fill_chunk = 8;
        });
        let got = cache.get("img/big", asked).await;
        match got {
            Ok(bytes) => assert!(served && bytes == body(&data[..24])),
            Err(error) => assert!(
                !served && matches!(error, Error::Unavailable(_)),
                "{error:?}"
            ),
        }
    }
}

/// Asserts that a put through the cache goes to the proxy, is kept (the test waits for the cache's
/// notice that the keep put the object in the index, on a paused clock, so that neither the
/// keep's wait nor its deadline can pass first), and is read back without a read of either tier.
#[tokio::test(start_paused = true)]
async fn a_put_through_the_cache_is_kept_and_read_without_a_tier_read() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    let kept = Arc::new(Notify::new());
    let told = Arc::clone(&kept);
    let observer: KeepObserver = Arc::new(move || told.notify_one());
    let (_dir, cache) = cache_observed(proxy, true, |_| {}, Some(observer));

    cache
        .put_stream("img/p", Some(3), chunks(&["abc"]))
        .await
        .unwrap();
    kept.notified().await;
    assert_eq!(cache.held(), (3, 1));
    assert_eq!(read(&*tiers.hot_mem, "img/p").await, body("abc"));
    let again = cache.put_stream("img/p", Some(3), chunks(&["xyz"])).await;
    assert_eq!(again, Err(Error::AlreadyExists("img/p".to_owned())));

    let before = tiers.reads();
    assert_eq!(cache.get("img/p", None).await.unwrap(), body("abc"));
    assert_eq!(tiers.reads(), before);
}

/// Asserts that a hot-only put through the cache is answered `Full`, with nothing stored in
/// either tier, when the hot tier is full; and that a plain put at the same level goes to the
/// cold tier. The first half fails when the cache turns a hot-only put into a plain one.
#[tokio::test]
async fn a_hot_only_put_through_the_cache_answers_full_and_stores_nothing_in_cold() {
    let rig = Rig::new();
    let proxy = Arc::new(rig.proxy("p1", 1000).await);
    set_usage(&rig, 1000, 0).await;
    let (_dir, cache) = cache_over(proxy, true, |_| {});

    for key in ["other/h", "img/h"] {
        let full = cache
            .put_stream_hot_only(key, Some(3), chunks(&["abc"]))
            .await;
        assert!(matches!(full, Err(Error::Full(_))), "{key}: {full:?}");
        assert_absent(&*rig.hot_mem, key).await;
        assert_absent(&*rig.cold_mem, key).await;
    }

    cache
        .put_stream("other/c", Some(3), chunks(&["abc"]))
        .await
        .unwrap();
    assert_eq!(read(&*rig.cold_mem, "other/c").await, body("abc"));
    assert_absent(&*rig.hot_mem, "other/c").await;
}

/// Asserts that a put of a key only the hot tier holds is refused by the cache before any byte of
/// the body is read, and that neither tier is written: the proxy's own put would write the whole
/// body to the hot tier before it found the key there. The body counts the chunks asked of it.
#[tokio::test]
async fn a_put_of_a_key_the_hot_tier_holds_is_refused_before_the_body_is_read() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers.hot_mem.put("img/h", body("old")).await.unwrap();
    let (_dir, cache) = cache_over(proxy, true, |_| {});
    let puts = (tiers.hot.calls(Op::Put), tiers.cold.calls(Op::Put));

    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&asked);
    let large = stream::iter(0..1000)
        .map(move |_| {
            let _ = counted.fetch_add(1, Ordering::SeqCst);
            Ok(body("0123456789"))
        })
        .boxed();
    let refused = cache.put_stream("img/h", Some(10_000), large).await;
    assert_eq!(refused, Err(Error::AlreadyExists("img/h".to_owned())));
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(read(&*tiers.hot_mem, "img/h").await, body("old"));
    assert_eq!(puts, (tiers.hot.calls(Op::Put), tiers.cold.calls(Op::Put)));
}
