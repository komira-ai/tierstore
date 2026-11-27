//! `If-Match` on a put and on the completion of an upload, through the S3 front over a cache that
//! writes through to one S3 bucket (the loopback S3 service of the tests): the bucket compares the
//! ETag and replaces in one step, the client gets its 412 and 404 as S3 gives them, and the cache
//! never serves the object that was replaced.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};
use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::dir::TestDir;

use super::testing::never_expires;
use super::{CacheConfig, LocalCache, SystemDisk};
use crate::s3::{Addressing, Capabilities, Credentials, S3Config};
use crate::server::auth::sign;
use crate::server::ops_tests::completion;
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, SECRET, texts};
use crate::server::{Server, ServerConfig};
use crate::{Backend, Error, PartBody, S3Backend, TestClock};

/// A cache in `dir` that keeps `cas/` over a bucket of `s3`, writing through as `writes` says.
fn cache_over(s3: &FakeS3, dir: &TestDir, writes: bool, clock: Arc<TestClock>) -> LocalCache {
    let policies = vec![("cas/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
    config.write_through = writes;
    never_expires(&mut config);
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
    let upstream = Arc::new(S3Backend::new(store).unwrap());
    LocalCache::open(config, upstream, Arc::new(SystemDisk), clock).unwrap()
}

/// The S3 front over a cache that keeps `cas/` and writes through to a bucket of `s3`.
struct Front {
    server: Arc<Server>,
    config: ServerConfig,
    _dir: TestDir,
}

impl Front {
    fn new(s3: &FakeS3, name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let clock = Arc::new(TestClock::new());
        clock.set(NOW);
        let cache = cache_over(s3, &dir, true, clock.clone());
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock.clone());
        config.rate_clock = clock;
        config.cache = Some(cache.clone());
        config.cache_writes = true;
        let backend: Arc<dyn Backend> = Arc::new(cache.clone());
        let server = Server::new(backend, config.clone());
        Self {
            server,
            config,
            _dir: dir,
        }
    }

    /// A call to `target` (a key, with its query) signed with the full key.
    async fn call(&self, method: &str, target: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{target}");
        let request: Request<Full<Bytes>> = sign(&self.config, method, &uri, extra, body);
        let response = self.server.handle(request).await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// A put of `body` with `If-Match: <tag>`.
    async fn swap(&self, key: &str, tag: &str, body: &[u8]) -> Reply {
        self.call("PUT", key, &[("if-match", tag)], body).await
    }

    /// The ETag a HEAD gives, with its quotes.
    async fn tag(&self, key: &str) -> String {
        let head = self.call("HEAD", key, &[], b"").await;
        assert_eq!(head.status, StatusCode::OK, "{key}");
        head.header("etag").to_owned()
    }

    /// The bytes a GET gives.
    async fn read(&self, key: &str) -> String {
        let got = self.call("GET", key, &[], b"").await;
        assert_eq!(got.status, StatusCode::OK, "{key}: {}", got.body);
        got.body
    }

    /// Starts an upload of `key` and sends `part` as its one part: the id and the part's ETag.
    async fn upload(&self, key: &str, part: &[u8]) -> (String, String) {
        let started = self.call("POST", &format!("{key}?uploads"), &[], b"").await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        let id = texts(&started.body, "UploadId").concat();
        let target = format!("{key}?partNumber=1&uploadId={id}");
        let sent = self.call("PUT", &target, &[], part).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        (id, sent.header("etag").to_owned())
    }

    /// The completion of the upload `id` of `key` from its one part, with `extra`.
    async fn complete(&self, key: &str, id: &str, part: &str, extra: &[(&str, &str)]) -> Reply {
        let body = completion(&[(1, part)]);
        let target = format!("{key}?uploadId={id}");
        self.call("POST", &target, extra, body.as_bytes()).await
    }
}

/// The status and the S3 code of an answer.
fn kind(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn precondition_failed() -> (u16, String) {
    (412, "PreconditionFailed".to_owned())
}

/// Asserts the compare-and-swap of a put, of a key the cache keeps (`cas/`) and of one it does not
/// (`other/`): a put with a stale ETag (another object's, quoted or not) is 412 and leaves the
/// bytes; a put with the current one is 200, answers the ETag of the new object, which a HEAD then
/// gives, and a GET gives the new bytes, also after the old ones were read into the cache; the
/// ETag that was current is stale after, and a put with it is 412. It catches a front that refuses
/// the replace, a cache that serves the old copy, and a compare that is not made.
#[tokio::test]
async fn a_put_with_if_match_replaces_only_the_object_with_that_etag() {
    let s3 = FakeS3::start().await;
    let front = Front::new(&s3, "tscache-swap-put");
    for key in ["cas/b/obj", "other/obj"] {
        assert_eq!(
            front.call("PUT", key, &[], b"old").await.status,
            StatusCode::OK
        );
        let old = front.tag(key).await;
        assert_eq!(front.read(key).await, "old", "{key}");

        let stale = front
            .swap(key, "\"0123456789abcdef0123456789abcdef\"", b"x")
            .await;
        assert_eq!(kind(&stale), precondition_failed(), "{key}");
        let bare = front
            .swap(key, "0123456789abcdef0123456789abcdef", b"x")
            .await;
        assert_eq!(kind(&bare), precondition_failed(), "{key}");
        assert_eq!(front.read(key).await, "old", "{key}");

        let replaced = front.swap(key, &old, b"new bytes").await;
        assert_eq!(replaced.status, StatusCode::OK, "{key}: {}", replaced.body);
        let new = replaced.header("etag").to_owned();
        assert_ne!(new, old, "{key}");
        assert_eq!(front.tag(key).await, new, "{key}");
        assert_eq!(front.read(key).await, "new bytes", "{key}");

        let again = front.swap(key, &old, b"late").await;
        assert_eq!(kind(&again), precondition_failed(), "{key}");
        assert_eq!(front.read(key).await, "new bytes", "{key}");
    }
}

/// Asserts that a put with `If-Match` of a key that holds no object is 404 `NoSuchKey`, as S3
/// answers, and creates nothing.
#[tokio::test]
async fn a_put_with_if_match_of_a_missing_key_is_404_and_creates_nothing() {
    let s3 = FakeS3::start().await;
    let front = Front::new(&s3, "tscache-swap-missing");
    let missing = front.swap("cas/b/none", "\"abc\"", b"x").await;
    assert_eq!(kind(&missing), (404, "NoSuchKey".to_owned()));
    let head = front.call("HEAD", "cas/b/none", &[], b"").await;
    assert_eq!(head.status, StatusCode::NOT_FOUND);
    assert!(s3.keys().is_empty(), "{:?}", s3.keys());
}

/// Asserts that of two puts that name the same current ETag and run at once, exactly one replaces
/// the object (200) and the other is 412, and that the object holds the bytes of the one that won.
/// Run for several rounds, each from the ETag the last winner made. It catches a compare made
/// apart from the replace (a HEAD and then a plain put), under which both writers can win.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_puts_with_one_etag_exactly_one_replaces() {
    let s3 = FakeS3::start().await;
    let front = Front::new(&s3, "tscache-swap-race");
    let key = "cas/b/raced";
    assert_eq!(
        front.call("PUT", key, &[], b"start").await.status,
        StatusCode::OK
    );
    for round in 0..8 {
        let tag = front.tag(key).await;
        let (a, b) = (format!("a{round}"), format!("b{round}"));
        let (first, second) = tokio::join!(
            front.swap(key, &tag, a.as_bytes()),
            front.swap(key, &tag, b.as_bytes())
        );
        let statuses = [first.status.as_u16(), second.status.as_u16()];
        let winner = match statuses {
            [200, 412] => a,
            [412, 200] => b,
            other => panic!("round {round}: {other:?}, {} {}", first.body, second.body),
        };
        assert_eq!(front.read(key).await, winner, "round {round}");
    }
}

/// Asserts the compare-and-swap of a completion: with a stale ETag it is 412 and the upload stays
/// open (the same completion with the current ETag then succeeds), with the current ETag it is
/// 200 and names the ETag of the new object, which a HEAD gives, and a GET gives the new bytes,
/// also after the old ones were read into the cache.
#[tokio::test]
async fn a_completion_with_if_match_replaces_only_the_object_with_that_etag() {
    let s3 = FakeS3::start().await;
    let front = Front::new(&s3, "tscache-swap-complete");
    let key = "cas/b/parts";
    assert_eq!(
        front.call("PUT", key, &[], b"old").await.status,
        StatusCode::OK
    );
    let old = front.tag(key).await;
    assert_eq!(front.read(key).await, "old");
    let (id, part) = front.upload(key, b"from parts").await;

    let stale = [("if-match", "\"0123456789abcdef0123456789abcdef\"")];
    let refused = front.complete(key, &id, &part, &stale).await;
    assert_eq!(kind(&refused), precondition_failed());
    assert_eq!(front.read(key).await, "old");

    let done = front.complete(key, &id, &part, &[("if-match", &old)]).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    let new = texts(&done.body, "ETag").concat();
    assert_ne!(new, old);
    assert_eq!(front.tag(key).await, new);
    assert_eq!(front.read(key).await, "from parts");
}

/// Asserts the 404s of a completion with `If-Match`: `NoSuchKey` where no object has the key (the
/// upload stays open), and `NoSuchUpload` for an upload that is not open, of a key that is there.
#[tokio::test]
async fn a_completion_with_if_match_tells_a_missing_key_from_a_missing_upload() {
    let s3 = FakeS3::start().await;
    let front = Front::new(&s3, "tscache-swap-complete-missing");
    let (id, part) = front.upload("cas/b/absent", b"parts").await;
    let extra = [("if-match", "\"abc\"")];
    let missing = front.complete("cas/b/absent", &id, &part, &extra).await;
    assert_eq!(kind(&missing), (404, "NoSuchKey".to_owned()));
    assert_eq!(s3.incomplete_uploads(), 1);

    assert_eq!(
        front.call("PUT", "cas/b/here", &[], b"x").await.status,
        StatusCode::OK
    );
    let tag = front.tag("cas/b/here").await;
    let extra = [("if-match", tag.as_str())];
    let gone = front
        .complete("cas/b/here", "no-such-id", &part, &extra)
        .await;
    assert_eq!(kind(&gone), (404, "NoSuchUpload".to_owned()));
    assert_eq!(front.read("cas/b/here").await, "x");
}

/// Asserts that `If-Match` beside `If-None-Match` is 501 `NotImplemented`, for a put and for a
/// completion, and changes nothing.
#[tokio::test]
async fn if_match_beside_if_none_match_is_501() {
    let s3 = FakeS3::start().await;
    let front = Front::new(&s3, "tscache-swap-both");
    let key = "cas/b/both";
    assert_eq!(
        front.call("PUT", key, &[], b"old").await.status,
        StatusCode::OK
    );
    let old = front.tag(key).await;
    let both = [("if-match", old.as_str()), ("if-none-match", "*")];
    let put = front.call("PUT", key, &both, b"new").await;
    let refused = (501, "NotImplemented".to_owned());
    assert_eq!(kind(&put), refused);
    let message = "If-Match and If-None-Match are not supported together";
    assert_eq!(texts(&put.body, "Message").concat(), message);
    let (id, part) = front.upload(key, b"parts").await;
    let completed = front.complete(key, &id, &part, &both).await;
    assert_eq!(kind(&completed), refused);
    assert_eq!(texts(&completed.body, "Message").concat(), message);
    assert_eq!(front.read(key).await, "old");
}

/// A body of one chunk.
fn once(bytes: &'static [u8]) -> PartBody<'static> {
    stream::once(async move { Ok(Bytes::from_static(bytes)) }).boxed()
}

/// Asserts that a cache that does not write through refuses a put and a completion with
/// `If-Match` as a read cache (`Unsupported`), and that one that does refuses them for a key a
/// storage node keeps for itself (`Reserved`); in each case the bucket is sent nothing and keeps
/// the object as it was.
#[tokio::test]
async fn a_read_cache_and_a_reserved_key_refuse_if_match() {
    let s3 = FakeS3::start().await;
    let dir = TestDir::under(std::env::temp_dir(), "tscache-swap-refused");
    let read_dir = TestDir::under(std::env::temp_dir(), "tscache-swap-refused-read");
    let writing = cache_over(&s3, &dir, true, Arc::new(TestClock::new()));
    writing
        .put_stream("cas/b/k", Some(3), once(b"old"))
        .await
        .unwrap();
    let tag = writing.head("cas/b/k").await.unwrap().tag;
    let reading = cache_over(&s3, &read_dir, false, Arc::new(TestClock::new()));
    let read_cache = Error::Unsupported("this tierstore is a read cache".to_owned());
    let put = reading.put_stream_if_match("cas/b/k", Some(3), once(b"new"), &tag);
    assert_eq!(put.await, Err(read_cache.clone()));
    let parts = [(1, "x".to_owned())];
    let completed = reading.complete_upload_if_match("cas/b/k", "id", &parts, &tag);
    assert_eq!(completed.await, Err(read_cache));

    let reserved = "_journal/k";
    let put = writing.put_stream_if_match(reserved, Some(3), once(b"new"), &tag);
    assert_eq!(put.await, Err(Error::Reserved(reserved.to_owned())));
    let completed = writing.complete_upload_if_match(reserved, "id", &parts, &tag);
    assert_eq!(completed.await, Err(Error::Reserved(reserved.to_owned())));
    assert_eq!(s3.keys(), ["cas/b/k".to_owned()]);
    let all = writing.get("cas/b/k", None).await.unwrap();
    assert_eq!(all, Bytes::from_static(b"old"));
}
