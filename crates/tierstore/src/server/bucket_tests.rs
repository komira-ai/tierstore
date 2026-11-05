//! The calls on the bucket itself, through `Server::handle`.

use std::sync::Arc;

use crate::journal::FORMAT_KEY;
use crate::{Backend, MemoryBackend, TestClock};
use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, StatusCode};

use super::bucket::created_at;
use super::presign::tests::{SECRET as PRESIGN_SECRET, grant, request};
use super::presign::{PresignKey, PresignRequest, presign_url};
use super::testing::{BUCKET, KEY_ID, NOW, Reply, Rig, SECRET, texts};
use super::{AccessKey, GcKey, Prefixes, ScopedKey};

const MEBIBYTE: u64 = 1 << 20;

/// The id and the secret of a read-only key.
const READER: (&str, &str) = ("AKIDREADER", "reader-secret-0123456789");

/// The id and the secret of a scoped key, and of a GC key.
const SCOPED: (&str, &str) = ("AKIDSCOPED", "scoped-secret-0123456789");
const GC: (&str, &str) = ("AKIDGC", "gc-secret-0123456789");

/// A rig with one key of each role: the full key (the rig's own), a read-only key, a scoped key
/// and a GC key.
pub(super) async fn rig_with_every_role() -> Rig {
    Rig::with(MEBIBYTE, |config| {
        config.read_only_keys = vec![AccessKey::new(READER.0, READER.1)];
        let prefixes = Prefixes {
            read: &["r/"],
            write: &["w/"],
            delete: &["d/"],
        };
        config.scoped_keys = vec![ScopedKey::new(SCOPED.0, SCOPED.1, &prefixes).unwrap()];
        config.gc_keys = vec![GcKey::new(GC.0, GC.1, "cas/b/").unwrap()];
    })
    .await
}

/// The status of a call on `uri`, signed by the key of each role, in the order full, read-only,
/// scoped, GC.
pub(super) async fn statuses(rig: &Rig, method: &str, uri: &str) -> [u16; 4] {
    let full = rig.send(method, uri, &[], b"").await.status.as_u16();
    let mut out = [full, 0, 0, 0];
    for (slot, key) in out[1..].iter_mut().zip([READER, SCOPED, GC]) {
        *slot = rig
            .send_as(key, method, uri, &[], b"")
            .await
            .status
            .as_u16();
    }
    out
}

/// A rig whose server is in the region `region`.
async fn rig_in(region: &'static str) -> Rig {
    Rig::with(MEBIBYTE, |config| config.region = region.to_owned()).await
}

/// A `GET` of the bucket with the query `query`.
async fn get(rig: &Rig, query: &str) -> Reply {
    rig.send("GET", &format!("/{BUCKET}?{query}"), &[], b"")
        .await
}

/// Asserts that GetBucketLocation in `us-east-1` answers 200 with an empty `LocationConstraint`
/// in the S3 namespace (S3 gives no name for that region), as an XML document.
#[tokio::test(start_paused = true)]
async fn location_of_the_bucket_in_us_east_1_is_empty() {
    let rig = rig_in("us-east-1").await;
    let reply = get(&rig, "location").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), "application/xml");
    assert_eq!(texts(&reply.body, "LocationConstraint"), [""]);
    assert!(
        reply
            .body
            .contains("<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"")
    );
}

/// Asserts that GetBucketLocation names the region the server is configured with, for a region
/// other than `us-east-1`.
#[tokio::test(start_paused = true)]
async fn location_of_the_bucket_is_the_region_of_the_server() {
    let rig = rig_in("eu-west-1").await;
    let reply = get(&rig, "location").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(texts(&reply.body, "LocationConstraint"), ["eu-west-1"]);
}

/// Asserts the edges of the call: `?location=` with a value is the same call, a read-only key may
/// make it, a request that is not signed is refused as every call is, another bucket is
/// `NoSuchBucket`, and `location` on an object, or in a put of the bucket, is still
/// `NotImplemented`.
#[tokio::test(start_paused = true)]
async fn location_is_for_signed_requests_on_the_bucket_only() {
    let keys = vec![AccessKey::new(READER.0, READER.1)];
    let rig = Rig::with(MEBIBYTE, |config| config.read_only_keys = keys).await;
    let valued = get(&rig, "location=").await;
    assert_eq!(valued.status, StatusCode::OK);
    let uri = format!("/{BUCKET}?location");
    let read = rig.send_as(READER, "GET", &uri, &[], b"").await;
    assert_eq!(read.status, StatusCode::OK);
    let bare = Request::get(uri.as_str())
        .body(Empty::<Bytes>::new())
        .unwrap();
    let anonymous = rig.send_raw(bare).await;
    assert_eq!(anonymous.status, StatusCode::FORBIDDEN);
    let other = rig.send("GET", "/other?location", &[], b"").await;
    assert_eq!(other.code(), "NoSuchBucket");
    let object = rig.call("GET", "k?location", b"").await;
    assert_eq!(object.status, StatusCode::NOT_IMPLEMENTED);
    let put = rig.send("PUT", &uri, &[], b"").await;
    assert_eq!(put.status, StatusCode::NOT_IMPLEMENTED);
}

/// The document the service root answers in `eu-west-1`, with the creation date of the rig's clock.
const LISTING: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
<Owner><ID>tierstore</ID><DisplayName>tierstore</DisplayName></Owner>\
<Buckets><Bucket><Name>tier-bucket</Name><CreationDate>2027-01-15T08:00:00.000Z</CreationDate>\
<BucketRegion>eu-west-1</BucketRegion></Bucket></Buckets></ListAllMyBucketsResult>";

/// A `GET` of the service root with the query `query`.
async fn service(rig: &Rig, query: &str) -> Reply {
    rig.send("GET", &format!("/?{query}"), &[], b"").await
}

/// Asserts the whole answer of ListBuckets, by exact equality: the one bucket the server serves,
/// with its creation date (the time the config was made at, the clock of the rig), its
/// region and the owner, in the S3 namespace.
#[tokio::test(start_paused = true)]
async fn listing_the_buckets_gives_the_one_bucket_the_server_serves() {
    let rig = rig_in("eu-west-1").await;
    let reply = rig.send("GET", "/", &[], b"").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), "application/xml");
    assert_eq!(reply.body, LISTING);
}

/// Asserts that the filters of ListBuckets narrow the list as S3 narrows it: `prefix` keeps the
/// bucket only if its name starts with it, `bucket-region` only if it is in that region, a
/// `max-buckets` in range changes nothing for one bucket, and `x-id`, which the SDKs add, is
/// ignored. A list narrowed to nothing still has its `Buckets` element, empty.
#[tokio::test(start_paused = true)]
async fn the_filters_of_a_listing_of_buckets_keep_or_drop_the_bucket() {
    let rig = rig_in("eu-west-1").await;
    for kept in [
        "prefix=tier",
        "prefix=",
        "bucket-region=eu-west-1",
        "max-buckets=1",
        "max-buckets=10000",
        "x-id=ListBuckets",
        "prefix=tier-bucket&bucket-region=eu-west-1&max-buckets=5",
    ] {
        let reply = service(&rig, kept).await;
        assert_eq!(reply.status, StatusCode::OK, "{kept}");
        assert_eq!(texts(&reply.body, "Name"), [BUCKET], "{kept}");
    }
    for dropped in [
        "prefix=other",
        "prefix=tier-bucket-longer",
        "bucket-region=us-east-1",
        "prefix=tier&bucket-region=us-west-2",
    ] {
        let reply = service(&rig, dropped).await;
        assert_eq!(reply.status, StatusCode::OK, "{dropped}");
        assert!(texts(&reply.body, "Name").is_empty(), "{dropped}");
        assert!(reply.body.contains("<Buckets></Buckets>"), "{dropped}");
        assert_eq!(texts(&reply.body, "ID"), ["tierstore"], "{dropped}");
    }
}

/// Asserts the refusals of ListBuckets: a `max-buckets` that is not a whole number from 1 to
/// 10000, and a continuation token (the list is never cut, so the server gave none), are 400
/// `InvalidArgument`; a parameter the call does not have is 501 `NotImplemented`.
#[tokio::test(start_paused = true)]
async fn a_listing_of_buckets_with_a_bad_argument_is_refused() {
    let rig = rig_in("eu-west-1").await;
    for bad in [
        "max-buckets=0",
        "max-buckets=10001",
        "max-buckets=many",
        "max-buckets=-1",
        "continuation-token=abc",
    ] {
        let reply = service(&rig, bad).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(reply.code(), "InvalidArgument", "{bad}");
    }
    let unknown = service(&rig, "delimiter=%2F").await;
    assert_eq!(unknown.status, StatusCode::NOT_IMPLEMENTED);
    for method in ["PUT", "DELETE", "POST"] {
        let reply = rig.send(method, "/", &[], b"").await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{method}");
    }
}

/// Asserts who may list the buckets: the full key and a read-only key may; a scoped key (it
/// is not told of buckets), a presigning key and a presigned URL may not (403 `AccessDenied`,
/// with the reason of each); and a request that is not signed is 403.
#[tokio::test(start_paused = true)]
async fn only_the_full_and_read_only_keys_list_the_buckets() {
    let scoped = ScopedKey::new(
        "scoped-a",
        "scoped-secret-0123456789",
        &Prefixes {
            read: &["r/"],
            write: &[],
            delete: &[],
        },
    )
    .unwrap();
    let presign = PresignKey::new("pack-presign", PRESIGN_SECRET, grant()).unwrap();
    let readers = vec![AccessKey::new(READER.0, READER.1)];
    let rig = Rig::with(MEBIBYTE, |config| {
        config.read_only_keys = readers;
        config.scoped_keys = vec![scoped];
        config.presign_keys = vec![presign];
    })
    .await;
    assert_eq!(rig.send("GET", "/", &[], b"").await.status, StatusCode::OK);
    let read = rig.send_as(READER, "GET", "/", &[], b"").await;
    assert_eq!(read.status, StatusCode::OK);
    let scoped = ("scoped-a", "scoped-secret-0123456789");
    let refused = [
        (scoped, "may not make this call"),
        (("pack-presign", PRESIGN_SECRET), "presigned PUT URLs only"),
    ];
    for (key, why) in refused {
        let reply = rig.send_as(key, "GET", "/", &[], b"").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{why}");
        assert_eq!(reply.code(), "AccessDenied", "{why}");
        assert!(reply.body.contains(why), "{}", reply.body);
    }
    let named = PresignRequest {
        bucket: "",
        now: NOW,
        ..request("", 300)
    };
    let url = presign_url("GET", &AccessKey::new(KEY_ID, SECRET), &named, &[]).unwrap();
    let path = url.strip_prefix("http://tier.test").unwrap().to_owned();
    let presigned = Request::get(path)
        .header("host", "tier.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let reply = rig.send_raw(presigned).await;
    assert_eq!(reply.code(), "AccessDenied");
    assert!(
        reply
            .body
            .contains("good for GET or HEAD of one object only")
    );
    let bare = Request::get("/").body(Empty::<Bytes>::new()).unwrap();
    assert_eq!(rig.send_raw(bare).await.status, StatusCode::FORBIDDEN);
}

/// Asserts which role may make GetBucketLocation: the full key and a read-only key do (200), a
/// scoped key and a GC key are `403 AccessDenied`, as they are for every call on the bucket
/// itself.
#[tokio::test(start_paused = true)]
async fn only_the_full_and_the_read_only_key_may_get_the_location() {
    let rig = rig_with_every_role().await;
    let uri = format!("/{BUCKET}?location");
    assert_eq!(statuses(&rig, "GET", &uri).await, [200, 200, 403, 403]);
}

/// Asserts that the creation date of the listed bucket is the `bucket_created` of the config, to
/// the millisecond, in S3's `Z` form.
#[tokio::test(start_paused = true)]
async fn the_listed_bucket_has_the_creation_date_of_the_config() {
    let rig = Rig::with(MEBIBYTE, |config| {
        config.bucket_created = 1_369_353_600_123;
    })
    .await;
    let reply = rig.send("GET", "/", &[], b"").await;
    assert_eq!(
        texts(&reply.body, "CreationDate"),
        ["2013-05-24T00:00:00.123Z"]
    );
}

/// Asserts the source of the creation date: the time the `_tiering/format` object of the hot tier
/// was written when there is one, and the fallback (the time the config was made at) where the
/// store has none.
#[tokio::test(start_paused = true)]
async fn the_bucket_was_made_when_the_format_object_was_written() {
    let fallback = 1_000;
    let bare = MemoryBackend::new();
    assert_eq!(created_at(&bare, fallback).await, fallback);
    let written = 1_369_353_600_123;
    let clock = Arc::new(TestClock::new());
    clock.set(written);
    let formatted = MemoryBackend::with_clock(Arc::clone(&clock));
    let body = Bytes::from_static(b"{\"epoch\":2,\"phase\":\"active\"}");
    formatted.put(FORMAT_KEY, body).await.unwrap();
    clock.set(written + 5_000);
    assert_eq!(created_at(&formatted, fallback).await, written);
}

/// Asserts which role may list the buckets, for all four roles: the full key and a read-only key
/// do (200), a scoped key and a GC key are `403 AccessDenied`.
#[tokio::test(start_paused = true)]
async fn only_the_full_and_the_read_only_key_may_list_the_buckets_of_every_role() {
    let rig = rig_with_every_role().await;
    assert_eq!(statuses(&rig, "GET", "/").await, [200, 200, 403, 403]);
}
