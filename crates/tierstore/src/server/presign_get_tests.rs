//! What the server does with a presigned GET URL, through `Server::handle`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use tokio::task::JoinSet;

use crate::{Backend, TestClock};

use super::presign::tests::{NOW, request};
use super::presign::{PresignRequest, presign_get, presign_url};
use super::testing::{KEY_ID, Reply, Rig, SECRET};
use super::{AccessKey, GcKey, Prefixes, ScopedKey, Server};

const MEBIBYTE: u64 = 1 << 20;

const FULL: (&str, &str) = (KEY_ID, SECRET);
const READER: (&str, &str) = ("AKIDREADER", "reader-secret");
/// Reads `cas/` and nothing else.
const SCOPED: (&str, &str) = ("scoped-reader", "scoped-secret-0123456789");
const GC: (&str, &str) = ("pack-gc", "gc-secret-0123456789");

/// A server whose clock is set by hand, with a read-only key, a scoped key that reads `cas/`
/// and a GC key, and URLs of at most ten minutes.
struct Fixture {
    rig: Rig,
    clock: Arc<TestClock>,
}

async fn fixture() -> Fixture {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let shared = Arc::clone(&clock);
    let rig = Rig::with(128 * MEBIBYTE, move |config| {
        config.clock = shared;
        config.read_only_keys = vec![AccessKey::new(READER.0, READER.1)];
        let reads = Prefixes {
            read: &["cas/"],
            write: &[],
            delete: &[],
        };
        config.scoped_keys = vec![ScopedKey::new(SCOPED.0, SCOPED.1, &reads).unwrap()];
        config.gc_keys = vec![GcKey::new(GC.0, GC.1, "cas/").unwrap()];
        config.presign_max_expires = Duration::from_secs(600);
    })
    .await;
    Fixture { rig, clock }
}

/// The URL of a GET of `object`, good for `secs`, signed with `key`.
fn get_url(key: (&str, &str), object: &str, secs: u64) -> String {
    let key = AccessKey::new(key.0, key.1);
    presign_get(&key, &request(object, secs)).unwrap()
}

/// The request for a URL, with a `Range` header if there is one.
fn request_for(method: &str, url: &str, range: Option<&str>) -> Request<Full<Bytes>> {
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    let mut builder = Request::builder()
        .method(method)
        .uri(&rest[at..])
        .header("host", &rest[..at]);
    if let Some(range) = range {
        builder = builder.header("range", range);
    }
    builder.body(Full::new(Bytes::new())).unwrap()
}

fn refused(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn access_denied() -> (u16, String) {
    (403, "AccessDenied".to_owned())
}

/// `len` bytes that depend on `seed` and on the place, so that a part in the wrong place, or of
/// another object, is not the bytes expected.
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    let byte = |i: usize| u8::try_from((i * 31 + seed * 7 + i / 251) % 256).unwrap();
    (0..len).map(byte).collect()
}

/// What a GET of `url` gave: the status and the body, as a task of its own would read it.
async fn fetch(server: Arc<Server>, url: String, range: Option<String>) -> (StatusCode, Vec<u8>) {
    let response = server
        .handle(request_for("GET", &url, range.as_deref()))
        .await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("a whole body").to_bytes();
    (parts.status, bytes.to_vec())
}

impl Fixture {
    async fn send(&self, method: &str, url: &str, range: Option<&str>) -> Reply {
        self.rig.send_raw(request_for(method, url, range)).await
    }

    async fn put(&self, key: &str, body: &[u8]) {
        assert_eq!(
            self.rig.put(key, body).await.status,
            StatusCode::OK,
            "{key}"
        );
    }
}

/// Asserts that a presigned GET URL of each key that reads (the full key, a read-only key, a
/// scoped key that reads the prefix) serves the object whole, serves a range of it as 206 with
/// exactly the bytes of the range, and answers HEAD (the URL signed for HEAD) with the size; and
/// that the `x-id=GetObject` parameter that the AWS SDKs add is accepted. The signature does not
/// cover the `Range` header, so one URL serves every range.
#[tokio::test(start_paused = true)]
async fn a_presigned_get_serves_whole_ranged_and_head_for_each_key_that_reads() {
    let f = fixture().await;
    f.put("cas/a", b"0123456789").await;
    for key in [FULL, READER, SCOPED] {
        let url = get_url(key, "cas/a", 300);
        let whole = f.send("GET", &url, None).await;
        assert_eq!(whole.status, StatusCode::OK, "{}", key.0);
        assert_eq!(whole.body, "0123456789", "{}", key.0);
        let part = f.send("GET", &url, Some("bytes=2-5")).await;
        assert_eq!(part.status, StatusCode::PARTIAL_CONTENT, "{}", key.0);
        assert_eq!(part.body, "2345", "{}", key.0);
        assert_eq!(part.header("content-range"), "bytes 2-5/10");
        let head = AccessKey::new(key.0, key.1);
        let head = presign_url("HEAD", &head, &request("cas/a", 300), &[]).unwrap();
        let head = f.send("HEAD", &head, None).await;
        assert_eq!(head.status, StatusCode::OK, "{}", key.0);
        assert_eq!(head.header("content-length"), "10", "{}", key.0);
        let key = AccessKey::new(key.0, key.1);
        let sdk = presign_url(
            "GET",
            &key,
            &request("cas/a", 300),
            &[("x-id", "GetObject")],
        );
        let sdk = f.send("GET", &sdk.unwrap(), None).await;
        assert_eq!(sdk.body, "0123456789");
    }
}

/// Asserts that a presigned GET URL has the rights of the key that signed it and no more: a scoped
/// key's URL for an object outside its read prefixes is 403 `AccessDenied` and shows nothing of the
/// object, while the full and the read-only key's URLs for it are served; a GC key, an unknown
/// key id and a key id in another region sign no URL (`InvalidAccessKeyId`); and a URL signed
/// with the wrong secret is `SignatureDoesNotMatch`.
#[tokio::test(start_paused = true)]
async fn a_presigned_get_has_the_rights_of_its_key_and_no_more() {
    let f = fixture().await;
    f.put("cas/a", b"CONTENT-OF-A").await;
    f.put("other/b", b"CONTENT-OF-B").await;
    for key in [FULL, READER] {
        let reply = f.send("GET", &get_url(key, "other/b", 300), None).await;
        assert_eq!(reply.body, "CONTENT-OF-B", "{}", key.0);
    }
    let outside = f.send("GET", &get_url(SCOPED, "other/b", 300), None).await;
    assert_eq!(refused(&outside), access_denied());
    assert!(!outside.body.contains("CONTENT-OF-B"));
    let inside = f.send("GET", &get_url(SCOPED, "cas/a", 300), None).await;
    assert_eq!(inside.body, "CONTENT-OF-A");
    for key in [GC, ("not-held", "any-secret-0123456789")] {
        let reply = f.send("GET", &get_url(key, "cas/a", 300), None).await;
        let wanted = (403, "InvalidAccessKeyId".to_owned());
        assert_eq!(refused(&reply), wanted, "{}", key.0);
        assert!(!reply.body.contains("CONTENT-OF-A"));
    }
    let west = PresignRequest {
        region: "eu-west-1",
        ..request("cas/a", 300)
    };
    let west = presign_get(&AccessKey::new(FULL.0, FULL.1), &west).unwrap();
    let reply = f.send("GET", &west, None).await;
    assert_eq!(refused(&reply), (403, "InvalidAccessKeyId".to_owned()));
    let wrong = get_url((FULL.0, "another-secret"), "cas/a", 300);
    let reply = f.send("GET", &wrong, None).await;
    assert_eq!(refused(&reply), (403, "SignatureDoesNotMatch".to_owned()));
}

/// Asserts that a presigned URL of a key that reads does nothing but read one object: the URL of
/// the full key signed for a PUT, a DELETE or a POST is 403 and stores and removes nothing; a GET URL
/// for the listing of the bucket, for the open uploads, for the parts of an upload, for one part
/// and for a parameter that is not one of SigV4 is 403 and tells nothing of the objects.
#[tokio::test(start_paused = true)]
async fn a_presigned_url_of_a_key_that_reads_does_nothing_but_read_one_object() {
    let f = fixture().await;
    f.put("cas/a", b"CONTENT-OF-A").await;
    let id = f.rig.cold_mem.create_upload("cas/open").await.unwrap();
    let full = AccessKey::new(FULL.0, FULL.1);
    for method in ["PUT", "DELETE", "POST"] {
        let url = presign_url(method, &full, &request("cas/new", 300), &[]).unwrap();
        let reply = f.send(method, &url, None).await;
        assert_eq!(refused(&reply), access_denied(), "{method}");
    }
    let delete = presign_url("DELETE", &full, &request("cas/a", 300), &[]).unwrap();
    let reply = f.send("DELETE", &delete, None).await;
    assert_eq!(refused(&reply), access_denied());
    assert_eq!(
        f.rig.get("cas/new", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.rig.get("cas/a", None).await.body, "CONTENT-OF-A");
    let calls: [(&str, &[(&str, &str)]); 6] = [
        ("", &[("list-type", "2")]),
        ("", &[("uploads", "")]),
        ("cas/a", &[("uploadId", "u1")]),
        ("cas/a", &[("partNumber", "1"), ("uploadId", "u1")]),
        ("cas/a", &[("uploads", "")]),
        ("cas/a", &[("response-content-type", "text/plain")]),
    ];
    for (key, extra) in calls {
        let url = presign_url("GET", &full, &request(key, 300), extra).unwrap();
        let reply = f.send("GET", &url, None).await;
        assert_eq!(refused(&reply), access_denied(), "{key} {extra:?}");
        let shown = reply.body.contains("CONTENT-OF-A") || reply.body.contains(&id);
        assert!(!shown, "{key} {extra:?}");
    }
}

/// Asserts the lifetime of a presigned GET URL, on the clock of the server: good until the second
/// `X-Amz-Date + X-Amz-Expires` and refused from it (`AccessDenied`, expired), and for any number of
/// requests before; refused when `X-Amz-Expires` is over `--presign-max-expires`
/// (`AuthorizationQueryParametersError`), even though the key could sign it; and refused when
/// its date is more than 15 minutes ahead of the server's clock (`RequestTimeTooSkewed`).
#[tokio::test(start_paused = true)]
async fn a_presigned_get_is_good_until_it_expires_and_never_beyond_the_cap() {
    let f = fixture().await;
    f.put("cas/a", b"CONTENT-OF-A").await;
    let url = get_url(FULL, "cas/a", 300);
    for elapsed in [0, 1, 299_999] {
        f.clock.set(NOW + elapsed);
        let reply = f.send("GET", &url, None).await;
        assert_eq!(reply.body, "CONTENT-OF-A", "{elapsed}");
    }
    f.clock.set(NOW + 300_000);
    let expired = f.send("GET", &url, None).await;
    assert_eq!(refused(&expired), access_denied());
    assert!(expired.body.contains("Request has expired"));
    assert!(!expired.body.contains("CONTENT-OF-A"));

    f.clock.set(NOW);
    let capped = f.send("GET", &get_url(FULL, "cas/a", 600), None).await;
    assert_eq!(capped.body, "CONTENT-OF-A");
    let over = f.send("GET", &get_url(FULL, "cas/a", 601), None).await;
    let wanted = (403, "AuthorizationQueryParametersError".to_owned());
    assert_eq!(refused(&over), wanted);
    let ahead = PresignRequest {
        now: NOW + 16 * 60 * 1000,
        ..request("cas/a", 300)
    };
    let ahead = presign_get(&AccessKey::new(FULL.0, FULL.1), &ahead).unwrap();
    let skewed = f.send("GET", &ahead, None).await;
    assert_eq!(refused(&skewed), (403, "RequestTimeTooSkewed".to_owned()));
}

/// Asserts that many objects fetched through presigned URLs at once each arrive with exactly their
/// own bytes. Forty-eight objects of different sizes are put; each is then fetched by a task of its
/// own, all started before any is awaited, through a URL signed by the full, the read-only or the
/// scoped key in turn. The test reads no clock and waits on no timer: every task is joined.
#[tokio::test(start_paused = true)]
async fn many_objects_fetched_through_presigned_urls_at_once_arrive_exactly() {
    let f = fixture().await;
    let objects: Vec<(String, Vec<u8>)> = (0..48)
        .map(|n| (format!("cas/many/{n:02}"), pattern(n, 1000 + n * 977)))
        .collect();
    for (key, body) in &objects {
        f.put(key, body).await;
    }
    let signers = [FULL, READER, SCOPED];
    let mut tasks = JoinSet::new();
    for (n, (key, _)) in objects.iter().enumerate() {
        let url = get_url(signers[n % signers.len()], key, 300);
        let server = Arc::clone(&f.rig.server);
        tasks.spawn(async move { (n, fetch(server, url, None).await) });
    }
    let mut got: Vec<Option<(StatusCode, Vec<u8>)>> = vec![None; objects.len()];
    while let Some(done) = tasks.join_next().await {
        let (n, fetched) = done.unwrap();
        got[n] = Some(fetched);
    }
    for (n, (key, body)) in objects.iter().enumerate() {
        let (status, bytes) = got[n].take().expect("every task was joined");
        assert_eq!(status, StatusCode::OK, "{key}");
        assert_eq!(bytes.len(), body.len(), "{key}");
        assert!(bytes == *body, "the bytes of {key} differ");
    }
}

/// Asserts that the ranged parts of one large object, fetched in parallel through one presigned URL
/// and put together in order, are exactly the object. The object is 5 MiB and 12345 bytes; the
/// URL is signed once; eight tasks, each with its own `Range`, are all started before any is awaited,
/// and each is 206 with the length of its range. The last part is the short one. No clock is read
/// and no timer is waited on.
#[tokio::test(start_paused = true)]
async fn ranged_parts_of_one_large_object_fetched_in_parallel_reassemble_exactly() {
    let f = fixture().await;
    let data = pattern(3, 5 * (1 << 20) + 12_345);
    f.put("cas/big/object", &data).await;
    let url = get_url(SCOPED, "cas/big/object", 300);
    let parts = 8;
    let size = data.len().div_ceil(parts);
    let mut tasks = JoinSet::new();
    let mut wanted = Vec::new();
    for n in 0..parts {
        let (first, last) = (n * size, ((n + 1) * size).min(data.len()) - 1);
        wanted.push(last - first + 1);
        let range = Some(format!("bytes={first}-{last}"));
        let server = Arc::clone(&f.rig.server);
        let url = url.clone();
        tasks.spawn(async move { (n, fetch(server, url, range).await) });
    }
    let mut got: Vec<Option<Vec<u8>>> = vec![None; parts];
    while let Some(done) = tasks.join_next().await {
        let (n, (status, bytes)) = done.unwrap();
        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "part {n}");
        assert_eq!(bytes.len(), wanted[n], "part {n}");
        got[n] = Some(bytes);
    }
    let joined: Vec<u8> = got
        .into_iter()
        .flat_map(|part| part.expect("every task was joined"))
        .collect();
    assert_eq!(joined.len(), data.len());
    assert!(
        joined == data,
        "the reassembled bytes differ from the object"
    );
}
