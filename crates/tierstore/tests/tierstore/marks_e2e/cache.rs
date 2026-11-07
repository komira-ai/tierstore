//! The local cache of a build node in front of a proxy on markers: the exception to strong
//! consistency of 5.16, bounded by `local_cache.revalidate_after`.

use std::time::Duration;

use tierstore::server::local_cache::{CacheConfig, LocalCache, SystemDisk};
use tierstore::{Clock, PutBody, PutClaim};
use tierstore_testkit::dir::TestDir;

use super::*;

/// The key of a test, under the prefix the cache keeps.
const K: &str = "img/k";

/// The age after which a hit is checked.
const AFTER: Duration = Duration::from_secs(60);

/// A cache over `proxy` that keeps what is under `img/` and checks a hit older than [`AFTER`] by the
/// wall clock of the world, in a scratch directory kept as long as the returned guard.
fn cache_over(world: &World, proxy: Arc<Proxy>) -> (TestDir, LocalCache) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-marks-e2e-cache");
    let policies = vec![("img/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
    config.revalidate_after = Some(AFTER);
    config.write_through = true;
    let clock: Arc<dyn Clock> = world.clock.clone();
    let cache = LocalCache::open(config, proxy, Arc::new(SystemDisk), clock).unwrap();
    (dir, cache)
}

/// An overwrite by `proxy`.
async fn overwrite(proxy: &Proxy, text: &str) {
    let claim = PutClaim::default();
    let put = proxy.put_plain(K, PutBody::Held(body(text)), &claim);
    put.await.unwrap();
}

/// Asserts the exception and its bound: a cache that holds a key serves it from the disk with no
/// request to the stores while the entry is younger than `revalidate_after`, even after another
/// proxy overwrote the key; once the entry is older, one hit is checked by a single HEAD through
/// the proxy, which is one hot HEAD and nothing else (no marker, no cold, no body), finds another
/// size, drops the entry, and is served the new bytes.
#[tokio::test]
async fn a_cache_serves_an_overwritten_key_until_revalidate_after_and_then_checks_with_one_hot_head()
 {
    let world = World::new().await;
    let writer = world.proxy("p1").await;
    let upstream = world.proxy("p2").await;
    writer.put(K, body("short")).await.unwrap();
    let (_dir, cache) = cache_over(&world, upstream);
    assert_eq!(cache.get(K, None).await.unwrap(), body("short"));

    overwrite(&writer, "a longer text").await;
    released(&world, K).await;
    let mut seen = world.cursor();
    world.clock.advance(59_000);
    assert_eq!(cache.get(K, None).await.unwrap(), body("short"));
    assert_eq!(world.since(&mut seen), []);

    world.clock.advance(2_000);
    let mut seen = world.cursor();
    assert_eq!(cache.get(K, None).await.unwrap(), body("a longer text"));
    let requests = world.since(&mut seen);
    assert_eq!(requests[0], req(Store::Hot, Op::Head, K), "{requests:?}");
    assert_eq!(count(&requests, Store::Marks, Op::Get), 0);
    assert_eq!(count(&requests, Store::Cold, Op::Head), 0);
}

/// Asserts that a key that was deleted is served by a cache until its entry is older than
/// `revalidate_after` and is then `NotFound`, the check being a HEAD through the proxy that finds
/// the key deleted (`gone`, with the cold copy still there), and that the entry is gone.
#[tokio::test]
async fn a_cache_asks_again_after_revalidate_after_and_finds_a_deleted_key_absent() {
    let world = World::new().await;
    let writer = world.proxy("p1").await;
    let upstream = world.proxy("p2").await;
    writer.put(K, body("bytes")).await.unwrap();
    assert_eq!(world.leader("r1").step().await.changed, 1);
    let (_dir, cache) = cache_over(&world, upstream);
    assert_eq!(cache.get(K, None).await.unwrap(), body("bytes"));

    writer.delete(K).await.unwrap();
    released(&world, K).await;
    world.clock.advance(30_000);
    assert_eq!(cache.get(K, None).await.unwrap(), body("bytes"));

    world.clock.advance(31_000);
    let got = cache.get(K, None).await;
    assert_eq!(got, Err(Error::NotFound(K.to_owned())));
    assert_eq!(cache.held(), (0, 0));
}

/// Asserts that a key that did not change costs one hot HEAD an interval and no body: the
/// check finds the size, serves the disk copy, and the entry is young again, so the hits that
/// follow within the interval make no request.
#[tokio::test]
async fn a_cache_hit_older_than_revalidate_after_of_an_unchanged_key_costs_one_hot_head() {
    let world = World::new().await;
    let upstream = world.proxy("p1").await;
    upstream.put(K, body("bytes")).await.unwrap();
    let (_dir, cache) = cache_over(&world, upstream);
    assert_eq!(cache.get(K, None).await.unwrap(), body("bytes"));

    world.clock.advance(61_000);
    let mut seen = world.cursor();
    assert_eq!(cache.get(K, None).await.unwrap(), body("bytes"));
    assert_eq!(world.since(&mut seen), [req(Store::Hot, Op::Head, K)]);
    assert_eq!(cache.get(K, None).await.unwrap(), body("bytes"));
    assert_eq!(world.since(&mut seen), []);
}

/// Asserts what a put through the cache of a build node costs the stores in this stack: the cache's
/// own HEAD of the key before the body (a miss through the proxy: the hot HEAD, the marker GET and
/// the cold HEAD), the create-only put of a new key (the same three reads, then the object and the
/// marker), and, since a memory hot tier gives no tag with its answer to a write, the HEAD that
/// learns the tag for the cache's copy. The pre-HEAD is the one that design 6.1 counts as gone and
/// that its section 10.4 moves to pull request 10; an S3 hot tier answers with the tag, and the HEAD
/// after goes. The cache then serves the bytes it kept, with no request.
#[tokio::test]
async fn a_put_through_the_cache_costs_its_own_head_before_the_create_only_put_of_the_proxy() {
    let world = World::new().await;
    let upstream = world.proxy("p1").await;
    let (_dir, cache) = cache_over(&world, upstream);
    let mut seen = world.cursor();
    let chunks: Vec<Result<bytes::Bytes, Error>> = vec![Ok(body("pushed"))];
    let stream = futures::StreamExt::boxed(futures::stream::iter(chunks));
    cache.put_stream(K, Some(6), stream).await.unwrap();
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Put, K),
            req(Store::Hot, Op::Head, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Marks, Op::Put, K),
            req(Store::Cold, Op::Head, K),
            req(Store::Cold, Op::Head, K),
        ]
    );
    assert_eq!(world.hot_text(K).await.as_deref(), Some("pushed"));
    assert_eq!(cache.get(K, None).await.unwrap(), body("pushed"));
    assert_eq!(world.since(&mut seen), []);
}
