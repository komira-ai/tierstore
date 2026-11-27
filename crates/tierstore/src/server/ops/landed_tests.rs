//! The `ETag` of the answer to a put: the one a HEAD and a GET of the object then give, over a
//! proxy of two memory tiers (under `active` and under `active-marks`), over a backend whose
//! answer names no tag, and over a cache that writes through to an S3 bucket (the loopback S3
//! service of the tests), which is the node of a front over one upstream store.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};
use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::dir::TestDir;

use crate::s3::{Addressing, Capabilities, Credentials, S3Config, S3Tuning};
use crate::server::auth::sign;
use crate::server::local_cache::testing::{never_expires, pattern};
use crate::server::local_cache::{CacheConfig, LocalCache, SystemDisk};
use crate::server::presign::presign_url;
use crate::server::presign::tests::{SECRET as PRESIGN_SECRET, access, grant, request};
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, Rig, SECRET};
use crate::server::{Prefixes, PresignKey, ScopedKey, Server, ServerConfig};
use crate::tiered::marks_delete_tests::Rig as MarksRig;
use crate::{Backend, Error, FaultyBackend, MemoryBackend, Op, S3Backend, TestClock};

const SCOPED: (&str, &str) = ("scoped-a", "scoped-secret-0123456789");
const PRESIGN: &str = "pack-presign";

/// A server over `backend` with the full key alone.
fn server_over(backend: Arc<dyn Backend>) -> (Arc<Server>, ServerConfig) {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
    (Server::new(backend, config.clone()), config)
}

/// Sends `request` to `server` and reads the answer.
async fn answer<B>(server: &Server, request: Request<B>) -> Reply
where
    B: hyper::body::Body + Send,
    B::Data: Send,
    B::Error: Into<crate::server::request::BoxError>,
{
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("a whole body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A call of the full key on the object `key`, with the headers `extra` and, for a `PUT`, the
/// `Content-Length` of the body (which a front over an S3 bucket needs).
async fn call(
    server: &Server,
    config: &ServerConfig,
    method: &str,
    key: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let uri = format!("/{BUCKET}/{key}");
    let length = body.len().to_string();
    let mut headers = extra.to_vec();
    if method == "PUT" {
        headers.push(("content-length", length.as_str()));
    }
    answer(server, sign(config, method, &uri, &headers, body)).await
}

/// The `ETag`s a HEAD and a GET of `key` give, through `server`.
async fn read_back(server: &Server, config: &ServerConfig, key: &str) -> (String, String) {
    let head = call(server, config, "HEAD", key, &[], b"").await;
    let get = call(server, config, "GET", key, &[], b"").await;
    assert_eq!((head.status, get.status), (StatusCode::OK, StatusCode::OK));
    (
        head.header("etag").to_owned(),
        get.header("etag").to_owned(),
    )
}

/// Asserts that `put` is a 200 whose `ETag` is a quoted tag, and gives it.
fn landed(put: &Reply) -> String {
    assert_eq!(put.status, StatusCode::OK, "{}", put.body);
    let etag = put.header("etag");
    assert!(
        etag.len() > 2 && etag.starts_with('"') && etag.ends_with('"'),
        "{etag:?}"
    );
    etag.to_owned()
}

/// Asserts that a plain put (no condition) and a create-only one (`If-None-Match: *`) of a new
/// key over a proxy of two memory tiers each answer 200 with an `ETag`, and that it is the one a
/// HEAD and a GET of the key then give: the tag the hot tier holds the object under. It fails on
/// a front whose answer to a put has no `ETag`, which a client that commits with a create-only
/// put and reads the tag back (object_store, and through it LanceDB, delta-rs and SlateDB)
/// cannot use.
#[tokio::test(start_paused = true)]
async fn put_answers_etag_of_the_landed_object() {
    let rig = Rig::new(1 << 20).await;
    let plain = rig.call("PUT", "plain", b"plain value").await;
    let created = rig.put("created", b"created value").await;
    for (key, put) in [("plain", &plain), ("created", &created)] {
        let etag = landed(put);
        let head = rig.call("HEAD", key, b"").await;
        let get = rig.get(key, None).await;
        assert_eq!((head.header("etag"), get.header("etag")), (&*etag, &*etag));
        let held = rig.hot_mem.head(key).await.unwrap();
        assert_eq!(etag, format!("\"{}\"", held.tag), "{key}");
    }
    assert_ne!(plain.header("etag"), created.header("etag"));
}

/// Asserts that a large put (8 MiB) is answered with the `ETag` that a HEAD and a GET then
/// give, and that a put the hot tier has no room for, which the proxy lands in its cold tier and
/// whose answer therefore names no tag, is answered with the tag of the HEAD after it (the cold
/// tier's), the same that a GET then gives.
#[tokio::test(start_paused = true)]
async fn a_large_put_and_one_landed_in_the_cold_tier_answer_the_etag_a_head_gives() {
    let rig = Rig::new(64 << 20).await;
    let large = pattern(8 << 20);
    let put = rig.call("PUT", "large", &large).await;
    let etag = landed(&put);
    let head = rig.call("HEAD", "large", b"").await;
    let get = rig.get("large", None).await;
    assert_eq!((head.header("etag"), get.header("etag")), (&*etag, &*etag));
    assert_eq!(rig.hot_mem.get("large", None).await.unwrap(), large);

    rig.hot
        .fail_next_on(Op::Put, "cold", Error::Full("hot".to_owned()));
    let put = rig.put("cold", b"cold value").await;
    let etag = landed(&put);
    assert!(rig.hot_mem.head("cold").await.is_err());
    let held = rig.cold_mem.head("cold").await.unwrap();
    assert_eq!(etag, format!("\"{}\"", held.tag));
    let head = rig.call("HEAD", "cold", b"").await;
    let get = rig.get("cold", None).await;
    assert_eq!((head.header("etag"), get.header("etag")), (&*etag, &*etag));
}

/// Asserts that a create-only put answers its `ETag` and a second one of the key answers 412
/// with no `ETag` and leaves the first object, whose tag a HEAD still gives; and that a put the
/// backend refuses (here a failure of the hot tier) has no `ETag` either. It fails on a front
/// that sends the tag of the object that was there with a refusal.
#[tokio::test(start_paused = true)]
async fn a_refused_put_has_no_etag_and_leaves_the_first_one_standing() {
    let rig = Rig::new(1 << 20).await;
    let etag = landed(&rig.put("k", b"first").await);
    let again = rig.put("k", b"second").await;
    assert_eq!(again.status, StatusCode::PRECONDITION_FAILED);
    assert!(again.headers.get("etag").is_none());
    assert_eq!(rig.call("HEAD", "k", b"").await.header("etag"), etag);

    rig.hot
        .fail_next_on(Op::Put, "broken", Error::Other("the disk".to_owned()));
    rig.cold
        .fail_next_on(Op::Put, "broken", Error::Other("the disk".to_owned()));
    let failed = rig.put("broken", b"value").await;
    assert!(failed.status.is_server_error(), "{}", failed.status);
    assert!(failed.headers.get("etag").is_none());
}

/// Asserts that the create-only put of a scoped key and a presigned put each answer the `ETag`
/// that a HEAD then gives: every kind of key whose put stores an object is answered so.
#[tokio::test(start_paused = true)]
async fn a_scoped_put_and_a_presigned_put_answer_the_etag_a_head_gives() {
    let rig = Rig::with(1 << 20, |config| {
        let scopes = Prefixes {
            read: &["cas/s/"],
            write: &["cas/s/"],
            delete: &[],
        };
        config.scoped_keys = vec![ScopedKey::new(SCOPED.0, SCOPED.1, &scopes).unwrap()];
        config.presign_keys = vec![PresignKey::new(PRESIGN, PRESIGN_SECRET, grant()).unwrap()];
    })
    .await;
    let uri = format!("/{BUCKET}/cas/s/scoped");
    let scoped = rig.send_as(SCOPED, "PUT", &uri, &[], b"scoped").await;
    let url = presign_url("PUT", &access(PRESIGN), &request("cas/b/signed", 300), &[]);
    let url = url.unwrap();
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    let presigned = Request::builder()
        .method("PUT")
        .uri(&rest[at..])
        .header("host", &rest[..at])
        .body(Full::new(Bytes::from_static(b"presigned")))
        .unwrap();
    let presigned = rig.send_raw(presigned).await;
    for (key, put) in [("cas/s/scoped", &scoped), ("cas/b/signed", &presigned)] {
        let etag = landed(put);
        assert_eq!(
            rig.call("HEAD", key, b"").await.header("etag"),
            etag,
            "{key}"
        );
    }
}

/// Asserts that over a proxy under `active-marks`, where a plain put replaces, the put of a new
/// key and the put that replaces it each answer the `ETag` a HEAD and a GET then give, and that
/// the two differ: a client that conditions its next write on the tag of its last put names the
/// object that stands. It fails on a front that answers the replaced object's tag.
#[tokio::test]
async fn a_replacing_put_answers_the_new_etag_and_not_the_old_one() {
    let rig = MarksRig::new().await;
    let (server, config) = server_over(rig.proxy.clone());
    let first = call(&server, &config, "PUT", "k", &[], b"first").await;
    let old = landed(&first);
    assert_eq!(
        read_back(&server, &config, "k").await,
        (old.clone(), old.clone())
    );
    let second = call(&server, &config, "PUT", "k", &[], b"second").await;
    let new = landed(&second);
    assert_ne!(new, old);
    assert_eq!(read_back(&server, &config, "k").await, (new.clone(), new));
}

/// Asserts that over a backend whose answer to a put names no tag the front asks with one HEAD
/// after the write and answers its tag; and that a put whose HEAD after it fails is still a 200,
/// with no `ETag`, and the object stored: the put succeeded, and a client that is told it failed
/// would send it again.
#[tokio::test]
async fn a_put_answered_with_no_tag_answers_the_tag_of_a_head_or_none_if_it_fails() {
    let memory = Arc::new(MemoryBackend::new());
    let faulty = Arc::new(FaultyBackend::new(memory.clone()).without_answers());
    let (server, config) = server_over(faulty.clone());
    let put = call(&server, &config, "PUT", "asked", &[], b"asked").await;
    let etag = landed(&put);
    assert_eq!(faulty.calls_on(Op::Head, "asked"), 1);
    assert_eq!(
        etag,
        format!("\"{}\"", memory.head("asked").await.unwrap().tag)
    );
    assert_eq!(
        read_back(&server, &config, "asked").await,
        (etag.clone(), etag)
    );

    faulty.fail_next_on(Op::Head, "unasked", Error::Other("the HEAD".to_owned()));
    let put = call(&server, &config, "PUT", "unasked", &[], b"unasked").await;
    assert_eq!(put.status, StatusCode::OK);
    assert!(put.headers.get("etag").is_none());
    assert_eq!(memory.get("unasked", None).await.unwrap(), "unasked");
    let (head, get) = read_back(&server, &config, "unasked").await;
    assert_eq!(head, get);
    assert_eq!(
        head,
        format!("\"{}\"", memory.head("unasked").await.unwrap().tag)
    );
}

/// A backend over the bucket of the loopback S3 service, with `single_put_max` of 16 MiB, so
/// that a body over it is a multipart upload.
fn s3_backend(s3: &FakeS3) -> Arc<S3Backend> {
    let store = S3Config {
        endpoint: s3.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        addressing: Addressing::Auto,
        ca_pem: None,
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        min_body_rate: None,
    };
    let tuning = S3Tuning {
        single_put_max: 16 << 20,
        ..S3Tuning::default()
    };
    Arc::new(S3Backend::new_tuned(store, tuning).unwrap())
}

/// A front over a cache of 64 MiB that writes through to `upstream` and keeps `cas/` (and no
/// other key), as a node over one upstream store is.
fn cache_front(upstream: Arc<FaultyBackend>, dir: &TestDir) -> (Arc<Server>, ServerConfig) {
    let policies = vec![("cas/".to_owned(), true)];
    let mut cache = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
    cache.write_through = true;
    cache.keep_queue_bytes = 64 << 20;
    never_expires(&mut cache);
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let opened = LocalCache::open(cache, upstream, Arc::new(SystemDisk), clock.clone());
    let cache = opened.unwrap();
    let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
    config.cache = Some(cache.clone());
    config.cache_writes = true;
    let backend: Arc<dyn Backend> = Arc::new(cache);
    (Server::new(backend, config.clone()), config)
}

/// Asserts that through a cache that writes through to an S3 bucket a put answers the `ETag`
/// of the bucket's answer, the one a HEAD and a GET through the front then give and the one the
/// bucket holds the object under: for a small object (one PUT), for a large one (over 16 MiB, a
/// multipart upload, whose tag has a dash), for a key the cache keeps and for one it does not,
/// create-only and plain. The tag comes from the answer to the write: no HEAD reaches the
/// bucket after a put, which a cache that drops its upstream's answer would make the front
/// send. A create-only put of a key that exists is 412 with no `ETag`.
#[tokio::test]
async fn a_put_through_a_cache_that_writes_through_answers_the_upstreams_etag() {
    let s3 = FakeS3::start().await;
    let bucket = s3_backend(&s3);
    let upstream = Arc::new(FaultyBackend::new(bucket.clone()));
    let dir = TestDir::under(std::env::temp_dir(), "tsput-etag-cache");
    let (server, config) = cache_front(upstream.clone(), &dir);
    let large = pattern(17 << 20);
    let small = pattern(3 << 10);
    let create: &[(&str, &str)] = &[("if-none-match", "*")];
    let puts: [(&str, &[(&str, &str)], &[u8]); 4] = [
        ("cas/b/small", create, &small),
        ("cas/b/large", &[], &large),
        ("logs/small", &[], &small),
        ("logs/large", create, &large),
    ];
    for (key, extra, body) in puts {
        let heads = upstream.calls_on(Op::Head, key);
        let put = call(&server, &config, "PUT", key, extra, body).await;
        let etag = landed(&put);
        let plain = extra.is_empty();
        let asked = upstream.calls_on(Op::Head, key) - heads;
        assert_eq!(
            asked,
            u64::from(!plain),
            "{key}: only the HEAD before a create-only put"
        );
        assert_eq!(
            etag,
            format!("\"{}\"", bucket.head(key).await.unwrap().tag),
            "{key}"
        );
        let (head, get) = read_back(&server, &config, key).await;
        assert_eq!((head, get), (etag.clone(), etag.clone()), "{key}");
        assert_eq!(etag.contains('-'), body.len() > 16 << 20, "{key}: {etag}");
    }
    let again = call(&server, &config, "PUT", "cas/b/small", create, b"other").await;
    assert_eq!(again.status, StatusCode::PRECONDITION_FAILED);
    assert!(again.headers.get("etag").is_none());
}

/// Asserts that through a cache that writes through to an S3 bucket a plain put of a key that
/// exists replaces it and answers the new object's `ETag`, which a HEAD and a GET then give and
/// which differs from the old one, also for a key the cache keeps a copy of.
#[tokio::test]
async fn a_replacing_put_through_a_cache_answers_the_new_etag() {
    let s3 = FakeS3::start().await;
    let bucket = s3_backend(&s3);
    let upstream = Arc::new(FaultyBackend::new(bucket.clone()));
    let dir = TestDir::under(std::env::temp_dir(), "tsput-etag-replace");
    let (server, config) = cache_front(upstream, &dir);
    for key in ["cas/b/k", "logs/k"] {
        let old = landed(&call(&server, &config, "PUT", key, &[], b"first").await);
        assert_eq!(
            read_back(&server, &config, key).await,
            (old.clone(), old.clone())
        );
        let new = landed(&call(&server, &config, "PUT", key, &[], b"second").await);
        assert_ne!(new, old, "{key}");
        assert_eq!(
            new,
            format!("\"{}\"", bucket.head(key).await.unwrap().tag),
            "{key}"
        );
        assert_eq!(
            read_back(&server, &config, key).await,
            (new.clone(), new),
            "{key}"
        );
    }
}
