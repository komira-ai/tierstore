//! The calls on the bucket itself, through `Server::handle`.

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, StatusCode};

use super::testing::{BUCKET, Reply, Rig, texts};
use super::{AccessKey, GcKey, Prefixes, ScopedKey};

const MEBIBYTE: u64 = 1 << 20;

/// The id and the secret of a read-only key.
const READER: (&str, &str) = ("AKIDREADER", "reader-secret-0123456789");

/// The id and the secret of a scoped key, and of a GC key.
const SCOPED: (&str, &str) = ("AKIDSCOPED", "scoped-secret-0123456789");
const GC: (&str, &str) = ("AKIDGC", "gc-secret-0123456789");

/// A rig with one key of each role: the full key (the rig's own), a read-only key, a scoped key
/// and a GC key.
async fn rig_with_every_role() -> Rig {
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
async fn statuses(rig: &Rig, method: &str, uri: &str) -> [u16; 4] {
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

/// Asserts which role may make GetBucketLocation: the full key and a read-only key do (200), a
/// scoped key and a GC key are `403 AccessDenied`, as they are for every call on the bucket
/// itself.
#[tokio::test(start_paused = true)]
async fn only_the_full_and_the_read_only_key_may_get_the_location() {
    let rig = rig_with_every_role().await;
    let uri = format!("/{BUCKET}?location");
    assert_eq!(statuses(&rig, "GET", &uri).await, [200, 200, 403, 403]);
}
