//! A cache over a proxy: the keys the proxy keeps for itself are out of reach through the cache
//! too. The cache has no list of them; the proxy refuses them and leaves them out of its
//! listings, and the cache passes its answers on.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use tierstore_testkit::dir::TestDir;

use super::testing::{FakeDisk, never_expires};
use super::{CacheConfig, LocalCache};
use crate::{Backend, Error, Listed, MemoryBackend, PartBody, Proxy, ProxyConfig, TestClock};

/// The keys of a listing.
fn names(listed: &[Listed]) -> Vec<&str> {
    listed.iter().map(|object| object.key.as_str()).collect()
}

/// A body of one chunk.
fn body(bytes: &'static [u8]) -> PartBody<'static> {
    stream::once(async move { Ok(Bytes::from_static(bytes)) }).boxed()
}

/// A cache that writes through over the proxy `p1`, whose tiers are in memory: `a/1` and `b/1`
/// are put through the proxy, `c/cold` is only in the cold tier, and the keys under the three
/// reserved prefixes are in the tiers as well (the hot tier has the proxy's own `_journal/` and
/// `_tiering/` keys, and `_entries/h`; the cold tier has `_entries/c`, `_tiering/c`, `_journal/c`
/// and the upload of `_journal/up`).
struct Rig {
    _dir: TestDir,
    cache: LocalCache,
}

impl Rig {
    async fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let clock = Arc::new(TestClock::new());
        let hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
        let cold = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
        let mut proxy_config = ProxyConfig::new("p1", 1 << 20, clock.clone());
        proxy_config.linger = Duration::from_millis(1);
        proxy_config.usage_refresh = Duration::ZERO;
        let proxy = Proxy::open(hot.clone(), cold.clone(), proxy_config).await;
        let proxy = proxy.expect("a proxy");
        proxy.put("a/1", Bytes::from_static(b"1")).await.unwrap();
        proxy.put("b/1", Bytes::from_static(b"1")).await.unwrap();
        hot.put("_entries/h", Bytes::from_static(b"r"))
            .await
            .unwrap();
        for key in ["c/cold", "_entries/c", "_tiering/c", "_journal/c", "z/last"] {
            cold.put(key, Bytes::from_static(b"r")).await.unwrap();
        }
        cold.create_upload("_journal/up").await.unwrap();
        cold.create_upload("d/up").await.unwrap();
        let mut config = CacheConfig::new(
            dir.path().join("cache"),
            1 << 20,
            vec![("cas/".to_owned(), true)],
        );
        never_expires(&mut config);
        config.write_through = true;
        let upstream: Arc<dyn Backend> = Arc::new(proxy);
        let disk = Arc::new(FakeDisk::new());
        let cache = LocalCache::open(config, upstream, disk, clock).expect("a cache");
        Self { _dir: dir, cache }
    }
}

/// Asserts that a read and a write of every kind of a key under `_entries/`, `_journal/` or
/// `_tiering/`, whichever tier holds it, is refused with `Reserved` naming the key: the proxy
/// refuses, the cache has no check of its own.
#[tokio::test]
async fn every_call_on_a_reserved_key_is_refused_by_the_proxy_behind_the_cache() {
    let rig = Rig::new("tscache-reserved-calls").await;
    let cache = &rig.cache;
    let parts = [(1, "t".to_owned())];
    for key in [
        "_entries/h",
        "_entries/c",
        "_journal/p1/0001",
        "_journal/c",
        "_tiering/lease",
        "_tiering/c",
    ] {
        let refused = Some(Error::Reserved(key.to_owned()));
        assert_eq!(cache.get(key, None).await.err(), refused, "get {key}");
        assert_eq!(cache.head(key).await.err(), refused, "head {key}");
        let put = cache.put(key, Bytes::from_static(b"x")).await;
        assert_eq!(put.err(), refused, "put {key}");
        let streamed = cache.put_stream(key, Some(1), body(b"x")).await;
        assert_eq!(streamed.err(), refused, "put_stream {key}");
        let hot_only = cache.put_stream_hot_only(key, Some(1), body(b"x")).await;
        assert_eq!(hot_only.err(), refused, "put_stream_hot_only {key}");
        assert_eq!(cache.delete(key).await.err(), refused, "delete {key}");
        let created = cache.create_upload(key).await;
        assert_eq!(created.err(), refused, "create_upload {key}");
        let part = cache.upload_part(key, "id", 1, body(b"x")).await;
        assert_eq!(part.err(), refused, "upload_part {key}");
        let done = cache.complete_upload(key, "id", &parts).await;
        assert_eq!(done.err(), refused, "complete_upload {key}");
        let aborted = cache.abort_upload(key, "id").await;
        assert_eq!(aborted.err(), refused, "abort_upload {key}");
    }
}

/// Asserts that a listing, a page of a listing and a listing of uploads leave out the keys under
/// the three reserved prefixes whichever tier holds them, also when the cold tier is the only
/// one that holds a reserved key, and that a prefix inside a reserved one lists nothing.
#[tokio::test]
async fn listings_through_the_cache_leave_out_the_reserved_keys_of_both_tiers() {
    let rig = Rig::new("tscache-reserved-lists").await;
    let cache = &rig.cache;
    let all = cache.list("").await.unwrap();
    assert_eq!(names(&all), ["a/1", "b/1", "c/cold", "z/last"]);
    for prefix in ["_entries/", "_journal/", "_tiering/", "_"] {
        let under = cache.list(prefix).await.unwrap();
        assert!(under.is_empty(), "{prefix}");
        let page = cache.list_page(prefix, None, 10).await.unwrap();
        assert!(page.is_empty(), "{prefix}");
    }
    let first = cache.list_page("", None, 2).await.unwrap();
    assert_eq!(names(&first), ["a/1", "b/1"]);
    let second = cache.list_page("", Some("b/1"), 2).await.unwrap();
    assert_eq!(names(&second), ["c/cold", "z/last"]);
    let uploads = cache.list_uploads("", None, 10).await.unwrap();
    let keys: Vec<&str> = uploads.iter().map(|up| up.key.as_str()).collect();
    assert_eq!(keys, ["d/up"]);
}
