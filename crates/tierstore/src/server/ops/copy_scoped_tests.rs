//! CopyObject and UploadPartCopy signed by a scoped key, through `Server::handle`: the key needs
//! the read right of the source and the create-only write right of the destination.

use hyper::StatusCode;

use crate::server::ops_tests::completion;
use crate::server::testing::{BUCKET, Reply, Rig, texts};
use crate::server::{Prefixes, ScopedKey};

const MEBIBYTE: u64 = 1 << 20;

/// The key of the tests: reads `r/`, creates under `w/`, and deletes nothing.
const KEY: (&str, &str) = ("scoped-copy", "scoped-copy-secret-0123456789");

/// A rig with the scoped key and the object `r/a`, which holds ten bytes.
async fn rig() -> Rig {
    let made = Rig::with(MEBIBYTE, |config| {
        let prefixes = Prefixes {
            read: &["r/"],
            write: &["w/"],
            delete: &[],
        };
        config.scoped_keys = vec![ScopedKey::new(KEY.0, KEY.1, &prefixes).unwrap()];
    });
    let rig = made.await;
    rig.put("r/a", b"0123456789").await;
    rig
}

fn uri(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

/// A `PUT` of `key` by the scoped key that copies `source` (the text of the header as it is).
async fn copy(rig: &Rig, key: &str, source: &str, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![("x-amz-copy-source", source)];
    headers.extend_from_slice(extra);
    rig.send_as(KEY, "PUT", &uri(key), &headers, b"").await
}

fn outcome(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn access_denied() -> (u16, String) {
    (403, "AccessDenied".to_owned())
}

async fn exists(rig: &Rig, key: &str) -> bool {
    rig.get(key, None).await.status == StatusCode::OK
}

/// Asserts that a scoped key that reads `r/` and creates under `w/` copies `r/a` to `w/new`: 200
/// with a `CopyObjectResult` that names the tag of the new object, the bytes are those of the
/// source, and the source is as it was; the source may be named percent-encoded and with the
/// version `null`.
#[tokio::test(start_paused = true)]
async fn a_scoped_key_copies_a_readable_object_to_a_writable_key() {
    let rig = rig().await;
    let reply = copy(&rig, "w/new", "/tier-bucket/r/a", &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("CopyObjectResult"), "{}", reply.body);
    let made = rig.get("w/new", None).await;
    assert_eq!(made.body, "0123456789");
    assert_eq!(texts(&reply.body, "ETag"), [made.header("etag")]);
    assert_eq!(rig.get("r/a", None).await.body, "0123456789");
    let encoded = copy(&rig, "w/encoded", "tier-bucket/r%2Fa", &[]).await;
    assert_eq!(encoded.status, StatusCode::OK, "{}", encoded.body);
    assert_eq!(rig.get("w/encoded", None).await.body, "0123456789");
    let versioned = copy(&rig, "w/versioned", "/tier-bucket/r/a?versionId=null", &[]).await;
    assert_eq!(versioned.status, StatusCode::OK, "{}", versioned.body);
    assert_eq!(rig.get("w/versioned", None).await.body, "0123456789");
}

/// Asserts that a scoped copy only creates: with no `If-None-Match` and with `If-None-Match: *` a
/// copy onto a key that exists is 412 `PreconditionFailed` and leaves the object as it was, and
/// both create a key that is free.
#[tokio::test(start_paused = true)]
async fn a_scoped_copy_creates_only_with_or_without_if_none_match() {
    let rig = rig().await;
    let star = [("if-none-match", "*")];
    for (key, extra) in [("w/bare", &[][..]), ("w/star", &star[..])] {
        let first = copy(&rig, key, "/tier-bucket/r/a", extra).await;
        assert_eq!(first.status, StatusCode::OK, "{key}: {}", first.body);
        let again = copy(&rig, key, "/tier-bucket/r/a", extra).await;
        assert_eq!(
            outcome(&again),
            (412, "PreconditionFailed".to_owned()),
            "{key}"
        );
    }
    rig.put("w/there", b"old").await;
    for extra in [&[][..], &star[..]] {
        let over = copy(&rig, "w/there", "/tier-bucket/r/a", extra).await;
        assert_eq!(outcome(&over), (412, "PreconditionFailed".to_owned()));
        assert_eq!(rig.get("w/there", None).await.body, "old");
    }
}

/// Asserts that a scoped copy that would replace is 403 whatever the source: `If-Match` and an
/// `If-None-Match` other than `*` are refused for a key that exists and for one that does not, and
/// nothing is created.
#[tokio::test(start_paused = true)]
async fn a_scoped_copy_that_would_replace_is_refused() {
    let rig = rig().await;
    rig.put("w/there", b"old").await;
    for key in ["w/there", "w/fresh"] {
        for extra in [("if-match", "\"abc\""), ("if-none-match", "\"abc\"")] {
            let reply = copy(&rig, key, "/tier-bucket/r/a", &[extra]).await;
            assert_eq!(outcome(&reply), access_denied(), "{key} {extra:?}");
        }
    }
    assert_eq!(rig.get("w/there", None).await.body, "old");
    assert!(!exists(&rig, "w/fresh").await);
}

/// Asserts that a scoped key cannot copy what it cannot read: an object under a write prefix, one
/// under no prefix of the key, a name that climbs out of the read prefix, the prefix itself, and a
/// key of the proxy are 403 as sources, and nothing is created.
#[tokio::test(start_paused = true)]
async fn a_scoped_copy_of_an_object_the_key_cannot_read_is_refused() {
    let rig = rig().await;
    rig.put("w/x", b"CONTENT-OF-X").await;
    rig.put("other/y", b"CONTENT-OF-Y").await;
    for source in [
        "/tier-bucket/w/x",
        "/tier-bucket/other/y",
        "/tier-bucket/r/../other/y",
        "/tier-bucket/r/",
        "/tier-bucket/_journal/p1/x",
    ] {
        let reply = copy(&rig, "w/new", source, &[]).await;
        assert_eq!(outcome(&reply), access_denied(), "{source}");
        assert!(!reply.body.contains("CONTENT-OF"), "{source}");
        assert!(!exists(&rig, "w/new").await, "{source}");
    }
}

/// Asserts that a scoped key cannot copy onto a key it may not write: `r/b` (a read prefix),
/// `other/b` and a destination that climbs out of `w/` are 403 for a source it reads, and nothing
/// is created.
#[tokio::test(start_paused = true)]
async fn a_scoped_copy_onto_a_key_the_key_cannot_write_is_refused() {
    let rig = rig().await;
    for key in ["r/b", "other/b", "w/../other/b"] {
        let reply = copy(&rig, key, "/tier-bucket/r/a", &[]).await;
        assert_eq!(outcome(&reply), access_denied(), "{key}");
        assert!(!exists(&rig, key).await, "{key}");
    }
}

/// Asserts that a source a scoped key's check cannot parse is 403, not a 400 that would tell more:
/// one that names no key, no bucket, a parameter other than `versionId`, a version other than
/// `null`, and one that is not percent-encoded UTF-8; nothing is created.
#[tokio::test(start_paused = true)]
async fn a_scoped_copy_with_a_source_that_cannot_be_parsed_is_refused() {
    let rig = rig().await;
    for source in [
        "r",
        "/",
        "tier-bucket",
        "tier-bucket/",
        "/tier-bucket/r/a?other=1",
        "/tier-bucket/r/a?versionId=3",
        "/tier-bucket/r/%ff",
    ] {
        let reply = copy(&rig, "w/new", source, &[]).await;
        assert_eq!(outcome(&reply), access_denied(), "{source}");
        assert!(!exists(&rig, "w/new").await, "{source}");
    }
}

/// Asserts that the bucket of the source is the front's to check, after the read right: a source
/// the key reads in another bucket is 404 `NoSuchBucket`, and nothing is created.
#[tokio::test(start_paused = true)]
async fn a_scoped_copy_from_another_bucket_is_no_such_bucket() {
    let rig = rig().await;
    let reply = copy(&rig, "w/new", "/elsewhere/r/a", &[]).await;
    assert_eq!(outcome(&reply), (404, "NoSuchBucket".to_owned()));
    assert!(!exists(&rig, "w/new").await);
}

/// Asserts that a scoped key copies into a part of an upload under its write prefix: the whole
/// source as part 1 and a range of it as part 2 answer 200 `CopyPartResult`, and the completion
/// makes the object from them in order.
#[tokio::test(start_paused = true)]
async fn a_scoped_key_copies_whole_and_ranged_parts_into_its_upload() {
    let rig = rig().await;
    let started = rig
        .send_as(KEY, "POST", &uri("w/big?uploads"), &[], b"")
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let id = texts(&started.body, "UploadId").concat();
    let mut tags = Vec::new();
    for (number, extra) in [
        (1, vec![]),
        (2, vec![("x-amz-copy-source-range", "bytes=2-5")]),
    ] {
        let target = uri(&format!("w/big?partNumber={number}&uploadId={id}"));
        let mut headers = vec![("x-amz-copy-source", "/tier-bucket/r/a")];
        headers.extend(extra);
        let part = rig.send_as(KEY, "PUT", &target, &headers, b"").await;
        assert_eq!(part.status, StatusCode::OK, "{}", part.body);
        assert!(part.body.contains("CopyPartResult"), "{}", part.body);
        tags.push(texts(&part.body, "ETag").concat());
    }
    let done = completion(&[(1, tags[0].as_str()), (2, tags[1].as_str())]);
    let complete = uri(&format!("w/big?uploadId={id}"));
    let end = rig
        .send_as(KEY, "POST", &complete, &[], done.as_bytes())
        .await;
    assert_eq!(end.status, StatusCode::OK, "{}", end.body);
    assert_eq!(rig.get("w/big", None).await.body, "01234567892345");
}

/// Asserts the refusals of a scoped part copy: a source the key cannot read and an upload of a key
/// outside the write prefixes are 403 and store no part, an upload id that does not exist is 404
/// `NoSuchUpload` (not `AccessDenied`), and a range past the end of the source is 400
/// `InvalidArgument`.
#[tokio::test(start_paused = true)]
async fn a_scoped_part_copy_is_refused_for_a_source_or_an_upload_outside_the_key() {
    let rig = rig().await;
    rig.put("w/x", b"unreadable").await;
    let started = rig
        .send_as(KEY, "POST", &uri("w/big?uploads"), &[], b"")
        .await;
    let id = texts(&started.body, "UploadId").concat();
    let target = format!("w/big?partNumber=1&uploadId={id}");
    let hidden = copy(&rig, &target, "/tier-bucket/w/x", &[]).await;
    assert_eq!(outcome(&hidden), access_denied());
    let outside = rig.call("POST", "r/big?uploads", b"").await;
    let other = texts(&outside.body, "UploadId").concat();
    let outer = format!("r/big?partNumber=1&uploadId={other}");
    let refused = copy(&rig, &outer, "/tier-bucket/r/a", &[]).await;
    assert_eq!(outcome(&refused), access_denied());
    let missing = copy(
        &rig,
        "w/big?partNumber=1&uploadId=nope",
        "/tier-bucket/r/a",
        &[],
    )
    .await;
    assert_eq!(outcome(&missing), (404, "NoSuchUpload".to_owned()));
    let past = [("x-amz-copy-source-range", "bytes=5-10")];
    let long = copy(&rig, &target, "/tier-bucket/r/a", &past).await;
    assert_eq!(outcome(&long), (400, "InvalidArgument".to_owned()));
    let listed = rig
        .send_as(KEY, "GET", &uri(&format!("w/big?uploadId={id}")), &[], b"")
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert!(
        texts(&listed.body, "PartNumber").is_empty(),
        "{}",
        listed.body
    );
}
