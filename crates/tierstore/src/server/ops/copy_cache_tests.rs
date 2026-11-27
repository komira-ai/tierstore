//! CopyObject and UploadPartCopy through the deployment they are for: an S3 front over a cache
//! that writes through (`write_through = true`, keeping what is under `cas/`) over one S3 store
//! (the loopback service of the tests). What the store copied, what the front read and put, and
//! what the cache holds afterwards are told from the requests the service saw and from the
//! cache's own count of what it holds.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};
use objstore_s3::s3::fake::{Failure, FakeS3};
use tierstore_testkit::dir::TestDir;

use crate::s3::{Capabilities, S3Tuning};
use crate::server::auth::sign;
use crate::server::local_cache::testing::{Rig, never_expires, until};
use crate::server::local_cache::{CacheConfig, LocalCache, SystemDisk};
use crate::server::ops_tests::completion;
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, SECRET, texts};
use crate::server::{Server, ServerConfig};
use crate::{Backend, S3Backend, TestClock};

/// An S3 front over a cache, which the tests send signed requests.
struct Front {
    server: Arc<Server>,
    config: ServerConfig,
}

impl Front {
    /// The front over `cache`, which it is told writes through, as the binary does.
    fn over(cache: &LocalCache) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(NOW);
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock.clone());
        config.rate_clock = clock;
        config.cache = Some(cache.clone());
        config.cache_writes = true;
        let backend: Arc<dyn Backend> = Arc::new(cache.clone());
        let server = Server::new(backend, config.clone());
        Self { server, config }
    }

    /// A call signed with the full key; `key` may carry a query.
    async fn call(&self, method: &str, key: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
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

    /// A plain put of `body` (its length said), which must be taken.
    async fn put(&self, key: &str, body: &[u8]) {
        let len = body.len().to_string();
        let told = [("content-length", len.as_str())];
        let reply = self.call("PUT", key, &told, body).await;
        assert_eq!(reply.status, StatusCode::OK, "{key}: {}", reply.body);
    }

    /// A copy of `src` into `dst` (a key, with a query for a part), with the headers `extra`.
    async fn copy(&self, dst: &str, src: &str, extra: &[(&str, &str)]) -> Reply {
        let source = format!("/{BUCKET}/{src}");
        let mut headers = vec![("x-amz-copy-source", source.as_str())];
        headers.extend_from_slice(extra);
        self.call("PUT", dst, &headers, b"").await
    }

    /// The body of a read of `key`, which must be taken.
    async fn read(&self, key: &str) -> String {
        let reply = self.call("GET", key, &[], b"").await;
        assert_eq!(reply.status, StatusCode::OK, "{key}: {}", reply.body);
        reply.body
    }

    /// The entity tag a HEAD of `key` gives, without quotes.
    async fn tag(&self, key: &str) -> String {
        let reply = self.call("HEAD", key, &[], b"").await;
        assert_eq!(reply.status, StatusCode::OK, "{key}");
        reply.header("etag").trim_matches('"').to_owned()
    }

    /// The start of a multipart upload of `key`: its id.
    async fn begin(&self, key: &str) -> String {
        let started = self.call("POST", &format!("{key}?uploads"), &[], b"").await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        texts(&started.body, "UploadId").concat()
    }
}

/// The status and the S3 error code of a refused call.
fn refused(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

/// The tag in the answer to a copy, without quotes.
fn answered_tag(reply: &Reply) -> String {
    texts(&reply.body, "ETag")
        .concat()
        .trim_matches('"')
        .to_owned()
}

/// The front, the cache and the store of the deployment, and the store as the cache has it, to
/// change what the store holds without the cache knowing.
struct Deploy {
    s3: FakeS3,
    upstream: Arc<S3Backend>,
    cache: LocalCache,
    front: Front,
    _dir: TestDir,
}

impl Deploy {
    /// The deployment with a store of the settings `tuning`.
    async fn start(name: &str, tuning: S3Tuning) -> Self {
        let s3 = FakeS3::start().await;
        let store = s3.config(Capabilities::default());
        let upstream = Arc::new(S3Backend::new_tuned(store, tuning).unwrap());
        let dir = TestDir::under(std::env::temp_dir(), name);
        let policies = vec![("cas/".to_owned(), true)];
        let mut config = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
        config.write_through = true;
        never_expires(&mut config);
        let clock = Arc::new(TestClock::new());
        let opened = LocalCache::open(config, upstream.clone(), Arc::new(SystemDisk), clock);
        let cache = opened.unwrap();
        let front = Front::over(&cache);
        Self {
            s3,
            upstream,
            cache,
            front,
            _dir: dir,
        }
    }

    /// The deployment with the settings a store has by default.
    async fn new(name: &str) -> Self {
        Self::start(name, S3Tuning::default()).await
    }

    /// How many requests the store has had so far.
    fn mark(&self) -> usize {
        self.s3.requests().len()
    }

    /// The requests of the store since `mark`.
    fn since(&self, mark: usize) -> Vec<String> {
        self.s3.requests()[mark..].to_vec()
    }

    /// Replaces the object the store holds under `key` with `body`, behind the cache's back.
    async fn replace_at_store(&self, key: &str, body: &'static [u8]) {
        self.upstream.delete(key).await.unwrap();
        self.upstream
            .put(key, Bytes::from_static(body))
            .await
            .unwrap();
    }

    /// Waits until the cache holds `bytes` bytes in `objects` objects.
    async fn holds(&self, bytes: u64, objects: usize) {
        let what = format!("the cache holds {objects} objects of {bytes} bytes");
        until(&what, || self.cache.held() == (bytes, objects)).await;
    }
}

/// How many of the requests are `method` of the key `key` of the store.
fn count(lines: &[String], method: &str, key: &str) -> usize {
    let start = format!("{method} /tierstore-fake/{key}?");
    lines.iter().filter(|line| line.starts_with(&start)).count()
}

/// `len` bytes of text that differ from one place to the next.
fn text(len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % 26) as u8).collect()
}

/// Asserts that a copy onto a key that exists replaces it: 200 with a `CopyObjectResult` whose tag
/// is the source's, a read of the destination gives the bytes of the source, and the copy was made
/// by the store: during it the store had no GET of the source, one PUT of the destination, and
/// took no body (no single put, no part).
#[tokio::test]
async fn a_copy_onto_an_existing_key_replaces_it_and_the_store_makes_it() {
    let d = Deploy::new("tscache-copy-replace").await;
    d.front.put("a", b"the bytes of a").await;
    d.front.put("b", b"what b was").await;
    let tag_a = d.front.tag("a").await;
    let (mark, stats) = (d.mark(), d.s3.stats());

    let copied = d.front.copy("b", "a", &[]).await;

    assert_eq!(copied.status, StatusCode::OK, "{}", copied.body);
    assert!(copied.body.contains("<CopyObjectResult"), "{}", copied.body);
    assert_eq!(answered_tag(&copied), tag_a);
    assert!(!texts(&copied.body, "LastModified").concat().is_empty());
    let seen = d.since(mark);
    assert_eq!(count(&seen, "GET", "a"), 0, "{seen:?}");
    assert_eq!(count(&seen, "PUT", "b"), 1, "{seen:?}");
    assert_eq!(d.s3.stats(), stats);
    assert_eq!(d.front.read("b").await, "the bytes of a");
    assert_eq!(d.front.tag("b").await, tag_a);
    assert_eq!(d.front.read("a").await, "the bytes of a");
}

/// Asserts that after a copy the store made into a key under `cas/` the cache holds nothing of
/// the key (it held the old object, which the copy replaced), that the first read of the key
/// fills the cache with the copy's bytes (one GET of the store), and that a second read is served
/// from the cache: the store gets no other GET.
#[tokio::test]
async fn a_store_copy_into_a_kept_key_leaves_it_out_of_the_cache_and_the_first_read_fills_it() {
    let d = Deploy::new("tscache-copy-kept").await;
    d.front.put("plain/src", b"the copied bytes").await;
    d.front.put("cas/dst", b"old").await;
    d.holds(3, 1).await;

    let copied = d.front.copy("cas/dst", "plain/src", &[]).await;
    assert_eq!(copied.status, StatusCode::OK, "{}", copied.body);
    assert_eq!(d.cache.held(), (0, 0));

    let mark = d.mark();
    assert_eq!(d.front.read("cas/dst").await, "the copied bytes");
    d.holds(16, 1).await;
    let first = count(&d.since(mark), "GET", "cas/dst");
    assert!(first >= 1, "the first read filled nothing from the store");

    assert_eq!(d.front.read("cas/dst").await, "the copied bytes");
    assert_eq!(count(&d.since(mark), "GET", "cas/dst"), first);
}

/// Asserts that a create-only copy (`If-None-Match: *`) is not the store's: the front reads the
/// source and puts it, so the store has a GET of the source and a single put of the destination;
/// and that the cache holds the destination afterwards when the key is under `cas/` and holds
/// nothing of it when the key is not.
#[tokio::test]
async fn a_create_only_copy_is_streamed_through_the_cache_and_kept_under_a_kept_prefix() {
    let d = Deploy::new("tscache-copy-create").await;
    d.front.put("plain/src", b"the copied bytes").await;
    let (mark, stats) = (d.mark(), d.s3.stats());

    let kept = d
        .front
        .copy("cas/new", "plain/src", &[("if-none-match", "*")])
        .await;
    assert_eq!(kept.status, StatusCode::OK, "{}", kept.body);
    d.holds(16, 1).await;
    assert!(count(&d.since(mark), "GET", "plain/src") >= 1);
    assert_eq!(d.s3.stats().single_puts, stats.single_puts + 1);

    let other = d
        .front
        .copy("plain/new", "plain/src", &[("if-none-match", "*")])
        .await;
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    assert_eq!(d.cache.held(), (16, 1));
    assert_eq!(d.s3.stats().single_puts, stats.single_puts + 2);
    assert_eq!(d.front.read("plain/new").await, "the copied bytes");
    assert_eq!(answered_tag(&kept), d.front.tag("cas/new").await);
}

/// Asserts that a source condition is sent to the store, which tests it against the object it
/// copies: the cache holds the source (tag `T1`) and the store has another object under its key
/// since, so the front's own check against the cache's HEAD passes `If-Match: T1` and the store
/// refuses it, 412 `PreconditionFailed`, and nothing is written; the source in the store stays.
#[tokio::test]
async fn a_source_condition_that_holds_of_the_cache_but_not_of_the_store_is_412() {
    let d = Deploy::new("tscache-copy-condition").await;
    d.front.put("cas/src", b"the first bytes").await;
    d.holds(15, 1).await;
    let tag = d.front.tag("cas/src").await;
    d.replace_at_store("cas/src", b"other bytes").await;
    assert_eq!(
        d.front.tag("cas/src").await,
        tag,
        "the cache's HEAD is not stale"
    );
    let said = format!("\"{tag}\"");

    let copied = d
        .front
        .copy(
            "cas/dst",
            "cas/src",
            &[("x-amz-copy-source-if-match", said.as_str())],
        )
        .await;

    assert_eq!(refused(&copied), (412, "PreconditionFailed".to_owned()));
    assert_eq!(d.s3.keys(), ["cas/src"]);
    assert_eq!(
        d.upstream.get("cas/src", None).await.unwrap(),
        "other bytes"
    );
}

/// Asserts that a source the store does not hold, though the cache's HEAD still says it does,
/// is 404 `NoSuchKey` from the front, for the copy of an object and for the copy of a part, and
/// that nothing is written.
#[tokio::test]
async fn a_source_the_store_lost_is_404_no_such_key_for_an_object_and_for_a_part() {
    let d = Deploy::new("tscache-copy-lost").await;
    d.front.put("cas/gone", b"it was here").await;
    d.holds(11, 1).await;
    d.upstream.delete("cas/gone").await.unwrap();
    assert!(
        d.front.tag("cas/gone").await.len() > 1,
        "the cache's HEAD is not stale"
    );

    let object = d.front.copy("cas/dst", "cas/gone", &[]).await;
    assert_eq!(refused(&object), (404, "NoSuchKey".to_owned()));
    assert_eq!(d.s3.keys(), Vec::<String>::new());

    let id = d.front.begin("multi").await;
    let target = format!("multi?partNumber=1&uploadId={id}");
    let part = d.front.copy(&target, "cas/gone", &[]).await;
    assert_eq!(refused(&part), (404, "NoSuchKey".to_owned()));
    assert_eq!(d.s3.stats().parts, 0);
}

/// Asserts UploadPartCopy by the store: a part from a range of a source and a part from the whole
/// of it are each answered with a `CopyPartResult` and a tag, the completion with those tags makes
/// an object of the bytes in the order of the parts, a read gives them, and the store had no GET
/// of the source while the parts were copied.
#[tokio::test]
async fn parts_copied_from_ranges_of_a_source_make_the_object_the_parts_say() {
    let d = Deploy::new("tscache-copy-parts").await;
    d.s3.set_min_part_bytes(10);
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    d.front.put("src", alphabet.as_bytes()).await;
    let id = d.front.begin("multi").await;
    let mark = d.mark();

    let first = format!("multi?partNumber=1&uploadId={id}");
    let ranged = [("x-amz-copy-source-range", "bytes=0-9")];
    let one = d.front.copy(&first, "src", &ranged).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert!(one.body.contains("<CopyPartResult"), "{}", one.body);
    let second = format!("multi?partNumber=2&uploadId={id}");
    let two = d.front.copy(&second, "src", &[]).await;
    assert_eq!(two.status, StatusCode::OK, "{}", two.body);
    assert_eq!(count(&d.since(mark), "GET", "src"), 0);
    assert_eq!(d.s3.stats().parts, 2);

    let (t1, t2) = (
        texts(&one.body, "ETag").concat(),
        texts(&two.body, "ETag").concat(),
    );
    let done = completion(&[(1, t1.as_str()), (2, t2.as_str())]);
    let ended = d
        .front
        .call(
            "POST",
            &format!("multi?uploadId={id}"),
            &[],
            done.as_bytes(),
        )
        .await;
    assert_eq!(ended.status, StatusCode::OK, "{}", ended.body);

    let expected = format!("ABCDEFGHIJ{alphabet}");
    assert_eq!(
        d.upstream.get("multi", None).await.unwrap(),
        expected.as_bytes()
    );
    assert_eq!(d.front.read("multi").await, expected);
    assert_eq!(d.s3.incomplete_uploads(), 0);
}

/// Asserts that a copy of a part into an upload the store does not hold is 404 `NoSuchUpload`.
#[tokio::test]
async fn a_part_copied_into_an_upload_the_store_does_not_hold_is_404_no_such_upload() {
    let d = Deploy::new("tscache-copy-no-upload").await;
    d.front.put("src", b"some bytes").await;

    let target = "multi?partNumber=1&uploadId=no-such-upload";
    let part = d.front.copy(target, "src", &[]).await;

    assert_eq!(refused(&part), (404, "NoSuchUpload".to_owned()));
    assert_eq!(d.s3.stats().parts, 0);
}

/// Asserts that a source longer than the store's `single_put_max` is copied by a read and a put,
/// not by the store: the store has a GET of the source and the destination is written as a
/// multipart upload (no single put), the destination holds the source's bytes and the answer
/// names the tag a HEAD of it gives.
#[tokio::test]
async fn a_source_over_the_single_put_limit_is_copied_by_a_read_and_a_put() {
    let tuning = S3Tuning {
        single_put_max: 1000,
        ..S3Tuning::default()
    };
    let d = Deploy::start("tscache-copy-large", tuning).await;
    let bytes = text(3000);
    d.front.put("src", &bytes).await;
    let (mark, stats) = (d.mark(), d.s3.stats());

    let copied = d.front.copy("dst", "src", &[]).await;

    assert_eq!(copied.status, StatusCode::OK, "{}", copied.body);
    let seen = d.since(mark);
    assert!(count(&seen, "GET", "src") >= 1, "{seen:?}");
    let after = d.s3.stats();
    assert_eq!(after.completed, stats.completed + 1);
    assert_eq!(after.single_puts, stats.single_puts);
    assert_eq!(d.upstream.get("dst", None).await.unwrap(), bytes);
    assert_eq!(answered_tag(&copied), d.front.tag("dst").await);
}

/// Asserts that a store that answers a copy 501 (it does not copy) does not fail the copy: the
/// front reads the source and puts it (a GET of the source and a single put of the destination),
/// the destination holds the source's bytes, and the next copy, which the store takes, is made by
/// the store again.
#[tokio::test]
async fn a_store_that_does_not_copy_is_copied_from_by_a_read_and_a_put() {
    let d = Deploy::new("tscache-copy-unsupported").await;
    d.front.put("src", b"the source bytes").await;
    d.s3.fail(Failure::CopyUnsupported);
    let (mark, stats) = (d.mark(), d.s3.stats());

    let copied = d.front.copy("dst", "src", &[]).await;

    assert_eq!(copied.status, StatusCode::OK, "{}", copied.body);
    assert!(count(&d.since(mark), "GET", "src") >= 1);
    assert_eq!(d.s3.stats().single_puts, stats.single_puts + 1);
    assert_eq!(d.front.read("dst").await, "the source bytes");
    assert_eq!(answered_tag(&copied), d.front.tag("dst").await);

    let (mark, stats) = (d.mark(), d.s3.stats());
    let again = d.front.copy("dst2", "src", &[]).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(count(&d.since(mark), "GET", "src"), 0);
    assert_eq!(d.s3.stats(), stats);
    assert_eq!(d.front.read("dst2").await, "the source bytes");
}

/// Asserts that a create-only copy onto a key that exists is 412 `PreconditionFailed` before the
/// source is read, for a key the cache keeps and one it does not: the store has no GET of the
/// source, no put is made, and the destination holds what it held.
#[tokio::test]
async fn a_refused_create_only_copy_reads_nothing_of_the_source() {
    let d = Deploy::new("tscache-copy-refused").await;
    d.front.put("plain/src", b"the source bytes").await;
    for key in ["cas/dst", "plain/dst"] {
        d.front.put(key, b"already here").await;
    }
    let (mark, stats) = (d.mark(), d.s3.stats());

    for key in ["cas/dst", "plain/dst"] {
        let copied = d
            .front
            .copy(key, "plain/src", &[("if-none-match", "*")])
            .await;
        assert_eq!(
            refused(&copied),
            (412, "PreconditionFailed".to_owned()),
            "{key}"
        );
        assert_eq!(d.front.read(key).await, "already here", "{key}");
    }

    let seen = d.since(mark);
    assert_eq!(count(&seen, "GET", "plain/src"), 0, "{seen:?}");
    assert_eq!(d.s3.stats(), stats);
}

/// Asserts that a copy of an object that is overwritten while it is copied is one whole version:
/// the read of the source has its bytes when the source is replaced by another object of another
/// length, and the copy answers 200 and writes exactly the bytes that were read (not a mix, and
/// not the new object), with the tag of those bytes.
#[tokio::test]
async fn a_source_overwritten_during_the_copy_is_copied_as_the_one_version_that_was_read() {
    let rig = Rig::writing("tscache-copy-overwritten");
    let front = Front::over(&rig.cache);
    rig.put("other/src", b"the old bytes").await;
    rig.put("other/reference", b"the old bytes").await;
    let gate = rig.tamper.hold_key_after_read("other/src");

    let (copied, ()) = tokio::join!(front.copy("other/dst", "other/src", &[]), async {
        until("the read of the source has its bytes", || {
            rig.tamper.parked() == 1
        })
        .await;
        rig.replace("other/src", b"the new bytes, which are longer")
            .await;
        gate.add_permits(10);
    });

    assert_eq!(copied.status, StatusCode::OK, "{}", copied.body);
    assert_eq!(front.read("other/dst").await, "the old bytes");
    let tag = front.tag("other/dst").await;
    assert_eq!(answered_tag(&copied), tag);
    assert_eq!(tag, front.tag("other/reference").await);
    assert_eq!(
        front.read("other/src").await,
        "the new bytes, which are longer"
    );
}
