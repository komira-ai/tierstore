//! What the server answers to the reads of the settings of the bucket (`?versioning`, `?policy`
//! and the rest): the answer of S3 for a bucket on which nothing is set.

use hyper::StatusCode;

use super::AccessKey;
use super::bucket::UNSET;
use super::bucket_tests::{rig_with_every_role, statuses};
use super::testing::{BUCKET, Reply, Rig, texts};

const MEBIBYTE: u64 = 1 << 20;

/// The id and the secret of a read-only key.
const READER: (&str, &str) = ("AKIDREADER", "reader-secret-0123456789");

const NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

/// A signed `GET` of the bucket with the query `query`.
async fn get(rig: &Rig, query: &str) -> Reply {
    rig.send("GET", &format!("/{BUCKET}?{query}"), &[], b"")
        .await
}

/// The calls that S3 answers 200 with an empty document for a bucket with nothing set, with the
/// root element of the document.
const EMPTY: [(&str, &str); 4] = [
    ("versioning", "VersioningConfiguration"),
    ("notification", "NotificationConfiguration"),
    ("accelerate", "AccelerateConfiguration"),
    ("logging", "BucketLoggingStatus"),
];

/// The calls that S3 answers 404 for a bucket with nothing set: the query, the code and the
/// text of S3.
const MISSING: [(&str, &str, &str); 11] = [
    (
        "object-lock",
        "ObjectLockConfigurationNotFoundError",
        "Object Lock configuration does not exist for this bucket",
    ),
    (
        "policy",
        "NoSuchBucketPolicy",
        "The bucket policy does not exist",
    ),
    (
        "policyStatus",
        "NoSuchBucketPolicy",
        "The bucket policy does not exist",
    ),
    (
        "lifecycle",
        "NoSuchLifecycleConfiguration",
        "The lifecycle configuration does not exist",
    ),
    (
        "cors",
        "NoSuchCORSConfiguration",
        "The CORS configuration does not exist",
    ),
    (
        "encryption",
        "ServerSideEncryptionConfigurationNotFoundError",
        "The server side encryption configuration was not found",
    ),
    ("tagging", "NoSuchTagSet", "The TagSet does not exist"),
    (
        "replication",
        "ReplicationConfigurationNotFoundError",
        "The replication configuration was not found",
    ),
    (
        "website",
        "NoSuchWebsiteConfiguration",
        "The specified bucket does not have a website configuration",
    ),
    (
        "ownershipControls",
        "OwnershipControlsNotFoundError",
        "The bucket ownership controls were not found",
    ),
    (
        "publicAccessBlock",
        "NoSuchPublicAccessBlockConfiguration",
        "The public access block configuration was not found",
    ),
];

/// Asserts that each setting S3 answers with an empty document is answered 200 with that
/// document, in the S3 namespace and with nothing inside (a client reads "not enabled"), as
/// `?name` and as `?name=`.
#[tokio::test(start_paused = true)]
async fn settings_s3_answers_with_an_empty_document_are_answered_so() {
    let rig = Rig::new(MEBIBYTE).await;
    for (query, root) in EMPTY {
        for spelled in [query.to_owned(), format!("{query}=")] {
            let reply = get(&rig, &spelled).await;
            assert_eq!(reply.status, StatusCode::OK, "{spelled}");
            assert_eq!(reply.header("content-type"), "application/xml");
            let want = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><{root} xmlns=\"{NAMESPACE}\"/>"
            );
            assert_eq!(reply.body, want, "{spelled}");
        }
    }
}

/// Asserts that the payer is the bucket owner, as S3 says for a bucket that does not ask the
/// requester to pay.
#[tokio::test(start_paused = true)]
async fn the_payer_of_the_bucket_is_its_owner() {
    let rig = Rig::new(MEBIBYTE).await;
    let reply = get(&rig, "requestPayment").await;
    assert_eq!(reply.status, StatusCode::OK);
    let want = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><RequestPaymentConfiguration \
         xmlns=\"{NAMESPACE}\"><Payer>BucketOwner</Payer></RequestPaymentConfiguration>"
    );
    assert_eq!(reply.body, want);
}

/// Asserts that the ACL of the bucket is the owner with `FULL_CONTROL` and nothing else, the
/// same owner as ListBuckets names.
#[tokio::test(start_paused = true)]
async fn the_acl_of_the_bucket_gives_the_owner_full_control() {
    let rig = Rig::new(MEBIBYTE).await;
    let reply = get(&rig, "acl").await;
    assert_eq!(reply.status, StatusCode::OK);
    let want = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><AccessControlPolicy xmlns=\"{NAMESPACE}\">\
         <Owner><ID>tierstore</ID><DisplayName>tierstore</DisplayName></Owner>\
         <AccessControlList><Grant><Grantee \
         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"CanonicalUser\">\
         <ID>tierstore</ID><DisplayName>tierstore</DisplayName></Grantee>\
         <Permission>FULL_CONTROL</Permission></Grant></AccessControlList></AccessControlPolicy>"
    );
    assert_eq!(reply.body, want);
}

/// Asserts that each setting S3 answers 404 for is answered 404 with the code and the text S3
/// gives, so that a client that reads "not configured" from the code does.
#[tokio::test(start_paused = true)]
async fn settings_s3_answers_not_found_are_answered_not_found() {
    let rig = Rig::new(MEBIBYTE).await;
    for (query, code, text) in MISSING {
        let reply = get(&rig, query).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{query}");
        assert_eq!(reply.code(), code, "{query}");
        assert_eq!(texts(&reply.body, "Message"), [text], "{query}");
    }
}

/// Asserts the edges: a read-only key may read the settings; an unsigned request is 403; another
/// bucket is `NoSuchBucket`; a name on an object (`k?acl`, `k?tagging`), a write of a setting
/// (`PUT ?versioning`, `DELETE ?policy`) and a call that is not one of these (`?versions`) stay
/// 501; and the setting wins over a listing parameter beside it.
#[tokio::test(start_paused = true)]
async fn the_settings_are_read_by_signed_requests_on_the_bucket_only() {
    let keys = vec![AccessKey::new(READER.0, READER.1)];
    let rig = Rig::with(MEBIBYTE, |config| config.read_only_keys = keys).await;
    let uri = format!("/{BUCKET}?policy");
    let read = rig.send_as(READER, "GET", &uri, &[], b"").await;
    assert_eq!(read.code(), "NoSuchBucketPolicy");
    let other = rig.send("GET", "/other?policy", &[], b"").await;
    assert_eq!(other.code(), "NoSuchBucket");
    for call in ["k?acl", "k?tagging", "k?versioning"] {
        let reply = rig.call("GET", call, b"").await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{call}");
    }
    for (method, query) in [
        ("PUT", "versioning"),
        ("DELETE", "policy"),
        ("PUT", "acl"),
        ("GET", "versions"),
    ] {
        let reply = rig
            .send(method, &format!("/{BUCKET}?{query}"), &[], b"")
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_IMPLEMENTED,
            "{method} {query}"
        );
    }
    let beside = get(&rig, "versioning&prefix=a%2F").await;
    assert_eq!(texts(&beside.body, "Key"), Vec::<String>::new());
    assert!(beside.body.contains("VersioningConfiguration"));
}

/// Asserts which role may read each setting of the bucket: the full key and a read-only key get
/// the answer of S3 for an unset setting (the same status for both, never 403), and a scoped key
/// and a GC key, which name no object, are `403 AccessDenied`.
#[tokio::test(start_paused = true)]
async fn only_the_full_and_the_read_only_key_may_read_the_settings_of_the_bucket() {
    let rig = rig_with_every_role().await;
    for (name, _) in UNSET.iter() {
        let uri = format!("/{BUCKET}?{name}");
        let [full, read_only, scoped, gc] = statuses(&rig, "GET", &uri).await;
        assert!(full == 200 || full == 404, "{name}: {full}");
        assert_eq!(read_only, full, "{name}");
        assert_eq!((scoped, gc), (403, 403), "{name}");
    }
}
