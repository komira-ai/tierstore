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

use futures::{StreamExt as _, stream};
use tierstore::server::local_cache::{CacheConfig, KeepObserver, LocalCache, SystemDisk};
use tierstore::{
    Backend, ByteRange, Clock, Error, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig,
    TestClock,
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
        let proxy = crate::support::open_live(self.hot.clone(), self.cold.clone(), config);
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
pub fn cache_over(
    upstream: Arc<dyn Backend>,
    change: impl FnOnce(&mut CacheConfig),
) -> (TestDir, LocalCache) {
    cache_observed(upstream, change, None)
}

/// [`cache_over`] with an observer of the keeps.
fn cache_observed(
    upstream: Arc<dyn Backend>,
    change: impl FnOnce(&mut CacheConfig),
    observer: Option<KeepObserver>,
) -> (TestDir, LocalCache) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-cache-over-proxy");
    let policies = vec![("img/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
    config.write_through = true;
    change(&mut config);
    let clock: Arc<dyn Clock> = Arc::new(tierstore::SystemClock);
    let disk = Arc::new(SystemDisk);
    let cache = LocalCache::open_observed(config, upstream, disk, clock, observer).unwrap();
    (dir, cache)
}

/// Asserts that an object only the cold tier holds, whose tag there is not the tag the hot tier
/// gives it, is read through a cache over the two tiers: the bytes come back, the proxy copied
/// nothing up (the fill's reads are ranged), and the next read is a hit that makes no call to
/// either tier.
#[tokio::test]
async fn a_cold_only_object_with_another_tag_function_is_filled_and_served() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers
        .cold_mem
        .put("img/a", body("cold bytes"))
        .await
        .unwrap();
    let (_dir, cache) = cache_over(proxy, |_| {});

    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(cache.held(), (10, 1));
    assert_absent(&*tiers.hot_mem, "img/a").await;
    let before = tiers.reads();
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(tiers.reads(), before);
}

/// Asserts that a cold tier that refuses every read of a whole object still fills a cache over
/// the proxy, whole-limit and windowed reads alike: the fill reads the object in chunks of its
/// range each, the cold tier is asked for those ranges only, and the proxy copies nothing into
/// the hot tier. It catches a proxy that reads the whole object from the cold tier to serve a
/// range of it.
#[tokio::test]
async fn a_ranged_fill_asks_the_cold_tier_for_ranges_only_and_copies_nothing_up() {
    let data = "0123456789abcdef0123456789abcdef";
    let asked = Some(ByteRange::new(0, 24));
    for whole_limit in [1 << 20, 4] {
        let clock = Arc::new(TestClock::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold = Arc::new(FaultyBackend::new(Arc::new(
            Retagged::over(cold_mem.clone()).ranged_only(),
        )));
        let config = ProxyConfig::new("p1", 1 << 20, clock.clone());
        let proxy = Proxy::open(hot_mem.clone(), cold.clone(), config).await;
        cold_mem.put("img/big", body(data)).await.unwrap();
        let (_dir, cache) = cache_over(Arc::new(proxy.unwrap()), |config| {
            config.whole_limit = whole_limit;
            config.fill_chunk = 8;
        });
        let got = cache.get("img/big", asked).await.unwrap();
        assert_eq!(got, body(&data[..24]), "whole_limit {whole_limit}");
        assert!(
            cold.get_ranges().iter().all(Option::is_some),
            "{:?}",
            cold.get_ranges()
        );
        assert_absent(&*hot_mem, "img/big").await;
    }
}

/// Asserts that a backend made ranged-only refuses a read of a whole object with its tag as it
/// refuses one without it, and answers the reads of a range, with the tag prefixed as a head
/// prefixes it: the guard of the fill test above holds for the read the proxy makes of the cold
/// tier.
#[tokio::test]
async fn a_ranged_only_backend_refuses_a_whole_tagged_read_and_answers_a_ranged_one() {
    let inner = Arc::new(MemoryBackend::new());
    inner.put("k", body("0123456789")).await.unwrap();
    let backend = Retagged::over(inner).ranged_only();

    let refused = backend.get_tagged("k", None).await;
    assert!(matches!(refused, Err(Error::Other(_))), "{refused:?}");
    let (bytes, tag) = backend
        .get_tagged("k", Some(ByteRange::new(2, 3)))
        .await
        .unwrap();
    assert_eq!(bytes, body("234"));
    assert_eq!(tag, backend.head("k").await.unwrap().tag);
}

/// Asserts that a read through a cache of an object only the cold tier holds is kept, and the
/// next read is a hit that makes no call to either tier, whatever the proxy copies up: the proxy
/// copies nothing up for the ranged reads of a fill, and tells versions apart by size alone.
#[tokio::test]
async fn a_cold_only_object_is_kept_and_the_next_read_makes_no_call_to_either_tier() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers
        .cold_mem
        .put("img/a", body("cold bytes"))
        .await
        .unwrap();
    let (_dir, cache) = cache_over(proxy, |_| {});

    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(cache.held(), (10, 1));
    let before = tiers.reads();
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("cold bytes"));
    assert_eq!(tiers.reads(), before);
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
    let (_dir, cache) = cache_observed(proxy, |_| {}, Some(observer));

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
    let (_dir, cache) = cache_over(proxy, |_| {});

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
    let (_dir, cache) = cache_over(proxy, |_| {});
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

/// Asserts that a plain put through a cache over a proxy whose format is not `active-marks` is the
/// proxy's create-only put, as a plain put of the proxy is: a new key is put and kept, a key the
/// hot tier holds is refused as `AlreadyExists` with no byte of the body read after the proxy's
/// answer and the bytes of the object as they were. (Only a proxy on markers replaces.)
#[tokio::test]
async fn a_plain_put_through_a_cache_over_an_entry_proxy_creates_and_refuses_an_existing_key() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers.hot_mem.put("img/h", body("old")).await.unwrap();
    let (_dir, cache) = cache_over(proxy, |_| {});
    let claim = tierstore::PutClaim::default();

    let made = cache.put_stream_replacing("img/n", Some(3), chunks(&["new"]), &claim);
    assert!(made.await.is_ok());
    assert_eq!(read(&*tiers.hot_mem, "img/n").await, body("new"));
    assert_eq!(cache.held(), (3, 1));

    let refused = cache.put_stream_replacing("img/h", Some(3), chunks(&["new"]), &claim);
    let refused = refused.await;
    assert_eq!(refused, Err(Error::AlreadyExists("img/h".to_owned())));
    assert_eq!(read(&*tiers.hot_mem, "img/h").await, body("old"));
    assert_eq!(cache.get("img/h", None).await.unwrap(), body("old"));
}

/// A cache over `upstream` that checks every hit (`revalidate_after = 0s`) and keeps what is under
/// `img/`, in the `cache` directory of `dir`: a second one over the same `dir` finds what the
/// first kept.
fn checking_cache_in(dir: &TestDir, upstream: Arc<dyn Backend>) -> LocalCache {
    let policies = vec![("img/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
    config.revalidate_after = Some(std::time::Duration::ZERO);
    let clock: Arc<dyn Clock> = Arc::new(tierstore::SystemClock);
    LocalCache::open(config, upstream, Arc::new(SystemDisk), clock).unwrap()
}

/// How many checks of hits ended with `result` (`same`, `changed`), by the metrics text.
fn checks(cache: &LocalCache, result: &str) -> u64 {
    let prefix = format!("tierstore_cache_revalidations_total{{result=\"{result}\"}} ");
    let metrics = cache.metrics();
    let line = metrics.lines().find_map(|line| line.strip_prefix(&prefix));
    line.expect("the series").parse().expect("a count")
}

/// Writes other bytes over an object a tier holds, as another process does (the tier refuses a
/// second create-only put, so the old object is deleted first).
async fn overwrite(tier: &MemoryBackend, key: &str, text: &str) {
    tier.delete(key).await.unwrap();
    tier.put(key, body(text)).await.unwrap();
}

/// Asserts that the check of an old hit over a proxy sees an object that another process wrote
/// over with bytes of the same length: the old bytes are not served, the new ones are read,
/// kept and served by the next hit, and an object written over again in the cold tier alone (the
/// hot tier has none) is seen the same way. A check that compared the sizes alone would serve the
/// old bytes for ever.
#[tokio::test]
async fn a_same_size_overwrite_under_a_proxy_is_seen_by_the_check_of_a_hit() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers.hot_mem.put("img/a", body("first")).await.unwrap();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-cache-revalidate");
    let cache = checking_cache_in(&dir, proxy);
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("first"));

    overwrite(&tiers.hot_mem, "img/a", "other").await;
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("other"));
    assert_eq!(cache.held(), (5, 1));
    assert_eq!(checks(&cache, "changed"), 1);
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("other"));
    assert_eq!(checks(&cache, "same"), 1);

    // Only the cold tier holds the object now, and it is written over there.
    tiers.hot_mem.delete("img/a").await.unwrap();
    tiers.cold_mem.put("img/a", body("third")).await.unwrap();
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("third"));
    overwrite(&tiers.cold_mem, "img/a", "fifth").await;
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("fifth"));
    assert_eq!(cache.held(), (5, 1));
}

/// Asserts what a check that finds the same object costs over a proxy: one HEAD of the hot tier,
/// no read of an object and no call to the cold tier, and the bytes come from the disk.
#[tokio::test]
async fn a_check_that_finds_the_same_object_is_one_hot_head_and_no_read_of_either_tier() {
    let tiers = Tiers::new();
    let proxy = tiers.proxy().await;
    tiers.hot_mem.put("img/a", body("first")).await.unwrap();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-cache-revalidate");
    let cache = checking_cache_in(&dir, proxy);
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("first"));
    let calls = |tiers: &Tiers| {
        let count = |tier: &FaultyBackend| [Op::Get, Op::Head].map(|op| tier.calls(op));
        (count(&tiers.hot), count(&tiers.cold))
    };
    let ([gets, heads], cold) = calls(&tiers);

    assert_eq!(cache.get("img/a", None).await.unwrap(), body("first"));
    assert_eq!(calls(&tiers), ([gets, heads + 1], cold));
    assert_eq!(checks(&cache, "same"), 1);
}

/// Asserts that an entry a former process kept, found on the disk at start, is compared by its
/// tag under a proxy too: the object written over with bytes of the same length while no cache
/// was running is read again by the first hit, not served from the disk.
#[tokio::test]
async fn an_entry_kept_before_a_restart_is_not_served_after_a_same_size_overwrite() {
    let tiers = Tiers::new();
    tiers.hot_mem.put("img/a", body("first")).await.unwrap();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-cache-revalidate");
    let before = checking_cache_in(&dir, tiers.proxy().await);
    assert_eq!(before.get("img/a", None).await.unwrap(), body("first"));
    drop(before);
    overwrite(&tiers.hot_mem, "img/a", "other").await;

    let after = checking_cache_in(&dir, tiers.proxy().await);
    assert_eq!(after.held(), (5, 1));
    assert_eq!(after.get("img/a", None).await.unwrap(), body("other"));
    assert_eq!(checks(&after, "changed"), 1);
    assert_eq!(after.get("img/a", None).await.unwrap(), body("other"));
    assert_eq!(checks(&after, "same"), 1);
}
