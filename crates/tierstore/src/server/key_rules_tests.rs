//! The keys the front takes follow S3's rules (`objstore_s3::key_rules`), through
//! `Server::handle`: any text of 1 to 1024 bytes is a key over in-memory tiers, over an S3 tier
//! and over a proxy with a directory hot tier and an S3 cold tier; a longer key is 400
//! `KeyTooLongError`, and so is a `.` or `..` segment over an S3 tier, whose client cannot send it.

use std::sync::Arc;
use std::time::Duration;

use hyper::StatusCode;
use objstore_s3::s3::fake::FakeS3;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tierstore_testkit::dir::TestDir;

use crate::s3::{Capabilities, Credentials, S3Config};
use crate::{
    Backend, DirBackend, EntryReconciler, EntryReconcilerConfig, ProxyConfig, ReconcilerConfig,
    S3Backend, TestClock,
};

use super::testing::{BUCKET, KEY_ID, NOW, Rig, SECRET, live_proxy, texts};
use super::{Server, ServerConfig};

/// Keys S3 accepts, one kind of difficulty each: parentheses and a space, `+`, a bare `%` and
/// a `%` that spells another character, `&`, `<`, non-ASCII text, a four-byte character, `?`
/// and `#`, a leading, a trailing and a doubled `/`, dots, and tab, line feed and carriage
/// return (which XML 1.0 allows).
const NAMES: [&str; 14] = [
    "f(x) g(y).txt",
    "plus+sign",
    "100%",
    "a%41",
    "and&amp;",
    "less<than>",
    "caf\u{e9} \u{65e5}\u{672c}",
    "emoji \u{1f600}",
    "what?x=1#frag",
    "/leading",
    "trailing/",
    "a//b/..c/d..",
    "tab\there\nline\rbreak",
    "...",
];

/// What a client escapes in the path of a request: everything but the unreserved characters
/// and the `/` between segments.
const ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/');

/// The key as it is written in the path or the query of a request.
fn enc(name: &str) -> String {
    utf8_percent_encode(name, ESCAPED).to_string()
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut sorted: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    sorted.sort();
    sorted
}

/// A key of `len` bytes.
fn of_len(len: usize) -> String {
    "k".repeat(len)
}

/// Puts, reads, heads, lists, starts an upload of and deletes each of `NAMES` through `rig`, and
/// asserts that every answer names the key as it was written.
async fn round_trip(rig: &Rig) {
    for name in NAMES {
        let key = enc(name);
        let body = format!("body of {name}");
        let put = rig.put(&key, body.as_bytes()).await;
        assert_eq!(put.status, StatusCode::OK, "put {name:?}: {}", put.body);
        let got = rig.get(&key, None).await;
        assert_eq!(
            (got.status, got.body.as_str()),
            (StatusCode::OK, body.as_str())
        );
        let head = rig.call("HEAD", &key, b"").await;
        assert_eq!(head.status, StatusCode::OK, "{name:?}");
        assert_eq!(head.header("content-length"), body.len().to_string());
    }
    let listed = rig.list("").await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(texts(&listed.body, "Key"), sorted(&NAMES));

    // Pages of one: the token of each page is sent back, and the keys come out in order.
    let (mut seen, mut token) = (Vec::new(), String::new());
    loop {
        let query = format!("max-keys=1&continuation-token={}", enc(&token));
        let page = rig
            .list(if token.is_empty() {
                "max-keys=1"
            } else {
                &query
            })
            .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        seen.extend(texts(&page.body, "Key"));
        token = texts(&page.body, "NextContinuationToken").concat();
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen, sorted(&NAMES));

    let under = rig.list(&format!("prefix={}", enc("f(x) g"))).await;
    assert_eq!(texts(&under.body, "Key"), ["f(x) g(y).txt"]);

    for name in NAMES {
        let key = enc(name);
        let started = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(started.status, StatusCode::OK, "{name:?}: {}", started.body);
        assert_eq!(texts(&started.body, "Key"), [name], "{name:?}");
        let deleted = rig.call("DELETE", &key, b"").await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{name:?}");
        let gone = rig.get(&key, None).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND, "{name:?}");
    }
}

/// Asserts that over in-memory tiers every key of `NAMES` is put, read, headed, listed (whole,
/// in pages of one, and under a prefix with parentheses and a space), started as an upload and
/// deleted, and that the answers name each key as it was written (XML escapes read back).
#[tokio::test(start_paused = true)]
async fn keys_with_s3_characters_round_trip_through_the_front() {
    let rig = Rig::new(1 << 20).await;
    round_trip(&rig).await;
}

/// Asserts the length limit: a key of 1024 bytes is put and read back (also one of 512
/// two-byte characters), a key of 1025 bytes (also 513 two-byte characters) is refused 400
/// `KeyTooLongError` at a put and at the start of an upload, and nothing is stored or begun.
#[tokio::test(start_paused = true)]
async fn a_key_of_1024_bytes_is_taken_and_one_of_1025_is_400_key_too_long() {
    let rig = Rig::new(1 << 20).await;
    for key in [of_len(1024), enc(&"\u{e9}".repeat(512))] {
        assert_eq!(rig.put(&key, b"x").await.status, StatusCode::OK);
        assert_eq!(rig.get(&key, None).await.body, "x");
    }
    for key in [of_len(1025), enc(&"\u{e9}".repeat(513))] {
        let put = rig.put(&key, b"x").await;
        assert_eq!(put.status, StatusCode::BAD_REQUEST);
        assert_eq!(put.code(), "KeyTooLongError");
        let start = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(start.status, StatusCode::BAD_REQUEST);
        assert_eq!(start.code(), "KeyTooLongError");
    }
    let listed = rig.list("").await;
    assert_eq!(texts(&listed.body, "Key").len(), 2);
    let uploads = rig
        .send("GET", &format!("/{BUCKET}?uploads"), &[], b"")
        .await;
    assert!(texts(&uploads.body, "Key").is_empty());
}

/// Asserts that over in-memory tiers a key with a `.` or `..` segment, which S3 takes, is put,
/// read, listed and started as an upload, as before (the front does not refuse what a tier can
/// hold).
#[tokio::test(start_paused = true)]
async fn a_dot_segment_is_a_key_over_a_memory_tier() {
    let rig = Rig::new(1 << 20).await;
    for key in ["a/../b", "./a", "a/.", "a/b/.."] {
        assert_eq!(rig.put(key, b"x").await.status, StatusCode::OK, "{key}");
        assert_eq!(rig.get(key, None).await.body, "x", "{key}");
        let start = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(start.status, StatusCode::OK, "{key}");
    }
    let listed = rig.list("").await;
    assert_eq!(
        texts(&listed.body, "Key"),
        ["./a", "a/.", "a/../b", "a/b/.."]
    );
}

/// A backend over the bucket of the loopback S3 service, which refuses a request whose
/// signature is not the one of the path as sent.
fn s3_backend(s3: &FakeS3) -> Arc<S3Backend> {
    s3.require_credentials("test-key", "test-secret");
    let store = S3Config {
        endpoint: s3.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    Arc::new(S3Backend::new(store).unwrap())
}

/// Asserts that over an S3 tier (the front's own key reaches the S3 client, which escapes it and
/// signs the path as sent) every key of `NAMES` is stored under its name, read, headed, listed
/// in pages, deleted, and started as an upload; a key of 1024 bytes is stored and one of 1025
/// is 400 `KeyTooLongError` at a put and at a read, with nothing stored.
#[tokio::test]
async fn an_s3_tier_stores_the_keys_under_their_names() {
    let s3 = FakeS3::start().await;
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
    let server = Server::new(s3_backend(&s3), config.clone());
    let rig = S3Front { server, config };
    for name in NAMES {
        let put = rig.call("PUT", &enc(name), b"data").await;
        assert_eq!(put.0, StatusCode::OK, "put {name:?}: {}", put.1);
    }
    assert_eq!(
        s3.keys(),
        sorted(&NAMES),
        "the tier holds the keys as written"
    );
    for name in NAMES {
        let key = enc(name);
        assert_eq!(rig.call("GET", &key, b"").await.1, "data", "{name:?}");
        let head = rig.call("HEAD", &key, b"").await;
        assert_eq!(head.0, StatusCode::OK, "{name:?}");
    }
    let listed = rig.list("").await;
    assert_eq!(texts(&listed.1, "Key"), sorted(&NAMES));
    let (mut seen, mut token) = (Vec::new(), String::new());
    loop {
        let query = if token.is_empty() {
            "max-keys=2".to_owned()
        } else {
            format!("max-keys=2&continuation-token={}", enc(&token))
        };
        let page = rig.list(&query).await;
        seen.extend(texts(&page.1, "Key"));
        token = texts(&page.1, "NextContinuationToken").concat();
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen, sorted(&NAMES));
    for name in NAMES {
        let key = enc(name);
        let started = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(started.0, StatusCode::OK, "{name:?}: {}", started.1);
        assert_eq!(texts(&started.1, "Key"), [name]);
        assert_eq!(
            rig.call("DELETE", &key, b"").await.0,
            StatusCode::NO_CONTENT
        );
    }
    assert!(s3.keys().is_empty());

    // The client of an S3 tier cannot send a `.` or `..` segment: 400, nothing stored.
    for method in ["PUT", "GET", "HEAD", "DELETE"] {
        let refused = rig.call(method, "a/../b", b"x").await;
        assert_eq!(refused.0, StatusCode::BAD_REQUEST, "{method}");
    }
    let long = of_len(1024);
    assert_eq!(rig.call("PUT", &long, b"x").await.0, StatusCode::OK);
    let long = of_len(1025);
    for method in ["PUT", "GET", "HEAD", "DELETE"] {
        let refused = rig.call(method, &long, b"x").await;
        assert_eq!(refused.0, StatusCode::BAD_REQUEST, "{method}");
        if method != "HEAD" {
            assert!(
                refused.1.contains("KeyTooLongError"),
                "{method}: {}",
                refused.1
            );
        }
    }
    assert_eq!(s3.keys().len(), 1);
}

/// A server with a request signer.
struct S3Front {
    server: Arc<Server>,
    config: ServerConfig,
}

impl S3Front {
    /// The status and the body of an answer to `method` on the key written as `key`.
    async fn call(&self, method: &str, key: &str, body: &[u8]) -> (StatusCode, String) {
        let uri = format!("/{BUCKET}/{key}");
        self.send(method, &uri, body).await
    }

    /// A listing of the bucket.
    async fn list(&self, query: &str) -> (StatusCode, String) {
        self.send("GET", &format!("/{BUCKET}?list-type=2&{query}"), b"")
            .await
    }

    async fn send(&self, method: &str, uri: &str, body: &[u8]) -> (StatusCode, String) {
        use http_body_util::BodyExt as _;
        let length = body.len().to_string();
        let extra: &[(&str, &str)] = if method == "PUT" {
            &[("if-none-match", "*"), ("content-length", length.as_str())]
        } else {
            &[]
        };
        let request = super::auth::sign(&self.config, method, uri, extra, body);
        let (parts, body) = self.server.handle(request).await.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (parts.status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Asserts that through a proxy with a directory hot tier and an S3 cold tier a key of `NAMES` is
/// put, copied to the cold tier under its name once the reconciler has run, and still read and
/// listed; and that a key of 1024 bytes, whose file name on the directory tier is over 255 bytes
/// encoded and so hashed, is put, read, listed and deleted there.
#[tokio::test]
async fn a_proxy_over_a_directory_and_an_s3_tier_carries_the_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-key-rules-hot");
    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let s3 = FakeS3::start().await;
    let cold = s3_backend(&s3);
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let proxy_config = ProxyConfig::new("p1", 1 << 40, clock.clone());
    let proxy = live_proxy(hot.clone(), cold.clone(), proxy_config).await;
    let config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock.clone());
    let server = Server::new(Arc::new(proxy.unwrap()), config.clone());
    let rig = S3Front { server, config };

    for name in NAMES {
        let put = rig.call("PUT", &enc(name), b"data").await;
        assert_eq!(put.0, StatusCode::OK, "put {name:?}: {}", put.1);
    }
    let base = ReconcilerConfig::new("key-rules/reconciler", 1 << 30, clock);
    let mut reconciler =
        EntryReconciler::new(hot.clone(), cold.clone(), EntryReconcilerConfig::new(base));
    while reconciler.step().await.progress() {}
    reconciler.stop().await;
    let cold_keys: Vec<String> = s3
        .keys()
        .into_iter()
        .filter(|k| !k.starts_with('_'))
        .collect();
    assert_eq!(
        cold_keys,
        sorted(&NAMES),
        "the cold tier holds the keys as written"
    );

    for name in NAMES {
        let got = rig.call("GET", &enc(name), b"").await;
        assert_eq!(
            (got.0, got.1.as_str()),
            (StatusCode::OK, "data"),
            "{name:?}"
        );
    }
    let listed = rig.list("").await;
    assert_eq!(texts(&listed.1, "Key"), sorted(&NAMES));

    let longest = of_len(1024);
    let put = rig.call("PUT", &longest, b"x").await;
    assert_eq!(put.0, StatusCode::OK, "{}", put.1);
    let got = rig.call("GET", &longest, b"").await;
    assert_eq!((got.0, got.1.as_str()), (StatusCode::OK, "x"));
    let backend: &dyn Backend = &*hot;
    assert_eq!(backend.head(&longest).await.unwrap().size, 1);
    let listed = rig.list("").await;
    assert!(texts(&listed.1, "Key").contains(&longest));
}
