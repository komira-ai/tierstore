//! The same behaviour tests of the local cache, run over each kind of upstream the cache takes
//! through the `Backend` interface: a plain store, and the in-process proxy of two tiers (the
//! upstream of a storage node's cache). Each test is written once, as a function of an upstream
//! and the stores behind it, and run by a test for each kind, so that a difference between the
//! two is a failure of the one that differs.
//!
//! The tests read nothing of the clock and wait for no keep: they assert what a `get` fills
//! (it fills before it answers) and what the upstream holds after a write.

use std::future::Future;
use std::sync::Arc;

use tierstore::{Backend, ByteRange, Error, MemoryBackend, Proxy, ProxyConfig, TestClock};

use crate::cache_over_proxy::cache_over;
use crate::support::{assert_absent, body, chunks, read};

/// An upstream and the store a test puts the objects it reads in: the store itself, or, behind
/// the proxy, its cold tier (so that the proxy has to find the object there).
struct Upstream {
    upstream: Arc<dyn Backend>,
    behind: Arc<MemoryBackend>,
}

/// A plain store as the upstream.
fn plain() -> Upstream {
    let store = Arc::new(MemoryBackend::new());
    Upstream {
        upstream: store.clone(),
        behind: store,
    }
}

/// An in-process proxy over two stores as the upstream; `behind` is its cold tier.
async fn proxied() -> Upstream {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let config = ProxyConfig::new("p1", 1 << 20, clock);
    let proxy = Proxy::open(hot, cold.clone(), config).await.unwrap();
    Upstream {
        upstream: Arc::new(proxy),
        behind: cold,
    }
}

/// Runs `suite` over each kind of upstream.
async fn over_each<F, Fut>(suite: F)
where
    F: Fn(Upstream) -> Fut,
    Fut: Future<Output = ()>,
{
    suite(plain()).await;
    suite(proxied().await).await;
}

/// A read of an object fills the cache, and the next read is a hit that changes nothing.
async fn read_fills_then_hits(up: Upstream) {
    up.behind.put("img/a", body("some bytes")).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("some bytes"));
    assert_eq!(cache.held(), (10, 1));
    assert_eq!(cache.get("img/a", None).await.unwrap(), body("some bytes"));
    assert_eq!(cache.held(), (10, 1));
}

/// A ranged read of a key the cache keeps gives the bytes of the range, and a read of a key
/// that is not there is `NotFound`, whole or ranged, and fills nothing.
async fn ranges_and_misses(up: Upstream) {
    up.behind.put("img/r", body("0123456789")).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    let asked = Some(ByteRange::new(2, 3));
    assert_eq!(cache.get("img/r", asked).await.unwrap(), body("234"));
    let missing = cache.get("img/none", None).await;
    assert_eq!(missing, Err(Error::NotFound("img/none".to_owned())));
    let ranged = cache.get("img/none", asked).await;
    assert_eq!(ranged, Err(Error::NotFound("img/none".to_owned())));
    assert_eq!(
        cache.head("img/none").await,
        Err(Error::NotFound("img/none".to_owned()))
    );
}

/// A key the cache does not keep is read from the upstream every time and never held.
async fn unkept_keys_pass_through(up: Upstream) {
    up.behind.put("other/a", body("pass")).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    assert_eq!(cache.get("other/a", None).await.unwrap(), body("pass"));
    assert_eq!(cache.held(), (0, 0));
}

/// A HEAD through the cache gives the size of the upstream's object.
async fn head_gives_the_size(up: Upstream) {
    up.behind.put("img/h", body("12345")).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    assert_eq!(cache.head("img/h").await.unwrap().size, 5);
}

/// A create-only put goes to the upstream, a second put of the key is `AlreadyExists` and leaves
/// the first object, and a read gives the first object's bytes.
async fn put_is_create_only(up: Upstream) {
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    cache
        .put_stream("other/p", Some(3), chunks(&["one"]))
        .await
        .unwrap();
    let again = cache.put_stream("other/p", Some(3), chunks(&["two"])).await;
    assert_eq!(again, Err(Error::AlreadyExists("other/p".to_owned())));
    assert_eq!(read(&*up.upstream, "other/p").await, body("one"));
    assert_eq!(cache.get("other/p", None).await.unwrap(), body("one"));
}

/// A delete through the cache removes the object from the upstream and from the cache, and a
/// delete of a key that is not there succeeds.
async fn delete_goes_through(up: Upstream) {
    up.behind.put("img/d", body("gone soon")).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    assert_eq!(cache.get("img/d", None).await.unwrap(), body("gone soon"));
    assert_eq!(cache.held(), (9, 1));
    cache.delete("img/d").await.unwrap();
    assert_eq!(cache.held(), (0, 0));
    assert_absent(&*up.upstream, "img/d").await;
    assert_eq!(
        cache.get("img/d", None).await,
        Err(Error::NotFound("img/d".to_owned()))
    );
    cache.delete("img/d").await.unwrap();
}

/// A listing through the cache is the upstream's listing.
async fn listing_is_the_upstreams(up: Upstream) {
    up.behind.put("img/l1", body("a")).await.unwrap();
    up.behind.put("img/l2", body("bb")).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    let keys: Vec<(String, u64)> = cache
        .list("img/")
        .await
        .unwrap()
        .into_iter()
        .map(|listed| (listed.key, listed.size))
        .collect();
    assert_eq!(keys, [("img/l1".to_owned(), 1), ("img/l2".to_owned(), 2)]);
}

/// A key of the reserved prefixes is refused by the cache as `Reserved`, with nothing written.
async fn reserved_keys_are_refused(up: Upstream) {
    let (_dir, cache) = cache_over(up.upstream.clone(), |_| {});
    let refused = cache
        .put_stream("_tiering/x", Some(1), chunks(&["x"]))
        .await;
    assert_eq!(refused, Err(Error::Reserved("_tiering/x".to_owned())));
    assert_absent(&*up.upstream, "_tiering/x").await;
}

/// An object larger than the whole-fill limit is read in windows and given in ranges.
async fn windowed_reads_give_the_bytes(up: Upstream) {
    let data = "0123456789abcdef0123456789abcdef";
    up.behind.put("img/w", body(data)).await.unwrap();
    let (_dir, cache) = cache_over(up.upstream.clone(), |config| {
        config.whole_limit = 4;
        config.fill_chunk = 8;
    });
    let first = Some(ByteRange::new(0, 20));
    assert_eq!(cache.get("img/w", first).await.unwrap(), body(&data[..20]));
    let asked = Some(ByteRange::new(5, 20));
    assert_eq!(cache.get("img/w", asked).await.unwrap(), body(&data[5..25]));
}

/// A read-only cache refuses a write as every cache without `write_through` does, and the
/// upstream is left as it was.
async fn a_read_cache_refuses_writes(up: Upstream) {
    let (_dir, cache) = cache_over(up.upstream.clone(), |config| config.write_through = false);
    let refused = cache.put_stream("other/w", Some(1), chunks(&["w"])).await;
    assert!(matches!(refused, Err(Error::Unsupported(_))), "{refused:?}");
    assert_absent(&*up.upstream, "other/w").await;
}

macro_rules! each_upstream {
    ($($suite:ident),* $(,)?) => {
        mod suites {
            use super::*;
            $(
                #[tokio::test]
                async fn $suite() {
                    over_each(super::$suite).await;
                }
            )*
        }
    };
}

each_upstream!(
    read_fills_then_hits,
    ranges_and_misses,
    unkept_keys_pass_through,
    head_gives_the_size,
    put_is_create_only,
    delete_goes_through,
    listing_is_the_upstreams,
    reserved_keys_are_refused,
    windowed_reads_give_the_bytes,
    a_read_cache_refuses_writes,
);
