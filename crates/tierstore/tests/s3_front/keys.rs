//! Keys that S3 accepts, over the loopback socket of the front: the S3 client of `objstore-s3`
//! (which escapes the path and signs it as sent) against the front (which checks the signature
//! of the path as it arrives and decodes it), and a read cache whose upstream is the front.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use objstore_s3::{ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore, PageSize};
use tierstore::s3::{Credentials, S3Config};
use tierstore::server::local_cache::{CacheConfig, LocalCache, SystemDisk};
use tierstore::{Backend, S3Backend, SystemClock};
use tierstore_testkit::dir::TestDir;

use crate::raw::call;
use crate::support::{BUCKET, Harness, KEY_ID, SECRET};

/// Keys S3 accepts that the old character set refused, one kind of difficulty each.
const NAMES: [&str; 12] = [
    "cas/f(x) g(y).txt",
    "cas/plus+sign",
    "cas/100%",
    "cas/a%41",
    "cas/and&amp;",
    "cas/less<than>",
    "cas/caf\u{e9} \u{65e5}\u{672c}",
    "cas/what?x=1#frag",
    "cas//double",
    "cas/trailing/",
    "cas/tab\there\nline\rbreak",
    "cas/emoji \u{1f600}",
];

fn sorted() -> Vec<String> {
    let mut names: Vec<String> = NAMES.iter().map(|name| (*name).to_owned()).collect();
    names.sort();
    names
}

/// Asserts that the S3 client puts, heads, reads (whole and by range), lists (whole, in pages of
/// one, and under a prefix with parentheses and a space), starts an upload of, completes an upload
/// of, and deletes keys with `(`, `)`, a space, `+`, `%`, `&`, `<`, `?`, `#`, non-ASCII text and
/// control characters against the front, which verifies every signature, and that a key of 1024
/// bytes is taken and one of 1025 bytes is refused by the front with 400 `KeyTooLongError`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_s3_client_and_the_front_agree_on_keys_with_s3_characters() {
    let harness = Harness::start(|_| {}).await;
    let store = harness.store(Capabilities::default());
    for name in NAMES {
        let key = ObjectKey::new(name).unwrap();
        let body = Bytes::from(format!("body of {name}"));
        store.put_new(&key, body.clone(), None).await.unwrap();
        assert_eq!(store.len(&key).await.unwrap(), Some(body.len() as u64));
        let whole = ByteRange::new(0, body.len() as u64).unwrap();
        assert_eq!(
            store.get_range(&key, whole).await.unwrap(),
            body,
            "{name:?}"
        );
        let part = ByteRange::new(3, 4).unwrap();
        assert_eq!(store.get_range(&key, part).await.unwrap(), body.slice(3..7));
    }

    let all = KeyPrefix::default();
    let page = store.list(&all, None, PageSize::MAX).await.unwrap();
    let listed: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(listed, sorted());

    let one = PageSize::new(1).unwrap();
    let (mut seen, mut after) = (Vec::new(), None);
    loop {
        let page = store.list(&all, after.as_ref(), one).await.unwrap();
        seen.extend(page.objects.iter().map(|o| o.key.as_str().to_owned()));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, sorted());

    let prefix = KeyPrefix::new("cas/f(x) g").unwrap();
    let page = store.list(&prefix, None, PageSize::MAX).await.unwrap();
    assert_eq!(page.objects.len(), 1);

    let upload = ObjectKey::new("cas/up (1)+&<%").unwrap();
    let id = store.create_multipart(&upload).await.unwrap();
    let data = Bytes::from_static(b"part one");
    let body = stream::iter([Ok(data.clone())]).boxed();
    let etag = store
        .upload_part_stream(&upload, &id, 1, data.len() as u64, body)
        .await
        .unwrap();
    let open = store
        .list_multipart_uploads(&all, None, PageSize::MAX)
        .await
        .unwrap();
    assert_eq!(open.uploads.len(), 1);
    assert_eq!(open.uploads[0].key, upload);
    store
        .complete_multipart(&upload, &id, &[(1, etag)])
        .await
        .unwrap();
    let whole = ByteRange::new(0, data.len() as u64).unwrap();
    assert_eq!(store.get_range(&upload, whole).await.unwrap(), data);
    store.delete(&upload).await.unwrap();

    for name in NAMES {
        let key = ObjectKey::new(name).unwrap();
        store.delete(&key).await.unwrap();
        assert_eq!(store.len(&key).await.unwrap(), None, "{name:?}");
    }

    let long = ObjectKey::new(format!("cas/{}", "k".repeat(1020))).unwrap();
    store.put_new(&long, data.clone(), None).await.unwrap();
    assert_eq!(store.len(&long).await.unwrap(), Some(data.len() as u64));
}

/// Asserts, with requests signed by the hand-written signer of these tests (not the client's),
/// that the front takes a put and a read of a key with S3's characters, listing it as written,
/// takes a key of 1024 bytes, and answers a key of 1025 bytes 400 `KeyTooLongError`. (A `..` segment
/// cannot be sent by an HTTP client that rewrites the path, which is why the S3 client refuses it.)
#[tokio::test]
async fn the_front_takes_hand_signed_requests_for_keys_with_s3_characters() {
    let h = Harness::start_fixed(|_| {}).await;
    let create = [("if-none-match", "*")];
    for name in NAMES {
        let put = call(&h, "PUT", name, &[], &create, b"x").await;
        assert_eq!(put.status(), 200, "{name:?}");
        let got = call(&h, "GET", name, &[], &[], b"").await;
        assert_eq!(got.bytes().await.unwrap(), "x", "{name:?}");
    }
    let listed = h.hot_mem.list("cas/").await.unwrap();
    let mut keys: Vec<String> = listed.into_iter().map(|o| o.key).collect();
    keys.sort();
    assert_eq!(keys, sorted());

    let long = "k".repeat(1024);
    assert_eq!(
        call(&h, "PUT", &long, &[], &create, b"x").await.status(),
        200
    );
    let too_long = "k".repeat(1025);
    let refused = call(&h, "PUT", &too_long, &[], &create, b"x").await;
    assert_eq!(refused.status(), 400);
    assert!(
        refused
            .text()
            .await
            .unwrap()
            .contains("<Code>KeyTooLongError</Code>")
    );
}

/// A read cache that keeps `cas/` and writes through, over the front at `harness`.
fn cache_over(harness: &Harness) -> (TestDir, LocalCache) {
    let upstream = S3Config {
        endpoint: format!("http://{}", harness.addr),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        credentials: Credentials::new(KEY_ID, SECRET),
        capabilities: tierstore::s3::Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    let upstream = Arc::new(S3Backend::new(upstream).unwrap());
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-key-rules-cache");
    let policies = vec![("cas/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
    config.write_through = true;
    let cache = LocalCache::open(
        config,
        upstream,
        Arc::new(SystemDisk),
        Arc::new(SystemClock),
    );
    (dir, cache.unwrap())
}

/// Asserts that a read cache whose upstream is the front (itself over the tiers) writes keys with
/// S3's characters through to the front, reads them back, lists them and deletes them, and that
/// a fresh client of the front sees the keys as written: the key travels from the cache's
/// backend, escaped and signed, to the front and back with no change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_over_the_front_carries_keys_with_s3_characters() {
    let harness = Harness::start(|_| {}).await;
    let (_dir, cache) = cache_over(&harness);
    for name in NAMES {
        let body = Bytes::from(format!("body of {name}"));
        cache
            .put(name, body)
            .await
            .unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
    for name in NAMES {
        let want = Bytes::from(format!("body of {name}"));
        assert_eq!(cache.get(name, None).await.unwrap(), want, "{name:?}");
    }
    let listed: Vec<String> = cache
        .list("cas/")
        .await
        .unwrap()
        .into_iter()
        .map(|object| object.key)
        .collect();
    assert_eq!(listed, sorted());

    let store = harness.store(Capabilities::default());
    let page = store
        .list(&KeyPrefix::new("cas/").unwrap(), None, PageSize::MAX)
        .await
        .unwrap();
    let seen: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(seen, sorted());

    for name in NAMES {
        cache.delete(name).await.unwrap();
        assert!(cache.get(name, None).await.is_err(), "{name:?}");
    }
}

/// Asserts the range read of a key with S3's characters through the cache: the bytes asked for
/// come back, from the upstream first and from the kept copy after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ranged_read_of_a_key_with_s3_characters_through_the_cache() {
    let harness = Harness::start(|_| {}).await;
    let (_dir, cache) = cache_over(&harness);
    let name = "cas/f(x) +100%&<\u{e9}";
    cache
        .put(name, Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    let range = tierstore::ByteRange::new(2, 4);
    for _ in 0..2 {
        let got = cache.get(name, Some(range)).await.unwrap();
        assert_eq!(got, Bytes::from_static(b"2345"));
    }
}
