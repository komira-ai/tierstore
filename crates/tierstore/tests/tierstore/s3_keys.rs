//! Keys that S3 accepts, down the whole chain: a read cache over a proxy whose hot and cold tiers
//! are both S3 buckets (the loopback S3 service of `objstore-s3`, which verifies the signature of
//! every request over the path as the client sent it).

use std::sync::Arc;

use objstore_s3::s3::fake::FakeS3;
use tierstore::{Backend, ProxyConfig, S3Backend, TestClock};

use crate::cache_over_proxy::cache_over;
use crate::s3::config;
use crate::support::{body, keys_under, open_live, read};

/// Keys S3 accepts that the old character set refused.
const NAMES: [&str; 8] = [
    "img/f(x) g(y).txt",
    "img/plus+sign",
    "img/100%",
    "img/a%41",
    "img/and&amp;<lt>",
    "img/caf\u{e9} \u{65e5}\u{672c}",
    "img/what?x=1#frag",
    "img//double/",
];

/// A backend over a service of its own that wants signed requests.
async fn tier() -> (FakeS3, Arc<S3Backend>) {
    let s3 = FakeS3::start().await;
    s3.require_credentials("test-key", "test-secret");
    let backend = S3Backend::new(config(s3.endpoint())).unwrap();
    (s3, Arc::new(backend))
}

fn sorted() -> Vec<String> {
    let mut names: Vec<String> = NAMES.iter().map(|name| (*name).to_owned()).collect();
    names.sort();
    names
}

/// Asserts that a cache over a proxy over two S3 tiers writes keys with S3's characters through to
/// the hot tier under their names, reads them back, lists them, and deletes them from the tier, and that a key of
/// 1024 bytes is carried and one of 1025 bytes is `InvalidKey`.
#[tokio::test]
async fn a_cache_over_a_proxy_over_s3_tiers_carries_keys_with_s3_characters() {
    let (hot_s3, hot) = tier().await;
    let (cold_s3, cold) = tier().await;
    let clock = Arc::new(TestClock::new());
    let proxy = ProxyConfig::new("p1", 1 << 30, clock);
    let proxy = open_live(hot, cold, proxy).await.unwrap();
    let (_dir, cache) = cache_over(Arc::new(proxy), |_| {});

    for name in NAMES {
        let value = format!("value of {name}");
        cache
            .put(name, body(&value))
            .await
            .unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
    let held: Vec<String> = hot_s3
        .keys()
        .into_iter()
        .filter(|key| key.starts_with("img/"))
        .collect();
    assert_eq!(held, sorted(), "the hot tier holds the keys as written");
    for name in NAMES {
        assert_eq!(read(&cache, name).await, body(&format!("value of {name}")));
    }
    assert_eq!(keys_under(&cache, "img/").await, sorted());

    for name in NAMES {
        cache.delete(name).await.unwrap();
    }
    let left: Vec<String> = hot_s3
        .keys()
        .into_iter()
        .chain(cold_s3.keys())
        .filter(|key| key.starts_with("img/"))
        .collect();
    assert_eq!(left, Vec::<String>::new());

    let long = format!("img/{}", "k".repeat(1020));
    cache.put(&long, body("long")).await.unwrap();
    let too_long = format!("img/{}", "k".repeat(1021));
    let refused = cache.put(&too_long, body("long")).await;
    assert!(
        matches!(refused, Err(tierstore::Error::InvalidKey(_))),
        "{refused:?}"
    );
}
