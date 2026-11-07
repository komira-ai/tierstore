//! The local cache in front of a proxy whose format object says `active-marks`: what a put through
//! the cache asks of the hot tier, its marks bucket and the cold tier.
//!
//! Every store is in memory behind a fault injector that counts its requests, and the keep of the
//! cache is waited for by the cache's own notice that it put the object in the index, on a paused
//! clock (as in `cache_over_proxy`), so nothing here waits for time.

use std::sync::Arc;

use tierstore::server::local_cache::{CacheConfig, KeepObserver, LocalCache, SystemDisk};
use tierstore::{
    Backend, Clock, Error, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, SystemClock,
    TestClock,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::support::{body, chunks, open_live};

const FORMAT: &str = r#"{"epoch":3,"marks_bucket":"hot-marks","phase":"active-marks"}"#;

/// The hot tier, the marks bucket and the cold tier, each behind a fault injector.
struct Stores {
    hot: Arc<FaultyBackend>,
    marks: Arc<FaultyBackend>,
    cold: Arc<FaultyBackend>,
}

/// A request: which store, what, and for which key.
type Request = (&'static str, Op, String);

impl Stores {
    async fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let marks = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold = Arc::new(MemoryBackend::with_clock(clock));
        hot_mem.put("_tiering/format", body(FORMAT)).await.unwrap();
        Self {
            hot: Arc::new(FaultyBackend::new(hot_mem.clone())),
            marks: Arc::new(FaultyBackend::new(marks)),
            cold: Arc::new(FaultyBackend::new(cold)),
        }
    }

    async fn proxy(&self) -> Arc<Proxy> {
        let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(TestClock::new()));
        config.marks = Some(self.marks.clone());
        config.marks_name = Some("hot-marks".to_owned());
        let proxy = open_live(self.hot.clone(), self.cold.clone(), config);
        Arc::new(proxy.await.unwrap())
    }

    /// Every request made so far to a key that is not the proxy's own, store by store.
    fn requests(&self) -> Vec<Request> {
        let stores = [
            ("hot", &self.hot),
            ("marks", &self.marks),
            ("cold", &self.cold),
        ];
        let mut all = Vec::new();
        for (name, store) in stores {
            let log = store.call_log();
            let own = |key: &str| key.starts_with("_tiering/");
            all.extend(
                log.into_iter()
                    .filter(|(_, key)| !own(key))
                    .map(|(op, key)| (name, op, key)),
            );
        }
        all
    }
}

/// The requests of `after` that `before` (an earlier `requests()`) did not have, store by store.
fn since(before: &[Request], after: Vec<Request>) -> Vec<Request> {
    let mut skip: Vec<(&str, usize)> = ["hot", "marks", "cold"]
        .into_iter()
        .map(|store| (store, before.iter().filter(|r| r.0 == store).count()))
        .collect();
    let mut news = Vec::new();
    for item in after {
        let slot = skip.iter_mut().find(|(store, _)| *store == item.0);
        match slot {
            Some((_, left)) if *left > 0 => *left -= 1,
            _ => news.push(item),
        }
    }
    news
}

fn request(store: &'static str, op: Op, key: &str) -> Request {
    (store, op, key.to_owned())
}

/// Asserts what a put of a new key through a cache costs the stores behind a marks proxy, without
/// the cache's HEAD before it and with it (`head_before_put`, on by default). Without it the cache
/// sends the proxy's put and nothing else: the hot HEAD, the marker GET and the cold HEAD of its
/// wave 1, then the object and the marker, and no HEAD after, since the proxy's answer names the
/// tag of the hot object. With it the same, and before it the cache's HEAD through the proxy, which
/// for a key nothing holds is the same three reads. Either way the cache then serves the bytes it
/// kept with no request.
#[tokio::test(start_paused = true)]
async fn a_put_through_makes_no_head_before_and_none_after_when_the_proxy_answers_with_the_tag() {
    let put = [
        request("hot", Op::Head, "img/p"),
        request("hot", Op::Put, "img/p"),
        request("marks", Op::Get, "img/p"),
        request("marks", Op::Put, "img/p"),
        request("cold", Op::Head, "img/p"),
    ];
    let with_head = [
        request("hot", Op::Head, "img/p"),
        request("hot", Op::Head, "img/p"),
        request("hot", Op::Put, "img/p"),
        request("marks", Op::Get, "img/p"),
        request("marks", Op::Get, "img/p"),
        request("marks", Op::Put, "img/p"),
        request("cold", Op::Head, "img/p"),
        request("cold", Op::Head, "img/p"),
    ];
    for (head_before_put, wanted) in [(false, &put[..]), (true, &with_head[..])] {
        let stores = Stores::new().await;
        let proxy = stores.proxy().await;
        let kept = Arc::new(Notify::new());
        let told = Arc::clone(&kept);
        let observer: KeepObserver = Arc::new(move || told.notify_one());
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-cache-over-marks");
        let policies = vec![("img/".to_owned(), true)];
        let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
        config.write_through = true;
        config.head_before_put = head_before_put;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let upstream: Arc<dyn Backend> = proxy;
        let opened = LocalCache::open_observed(
            config,
            upstream,
            Arc::new(SystemDisk),
            clock,
            Some(observer),
        );
        let cache = opened.unwrap();

        cache
            .put_stream("img/p", Some(3), chunks(&["abc"]))
            .await
            .unwrap();
        kept.notified().await;
        assert_eq!(
            stores.requests(),
            wanted,
            "head_before_put {head_before_put}"
        );

        assert_eq!(cache.get("img/p", None).await.unwrap(), body("abc"));
        assert_eq!(
            stores.requests(),
            wanted,
            "head_before_put {head_before_put}"
        );
    }
}

/// Asserts what an overwrite through a cache costs the stores behind a marks proxy, and what the
/// cache serves after it: the plain put goes to the proxy as its plain put and the cache makes no
/// HEAD of its own, with `head_before_put` on or off, so the requests are the proxy's own for an
/// overwrite (the hot HEAD that finds the key, the marker read under the lock and its writes, the
/// hot PUT) and none after it; the next read is the new bytes served from the cache's disk with no
/// request, also in a cache opened again over the same directory; and a create-only put of the key
/// is still refused as `AlreadyExists`, the cached copy left as it was.
#[tokio::test(start_paused = true)]
async fn an_overwrite_through_a_cache_makes_the_proxys_plain_put_and_is_served_locally() {
    for head_before_put in [true, false] {
        let stores = Stores::new().await;
        let proxy = stores.proxy().await;
        let kept = Arc::new(Notify::new());
        let told = Arc::clone(&kept);
        let observer: KeepObserver = Arc::new(move || told.notify_one());
        let dir = TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            "tierstore-cache-over-marks-put",
        );
        let policies = vec![("img/".to_owned(), true)];
        let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
        config.write_through = true;
        config.head_before_put = head_before_put;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let upstream: Arc<dyn Backend> = proxy;
        let opened = LocalCache::open_observed(
            config.clone(),
            Arc::clone(&upstream),
            Arc::new(SystemDisk),
            Arc::clone(&clock),
            Some(observer),
        );
        let cache = opened.unwrap();
        cache
            .put_stream("img/p", Some(3), chunks(&["old"]))
            .await
            .unwrap();
        kept.notified().await;
        let before = stores.requests();

        let claim = tierstore::PutClaim::default();
        let made = cache.put_stream_replacing("img/p", Some(3), chunks(&["new"]), &claim);
        made.await.unwrap();
        kept.notified().await;
        let overwrite = [
            request("hot", Op::Head, "img/p"),
            request("hot", Op::Head, "img/p"),
            request("hot", Op::PutIfMatch, "img/p"),
            request("marks", Op::Get, "img/p"),
            request("marks", Op::PutIfMatch, "img/p"),
            request("marks", Op::PutIfMatch, "img/p"),
        ];
        let after = stores.requests();
        assert_eq!(since(&before, after), overwrite, "{head_before_put}");

        let settled = stores.requests();
        assert_eq!(cache.get("img/p", None).await.unwrap(), body("new"));
        assert_eq!(stores.requests(), settled, "{head_before_put}");
        let again = LocalCache::open(config, upstream, Arc::new(SystemDisk), clock);
        let again = again.unwrap();
        assert_eq!(again.get("img/p", None).await.unwrap(), body("new"));
        assert_eq!(stores.requests(), settled, "{head_before_put}");

        let refused = cache.put_stream("img/p", Some(3), chunks(&["xyz"])).await;
        assert_eq!(refused, Err(Error::AlreadyExists("img/p".to_owned())));
        let settled = stores.requests();
        assert_eq!(cache.get("img/p", None).await.unwrap(), body("new"));
        assert_eq!(stores.requests(), settled, "{head_before_put}");
    }
}
