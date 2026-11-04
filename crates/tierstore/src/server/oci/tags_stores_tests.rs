//! Tags of the OCI front over the stores it runs on: the proxy over two memory tiers, with a
//! version moved from the hot tier to the cold one as an eviction leaves it, and directory
//! backends that share a directory. A tag
//! move writes a new version, so the proxy, whose objects are write-once, takes it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use hyper::{Method, StatusCode};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Barrier;

use super::names::Digest;
use super::push_manifest_tests::image;
use super::push_tests::{Rig, digest_of, front_over, hex_of};
use super::tags;
use super::tags_tests::{Gated, manifests, version};
use crate::{
    Backend, DirBackend, Error, FaultyBackend, MemoryBackend, Proxy, ProxyConfig, TestClock,
};

/// A proxy over two memory tiers, and the tiers.
async fn proxy_over_memory(
    clock: &Arc<TestClock>,
) -> (Arc<Proxy>, Arc<MemoryBackend>, Arc<MemoryBackend>) {
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let cold = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let mut config = ProxyConfig::new("p1", 1 << 20, clock.clone());
    config.usage_refresh = Duration::ZERO;
    let opened = crate::server::testing::live_proxy(hot.clone(), cold.clone(), config).await;
    (Arc::new(opened.expect("a proxy")), hot, cold)
}

/// Leaves `key` as an eviction does: the cold tier holds the object and the hot tier does not.
async fn evict(hot: &MemoryBackend, cold: &MemoryBackend, key: &str) {
    let body = hot.get(key, None).await.expect("the hot object");
    cold.put(key, body).await.expect("the copy");
    hot.delete(key).await.expect("the eviction");
}

/// Asserts the case that was 500 `UNKNOWN`: a tag is pushed over the proxy and its version is
/// left in the cold tier only (as the copy and the eviction of the entry reconciler leave it),
/// and the tag is then moved. The move is 201 and not a failure (the proxy replaces nothing: the move is a new object), a pull by tag
/// serves the new manifest and not the old one (the hot tier holds the newer version and the cold
/// tier the older, and the listing gives the newer first), after the new version is evicted too
/// it still does, and the first version in the cold tier is byte for byte what it was.
#[tokio::test(start_paused = true)]
async fn a_tag_moved_after_its_version_left_the_hot_tier_is_pulled_as_moved() {
    let clock = Arc::new(TestClock::new());
    let (proxy, hot, cold) = proxy_over_memory(&clock).await;
    let (front, metrics) = front_over(proxy.clone());
    let rig = Rig {
        faulty: Arc::new(FaultyBackend::new(cold.clone())),
        memory: cold.clone(),
        front,
        metrics,
    };
    let made = manifests(&rig, 2).await;
    let (first, second) = (made[0].clone(), made[1].clone());

    let put = rig.tag("app", "latest", &first.0).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    let key = version("latest", 0);
    evict(&hot, &cold, &key).await;
    assert!(hot.get(&key, None).await.is_err(), "the version left hot");
    assert_eq!(
        cold.get(&key, None).await.expect("cold"),
        first.1.as_bytes()
    );

    let moved = rig.tag("app", "latest", &second.0).await;
    assert_eq!(moved.status, StatusCode::CREATED, "{}", moved.text());
    let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(by_tag.body, second.0);
    assert_eq!(by_tag.header("docker-content-digest"), second.1);
    let page = proxy.list_page("oci/tags/app/latest/", None, 1).await;
    let newest: Vec<String> = page.expect("a page").into_iter().map(|o| o.key).collect();
    assert_eq!(newest, [version("latest", 1)]);

    evict(&hot, &cold, &version("latest", 1)).await;
    let again = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(again.body, second.0);
    assert_eq!(
        cold.get(&key, None).await.expect("cold"),
        first.1.as_bytes()
    );
    let held = cold.get(&version("latest", 1), None).await;
    assert_eq!(held.expect("cold"), second.1.as_bytes());
}

/// Asserts that the proxy's listing of one key gives the smallest key of both tiers, so the
/// newest version of a tag wherever it is: the hot tier holds the version after the one the cold
/// tier holds, and a page of one key is the hot one, a page of two has both, newest first.
#[tokio::test(start_paused = true)]
async fn the_proxy_lists_the_newer_version_first_when_the_older_is_in_the_cold_tier() {
    let clock = Arc::new(TestClock::new());
    let (proxy, hot, cold) = proxy_over_memory(&clock).await;
    let (older, newer) = (digest_of(b"older"), digest_of(b"newer"));
    cold.put(&version("t", 0), Bytes::from(older))
        .await
        .expect("put");
    hot.put(&version("t", 1), Bytes::from(newer))
        .await
        .expect("put");
    let keys =
        |page: Vec<crate::Listed>| -> Vec<String> { page.into_iter().map(|o| o.key).collect() };
    let one = proxy.list_page("oci/tags/app/t/", None, 1).await;
    assert_eq!(keys(one.expect("a page")), [version("t", 1)]);
    let both = proxy.list_page("oci/tags/app/t/", None, 10).await;
    assert_eq!(
        keys(both.expect("a page")),
        [version("t", 1), version("t", 0)]
    );
}

/// Asserts that two directory backends over one directory (two processes would be the same) that
/// both found a tag without a version before either wrote have exactly one winner: one create
/// is taken, the other is `PreconditionFailed`, and the version holds the winner's digest.
#[tokio::test]
async fn two_directory_backends_over_one_directory_have_one_winner() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-oci-tags-race");
    let one: Arc<dyn Backend> = Arc::new(DirBackend::open(dir.path()).expect("a directory"));
    let two: Arc<dyn Backend> = Arc::new(DirBackend::open(dir.path()).expect("a directory"));
    let barrier = Arc::new(Barrier::new(2));
    let (gate_one, gate_two) = (
        Gated::over(one.clone(), barrier.clone()),
        Gated::over(two.clone(), barrier),
    );
    gate_one.armed.store(true, Ordering::SeqCst);
    gate_two.armed.store(true, Ordering::SeqCst);
    let (text_one, text_two) = (digest_of(b"one"), digest_of(b"two"));
    let digest_one = Digest::parse(&text_one).expect("a digest");
    let digest_two = Digest::parse(&text_two).expect("a digest");
    let prefix = "oci/tags/app/latest/";
    let (a, b) = tokio::join!(
        tags::point(&gate_one, prefix, &digest_one),
        tags::point(&gate_two, prefix, &digest_two)
    );
    let winner = match (a, b) {
        (Ok(()), Err(Error::PreconditionFailed(_))) => text_one,
        (Err(Error::PreconditionFailed(_)), Ok(())) => text_two,
        other => panic!("not one winner and one loser: {other:?}"),
    };
    let held = one
        .get(&version("latest", 0), None)
        .await
        .expect("a version");
    assert_eq!(held, winner.as_bytes());
    assert_eq!(one.list(prefix).await.expect("a listing").len(), 1);
}

/// Asserts that on a directory tier, where the whole key is one file name of at most 255 bytes,
/// a tag whose key (`oci/tags/<name>/<tag>/` and the 20 digits) is exactly 255 bytes is pushed
/// and pulled, and one byte more is not taken (500, nothing tagged), so that the 20 digits of a
/// version are accounted for in the limit the docs state.
#[tokio::test]
async fn a_version_key_of_255_bytes_fits_a_directory_tier_and_one_more_does_not() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-oci-tags-name");
    let backend = Arc::new(DirBackend::open(dir.path()).expect("a directory"));
    let (front, metrics) = front_over(backend.clone());
    let rig = Rig {
        faulty: Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new()))),
        memory: Arc::new(MemoryBackend::new()),
        front,
        metrics,
    };
    let config = b"{}";
    let layer = b"layer";
    for bytes in [&config[..], &layer[..]] {
        let key = format!("oci/blobs/sha256/{}", hex_of(bytes));
        backend
            .put(&key, Bytes::copy_from_slice(bytes))
            .await
            .expect("put");
    }
    let manifest = image(&digest_of(config), &[digest_of(layer)]);
    let name = "a".repeat(100);
    let fits = "t".repeat(255 - 31 - 100);
    let over = "u".repeat(255 - 31 - 100 + 1);
    let target = format!("/v2/{name}/manifests/{fits}");
    let put = rig
        .ask(
            Method::PUT,
            &target,
            &[],
            super::push_tests::full(&manifest),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    let got = rig.send(Method::GET, &target).await;
    assert_eq!(got.body, manifest);
    let target = format!("/v2/{name}/manifests/{over}");
    let put = rig
        .ask(
            Method::PUT,
            &target,
            &[],
            super::push_tests::full(&manifest),
        )
        .await;
    put.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
    let got = rig.send(Method::GET, &target).await;
    got.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
}
