//! The S3 front over a cache that writes through to an S3 upstream (the loopback S3 service of the
//! tests, made to keep a PUT cut short as SeaweedFS 4.54 can): a put whose body fails leaves no
//! object of the wrong length under the key, an object there before the put stays, the digests the
//! request named go to the upstream, and `/metricz` counts the failed puts and the short objects.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper::body::{Body, Frame};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Request, StatusCode};
use md5::Md5;
use objstore_s3::ObjectKey;
use objstore_s3::s3::fake::FakeS3;
use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;
use tokio::sync::oneshot;

use super::testing::never_expires;
use super::{CacheConfig, LocalCache, SystemDisk};
use crate::s3::{Addressing, Capabilities, Credentials, S3Config, S3Tuning};
use crate::server::auth::{sign, sign_claiming};
use crate::server::request::Check;
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, SECRET};
use crate::server::{Server, ServerConfig};
use crate::{Backend, BodyDigests, S3Backend, TestClock};

/// The ten bytes the puts of these tests declare.
const TEN: &[u8] = b"0123456789";

/// A front over a cache of 64 MiB that keeps `cas/` and writes through, over a bucket of the
/// loopback S3 service, which keeps the bytes that came of a PUT cut short.
struct Node {
    s3: FakeS3,
    /// The upstream backend, kept to wait for the look after a cut PUT (a task of its own).
    backend: Arc<S3Backend>,
    server: Arc<Server>,
    config: ServerConfig,
    _dir: TestDir,
}

impl Node {
    async fn new(name: &str, head_before_put: bool) -> Self {
        let s3 = FakeS3::start().await;
        s3.keep_cut_bodies();
        let dir = TestDir::under(std::env::temp_dir(), name);
        let policies = vec![("cas/".to_owned(), true)];
        let mut cache = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
        cache.write_through = true;
        cache.head_before_put = head_before_put;
        never_expires(&mut cache);
        let clock = Arc::new(TestClock::new());
        clock.set(NOW);
        let disk = Arc::new(SystemDisk);
        let backend = s3_backend(&s3);
        let cache = LocalCache::open(cache, backend.clone(), disk, clock.clone()).unwrap();
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock.clone());
        config.rate_clock = clock;
        config.cache = Some(cache.clone());
        config.cache_writes = true;
        let front: Arc<dyn Backend> = Arc::new(cache);
        let server = Server::new(front, config.clone());
        Self {
            s3,
            backend,
            server,
            config,
            _dir: dir,
        }
    }

    /// A whole put of `body` under `key`, with the headers `extra`; the `Content-Length` an S3
    /// upstream needs is added.
    async fn put_with(&self, key: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        let len = body.len().to_string();
        let mut headers = vec![("content-length", len.as_str())];
        headers.extend_from_slice(extra);
        let request = sign(&self.config, "PUT", &uri, &headers, body);
        reply(self.server.handle(request).await).await
    }

    /// A whole put of `body` under `key`.
    async fn put(&self, key: &str, body: &[u8]) -> Reply {
        self.put_with(key, &[], body).await
    }

    /// A put of `key` with the headers `extra` (its `Content-Length` of ten bytes among them)
    /// whose client goes away before the first byte of the body, once the upstream has read the
    /// head of the PUT the cache sends it (the request after the `before` ones the service had
    /// begun). The HEADs of the upstream after the next `heads` are held until the upstream has
    /// taken the cut PUT, so that the look after it sees what the upstream kept.
    async fn cut(&self, key: &str, extra: &[(&str, &str)], heads: u32) -> Reply {
        let before = self.s3.requests_begun();
        self.s3.hold_heads(heads);
        let cuts = self.s3.cut_requests();
        let uri = format!("/{BUCKET}/{key}");
        let who = (KEY_ID, SECRET);
        let claim = "UNSIGNED-PAYLOAD";
        let signed = sign_claiming(&self.config, who, "PUT", &uri, extra, claim, b"");
        let (gone, body) = oneshot::channel();
        let request = Request::from_parts(signed.into_parts().0, Gone(body));
        let server = Arc::clone(&self.server);
        let answer = tokio::spawn(async move { reply(server.handle(request).await).await });
        let begun = before + 1 + u64::from(heads);
        self.s3.wait_for(|s| s.requests_begun() == begun).await;
        gone.send(()).unwrap();
        self.s3.wait_for(|s| s.cut_requests() == cuts + 1).await;
        self.s3.release_heads();
        let answered = answer.await.unwrap();
        self.backend.cleanups_idle().await;
        answered
    }

    /// The sample of `series` on `/metricz`.
    fn metric(&self, series: &str) -> u64 {
        let text = self.server.metrics_probe();
        let line = text.lines().find(|line| {
            line.strip_prefix(series)
                .is_some_and(|rest| rest.starts_with(' '))
        });
        let line = line.unwrap_or_else(|| panic!("no {series} in\n{text}"));
        line.rsplit(' ').next().unwrap().parse().unwrap()
    }

    /// The counts of `tierstore_cache_upstream_short_objects_total`: removed, left, failed.
    fn short_objects(&self) -> [u64; 3] {
        let of = |result: &str| {
            let series =
                format!("tierstore_cache_upstream_short_objects_total{{result=\"{result}\"}}");
            self.metric(&series)
        };
        [of("removed"), of("left"), of("failed")]
    }

    /// The count of `tierstore_s3_put_failures_total` of `reason`.
    fn put_failures(&self, reason: &str) -> u64 {
        self.metric(&format!(
            "tierstore_s3_put_failures_total{{reason=\"{reason}\"}}"
        ))
    }

    fn object_len(&self, key: &str) -> Option<u64> {
        self.s3.object_len(&ObjectKey::new(key).unwrap())
    }
}

/// A backend over the bucket of the loopback S3 service that looks at the key at once after a PUT
/// that got no answer: the tests order the look after the cut PUT with held HEADs.
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
        cut_put_look_after: Duration::ZERO,
        ..S3Tuning::default()
    };
    Arc::new(S3Backend::new_tuned(store, tuning).unwrap())
}

async fn reply<B>(response: hyper::Response<B>) -> Reply
where
    B: Body,
    B::Error: std::fmt::Debug,
{
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A body that gives no byte and fails once the test sends on its channel: a client that went
/// away before the first byte of its body, at the moment the test says.
struct Gone(oneshot::Receiver<()>);

impl Body for Gone {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(Some(Err(std::io::Error::other("the client went away")))),
        }
    }
}

/// Asserts the case of komira-ai/tierstore#591: a create-only put through the front whose client
/// goes away before the first byte, over an upstream that keeps such a PUT as an object of 0
/// bytes, is answered `IncompleteBody` and leaves no object under the key; `/metricz` counts one
/// short object removed and one failed put of reason `incomplete`. The HEAD the cache sends
/// before the put is let through.
/// Catches: a failed put that leaves the 0-byte object a create-only writer is refused by for ever.
#[tokio::test]
async fn a_put_cut_before_its_first_byte_leaves_no_object_upstream() {
    let node = Node::new("short-cut-empty", true).await;
    let extra = [("content-length", "10"), ("if-none-match", "*")];
    let cut = node.cut("cas/b/cut", &extra, 1).await;
    assert_eq!(
        (cut.status, cut.code()),
        (StatusCode::BAD_REQUEST, "IncompleteBody".to_owned())
    );
    assert_eq!(node.object_len("cas/b/cut"), None);
    assert_eq!(node.short_objects(), [1, 0, 0]);
    assert_eq!(node.put_failures("incomplete"), 1);

    let made = node.put("cas/b/cut", TEN).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    assert_eq!(node.object_len("cas/b/cut"), Some(10));
}

/// Asserts that a create-only put cut short over a key that already held a whole object (the
/// upstream refuses the PUT; the cache sends no HEAD before it) leaves that object as it was, and
/// counts it as left, not removed.
/// Catches: a look that deletes an object that was there before the put began.
#[tokio::test]
async fn a_put_cut_over_a_whole_object_leaves_it() {
    let node = Node::new("short-cut-held", false).await;
    let made = node.put("cas/b/held", b"eight by").await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let extra = [("content-length", "10"), ("if-none-match", "*")];
    let cut = node.cut("cas/b/held", &extra, 0).await;
    assert_eq!(cut.status, StatusCode::BAD_REQUEST);
    assert_eq!(node.object_len("cas/b/held"), Some(8));
    assert_eq!(node.short_objects(), [0, 1, 0]);
    let deletes = node
        .s3
        .requests()
        .iter()
        .filter(|r| r.starts_with("DELETE "))
        .count();
    assert_eq!(deletes, 0);
}

/// Asserts that a plain put (no `If-None-Match`) cut short, which the upstream keeps as 0 bytes
/// over the object the key held, is removed: the object left is the put's own.
#[tokio::test]
async fn a_plain_put_cut_short_removes_the_object_it_left() {
    let node = Node::new("short-cut-plain", true).await;
    let made = node.put("ac/plain", b"old").await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let cut = node.cut("ac/plain", &[("content-length", "10")], 0).await;
    assert_eq!(cut.status, StatusCode::BAD_REQUEST);
    assert_eq!(node.object_len("ac/plain"), None);
    assert_eq!(node.short_objects(), [1, 0, 0]);
}

/// Asserts that the digests a put names in `Content-MD5` and `x-amz-checksum-sha256` go to the
/// upstream with its PUT: a put cut short is refused by the upstream (nothing kept, nothing to
/// remove), and a whole put is kept with the SHA-256 as its checksum.
/// Catches: a front that does not pass the client's digests on, so the store keeps a body cut short.
#[tokio::test]
async fn the_digests_of_a_put_go_to_the_upstream() {
    let node = Node::new("short-cut-digests", true).await;
    let md5 = STANDARD.encode(Md5::digest(TEN));
    let sha256: [u8; 32] = Sha256::digest(TEN).into();
    let sum = STANDARD.encode(sha256);
    let extra = [
        ("content-length", "10"),
        ("content-md5", md5.as_str()),
        ("x-amz-checksum-sha256", sum.as_str()),
    ];
    let cut = node.cut("cas/b/named", &extra, 0).await;
    assert_eq!(cut.status, StatusCode::BAD_REQUEST);
    assert_eq!(node.object_len("cas/b/named"), None);
    assert_eq!(node.short_objects(), [0, 0, 0]);

    let named = [
        ("content-md5", md5.as_str()),
        ("x-amz-checksum-sha256", sum.as_str()),
    ];
    let made = node.put_with("cas/b/named", &named, TEN).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let store = node.s3.store(Capabilities::default());
    let held = store
        .head_sha256(&ObjectKey::new("cas/b/named").unwrap())
        .await;
    assert_eq!(held.unwrap().checksum, Some(sha256));
}

/// Asserts the reasons the front counts a failed streamed put under: a body that stops coming is
/// `timeout`, one cut `incomplete`, a refused digest `bad_digest`, a body over the limit
/// `too_large`, a server taking all it may `busy`, a key that exists `exists`, a store that failed
/// `upstream`, and any other refusal `other`.
#[test]
fn a_failed_put_counts_under_the_reason_of_its_answer() {
    use crate::server::front_counts::PutFailure;
    let cases = [
        ("RequestTimeout", PutFailure::Timeout),
        ("IncompleteBody", PutFailure::Incomplete),
        ("BadDigest", PutFailure::BadDigest),
        ("XAmzContentSHA256Mismatch", PutFailure::BadDigest),
        ("InvalidDigest", PutFailure::BadDigest),
        ("EntityTooLarge", PutFailure::TooLarge),
        ("SlowDown", PutFailure::Busy),
        ("PreconditionFailed", PutFailure::Exists),
        ("ServiceUnavailable", PutFailure::Upstream),
        ("InternalError", PutFailure::Upstream),
        ("MissingContentLength", PutFailure::Other),
    ];
    for (code, why) in cases {
        assert_eq!(PutFailure::of(code), why, "{code}");
    }
}

/// Asserts which digests a request names for its body: the MD5 of `Content-MD5`; the SHA-256 the
/// signature covers, or of `x-amz-checksum-sha256` for an unsigned body; none for an unsigned body
/// with no checksum header, nor for a payload hash that is not 32 bytes.
#[test]
fn a_request_names_the_digests_it_claims() {
    let md5: [u8; 16] = Md5::digest(TEN).into();
    let sha256: [u8; 32] = Sha256::digest(TEN).into();
    let headers = |pairs: &[(&'static str, String)]| {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    };
    let digests = |claim: &str, map: &HeaderMap| Check::new(claim, map, true).unwrap().digests();
    let none = headers(&[]);
    assert_eq!(digests("UNSIGNED-PAYLOAD", &none), BodyDigests::default());
    let signed = digests(&hex::encode(sha256), &none);
    assert_eq!(
        signed,
        BodyDigests {
            md5: None,
            sha256: Some(sha256)
        }
    );
    assert_eq!(digests("abcd", &none), BodyDigests::default());
    let both = headers(&[
        ("content-md5", STANDARD.encode(md5)),
        ("x-amz-checksum-sha256", STANDARD.encode(sha256)),
    ]);
    let named = digests("UNSIGNED-PAYLOAD", &both);
    assert_eq!(
        named,
        BodyDigests {
            md5: Some(md5),
            sha256: Some(sha256)
        }
    );
}
