//! ListObjectVersions (`GET /<bucket>?versions`) and `versionId=null` on an object, through
//! `Server::handle`, over a proxy whose tiers are in memory. The bucket is not versioned: S3
//! lists every object as one version with the id `null`, which is the only version id a call on
//! an object may name.

use hyper::StatusCode;

use super::testing::{BUCKET, Reply, Rig, texts};

async fn rig_with(keys: &[&str]) -> Rig {
    let rig = Rig::new(1 << 20).await;
    for key in keys {
        assert_eq!(rig.put(key, b"body").await.status, StatusCode::OK, "{key}");
    }
    rig
}

/// A ListObjectVersions with this query string.
async fn versions(rig: &Rig, query: &str) -> Reply {
    let uri = format!("/{BUCKET}?versions&{query}");
    rig.send("GET", &uri, &[], b"").await
}

fn prefixes(reply: &Reply) -> Vec<String> {
    let doc = roxmltree::Document::parse(&reply.body).expect("well-formed XML");
    doc.descendants()
        .filter(|node| node.tag_name().name() == "CommonPrefixes")
        .flat_map(|node| node.children())
        .filter(|node| node.tag_name().name() == "Prefix")
        .map(|node| node.text().unwrap_or_default().to_owned())
        .collect()
}

async fn on_object(rig: &Rig, method: &str, key: &str, query: &str) -> Reply {
    let uri = format!("/{BUCKET}/{key}?{query}");
    rig.send(method, &uri, &[], b"").await
}

/// Asserts the shape of the answer on a bucket that is not versioned: `ListVersionsResult` with
/// one `Version` for each object, whose `VersionId` is `null` and `IsLatest` is `true`, with the
/// `Key`, `LastModified`, the `ETag` a HEAD gives, `Size`, an `Owner` and `StorageClass`; no
/// `DeleteMarker`; and the markers of the request (empty here), `MaxKeys` and `IsTruncated`.
#[tokio::test(start_paused = true)]
async fn every_object_is_one_version_with_the_id_null() {
    let rig = rig_with(&["a", "b/1"]).await;
    let reply = versions(&rig, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("<ListVersionsResult"), "{}", reply.body);
    assert_eq!(texts(&reply.body, "Name"), [BUCKET]);
    assert_eq!(texts(&reply.body, "Key"), ["a", "b/1"]);
    assert_eq!(texts(&reply.body, "VersionId"), ["null", "null"]);
    assert_eq!(texts(&reply.body, "IsLatest"), ["true", "true"]);
    assert_eq!(texts(&reply.body, "Size"), ["4", "4"]);
    assert_eq!(texts(&reply.body, "ID"), ["tierstore"; 2]);
    assert_eq!(texts(&reply.body, "StorageClass"), ["STANDARD"; 2]);
    assert_eq!(texts(&reply.body, "MaxKeys"), ["1000"]);
    assert_eq!(texts(&reply.body, "IsTruncated"), ["false"]);
    assert_eq!(texts(&reply.body, "KeyMarker"), [""]);
    assert_eq!(texts(&reply.body, "VersionIdMarker"), [""]);
    assert!(texts(&reply.body, "NextKeyMarker").is_empty());
    assert!(texts(&reply.body, "NextVersionIdMarker").is_empty());
    assert!(!reply.body.contains("DeleteMarker"), "{}", reply.body);
    let head = on_object(&rig, "HEAD", "a", "").await;
    let etag = head.headers["etag"].to_str().unwrap().to_owned();
    assert_eq!(texts(&reply.body, "ETag")[0], etag);
}

/// Asserts that `versions` is the call whatever else is in the query: with `list-type=2` it is
/// still ListObjectVersions, not ListObjectsV2.
#[tokio::test(start_paused = true)]
async fn the_versions_parameter_selects_the_call() {
    let rig = rig_with(&["a"]).await;
    let uri = format!("/{BUCKET}?list-type=2&versions");
    let reply = rig.send("GET", &uri, &[], b"").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("<ListVersionsResult"), "{}", reply.body);
}

/// Asserts that `prefix` and `delimiter` fold as they do for ListObjectsV2: under `/` the keys
/// `a/1`, `a/2` and `b/1` give the prefixes `a/` and `b/` and the object `c` its version; the
/// `Prefix` and `Delimiter` of the request are echoed.
#[tokio::test(start_paused = true)]
async fn the_prefix_and_the_delimiter_fold_the_versions() {
    let rig = rig_with(&["a/1", "a/2", "b/1", "c"]).await;
    let reply = versions(&rig, "delimiter=%2F").await;
    assert_eq!(prefixes(&reply), ["a/", "b/"]);
    assert_eq!(texts(&reply.body, "Key"), ["c"]);
    assert_eq!(texts(&reply.body, "Delimiter"), ["/"]);
    let under = versions(&rig, "prefix=a%2F&delimiter=%2F").await;
    assert_eq!(texts(&under.body, "Key"), ["a/1", "a/2"]);
    assert_eq!(texts(&under.body, "Prefix"), ["a/"]);
    assert!(prefixes(&under).is_empty());
}

/// Asserts the paging by `key-marker` and `version-id-marker`: a page cut by `max-keys` is
/// truncated and gives `NextKeyMarker` (its last key) and `NextVersionIdMarker` `null`; the
/// client that sends them back gets every key exactly once; a page that ends with a common
/// prefix gives that prefix as `NextKeyMarker` and an empty `NextVersionIdMarker`, and sending
/// it back passes every key under the prefix.
#[tokio::test(start_paused = true)]
async fn the_markers_page_through_the_versions() {
    let rig = rig_with(&["a", "b", "c", "d/1", "d/2", "e"]).await;
    let first = versions(&rig, "max-keys=2").await;
    assert_eq!(texts(&first.body, "Key"), ["a", "b"]);
    assert_eq!(texts(&first.body, "IsTruncated"), ["true"]);
    assert_eq!(texts(&first.body, "NextKeyMarker"), ["b"]);
    assert_eq!(texts(&first.body, "NextVersionIdMarker"), ["null"]);
    let mut seen = texts(&first.body, "Key");
    let mut query = "max-keys=2&key-marker=b&version-id-marker=null".to_owned();
    loop {
        let page = versions(&rig, &query).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        assert_eq!(texts(&page.body, "KeyMarker").len(), 1);
        seen.extend(texts(&page.body, "Key"));
        if texts(&page.body, "IsTruncated") == ["false"] {
            break;
        }
        let key = texts(&page.body, "NextKeyMarker").remove(0);
        let id = texts(&page.body, "NextVersionIdMarker").remove(0);
        query = format!("max-keys=2&key-marker={key}&version-id-marker={id}");
    }
    assert_eq!(seen, ["a", "b", "c", "d/1", "d/2", "e"]);

    let folded = versions(&rig, "delimiter=%2F&max-keys=4").await;
    assert_eq!(texts(&folded.body, "Key"), ["a", "b", "c"]);
    assert_eq!(prefixes(&folded), ["d/"]);
    assert_eq!(texts(&folded.body, "NextKeyMarker"), ["d/"]);
    assert_eq!(texts(&folded.body, "NextVersionIdMarker"), [""]);
    let rest = versions(&rig, "delimiter=%2F&max-keys=4&key-marker=d%2F").await;
    assert_eq!(texts(&rest.body, "Key"), ["e"]);
    assert_eq!(texts(&rest.body, "IsTruncated"), ["false"]);
}

/// Asserts the rules for the markers: a `key-marker` with no version id marker starts after
/// that key (all its versions are passed, and there is one); a `version-id-marker` with no
/// `key-marker` is 400 `InvalidArgument`, and so is a `version-id-marker` that is not `null`,
/// the only id there is; an empty `version-id-marker` is none.
#[tokio::test(start_paused = true)]
async fn the_markers_are_held_to_the_rules_of_a_listing_of_versions() {
    let rig = rig_with(&["a", "b", "c"]).await;
    let by_key = versions(&rig, "key-marker=a").await;
    assert_eq!(texts(&by_key.body, "Key"), ["b", "c"]);
    assert_eq!(texts(&by_key.body, "KeyMarker"), ["a"]);
    let with_null = versions(&rig, "key-marker=b&version-id-marker=null").await;
    assert_eq!(texts(&with_null.body, "Key"), ["c"]);
    assert_eq!(texts(&with_null.body, "VersionIdMarker"), ["null"]);
    let empty = versions(&rig, "key-marker=a&version-id-marker=").await;
    assert_eq!(texts(&empty.body, "Key"), ["b", "c"]);
    for query in [
        "version-id-marker=null",
        "key-marker=a&version-id-marker=v1",
    ] {
        let bad = versions(&rig, query).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(texts(&bad.body, "Code"), ["InvalidArgument"], "{query}");
    }
}

/// Asserts that `encoding-type=url` percent-encodes the keys, `Prefix`, `KeyMarker`,
/// `NextKeyMarker` and a common prefix and says `EncodingType` `url`, and that another encoding
/// type is 400 `InvalidArgument`; and that `max-keys=0` is an empty untruncated page and a size
/// that is not a number is 400 `InvalidArgument`.
#[tokio::test(start_paused = true)]
async fn encoding_and_page_size_are_as_for_a_listing_of_objects() {
    let rig = rig_with(&["d%20d/1", "e%20e", "f"]).await;
    let reply = versions(
        &rig,
        "encoding-type=url&delimiter=%2F&key-marker=a%20b&max-keys=1",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(texts(&reply.body, "KeyMarker"), ["a+b"]);
    assert_eq!(prefixes(&reply), ["d+d/"]);
    assert_eq!(texts(&reply.body, "NextKeyMarker"), ["d+d/"]);
    assert_eq!(texts(&reply.body, "EncodingType"), ["url"]);
    let keys = versions(&rig, "encoding-type=url&prefix=e").await;
    assert_eq!(texts(&keys.body, "Key"), ["e+e"]);
    let bad = versions(&rig, "encoding-type=base64").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let zero = versions(&rig, "max-keys=0").await;
    assert!(texts(&zero.body, "Key").is_empty());
    assert_eq!(texts(&zero.body, "IsTruncated"), ["false"]);
    let many = versions(&rig, "max-keys=lots").await;
    assert_eq!(many.status, StatusCode::BAD_REQUEST);
    assert_eq!(texts(&many.body, "Code"), ["InvalidArgument"]);
}

/// Asserts that `versionId=null` names the one version of an object: a GET gives the body
/// (and a range of it), a HEAD the headers, and a DELETE removes the object, as the same calls
/// without `versionId` do; and that a key that is not there is `NoSuchKey` as before.
#[tokio::test(start_paused = true)]
async fn version_id_null_is_the_call_without_a_version_id() {
    let rig = rig_with(&["k"]).await;
    let got = on_object(&rig, "GET", "k", "versionId=null").await;
    assert_eq!((got.status, got.body.as_str()), (StatusCode::OK, "body"));
    let plain = on_object(&rig, "HEAD", "k", "").await;
    let head = on_object(&rig, "HEAD", "k", "versionId=null").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.headers["etag"], plain.headers["etag"]);
    assert_eq!(head.headers["content-length"], "4");
    let ranged = rig
        .send(
            "GET",
            &format!("/{BUCKET}/k?versionId=null"),
            &[("range", "bytes=1-2")],
            b"",
        )
        .await;
    assert_eq!(
        (ranged.status, ranged.body.as_str()),
        (StatusCode::PARTIAL_CONTENT, "od")
    );
    let deleted = on_object(&rig, "DELETE", "k", "versionId=null").await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = on_object(&rig, "HEAD", "k", "").await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let missing = on_object(&rig, "GET", "k", "versionId=null").await;
    assert_eq!(texts(&missing.body, "Code"), ["NoSuchKey"]);
}

/// Asserts that any other version id is 404 `NoSuchVersion` on a GET, a HEAD and a DELETE
/// (whether or not the key exists), and that the DELETE leaves the object where it was.
#[tokio::test(start_paused = true)]
async fn another_version_id_is_no_such_version() {
    let rig = rig_with(&["k"]).await;
    for (method, key) in [("GET", "k"), ("DELETE", "k"), ("GET", "absent")] {
        let reply = on_object(&rig, method, key, "versionId=3sL4kqtJlcpXroDTDmJ").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {key}");
        if method != "HEAD" {
            assert_eq!(
                texts(&reply.body, "Code"),
                ["NoSuchVersion"],
                "{method} {key}"
            );
        }
    }
    let head = on_object(&rig, "HEAD", "k", "versionId=other").await;
    assert_eq!(head.status, StatusCode::NOT_FOUND);
    let still = on_object(&rig, "GET", "k", "").await;
    assert_eq!(
        (still.status, still.body.as_str()),
        (StatusCode::OK, "body")
    );
}

/// Asserts that `versionId` is only for a GET, a HEAD and a DELETE of an object: on a put or on
/// a call of a multipart upload it is 501 `NotImplemented`, and a key of the proxy's own is
/// refused with `AccessDenied` before the version id is looked at.
#[tokio::test(start_paused = true)]
async fn version_id_belongs_to_the_reads_and_the_delete_of_an_object() {
    let rig = rig_with(&[]).await;
    let put = rig
        .send(
            "PUT",
            &format!("/{BUCKET}/k?versionId=null"),
            &[("if-none-match", "*")],
            b"x",
        )
        .await;
    assert_eq!(put.status, StatusCode::NOT_IMPLEMENTED);
    let start = on_object(&rig, "POST", "k", "uploads&versionId=null").await;
    assert_eq!(start.status, StatusCode::NOT_IMPLEMENTED);
    let reserved = on_object(&rig, "GET", "_journal/x", "versionId=other").await;
    assert_eq!(texts(&reserved.body, "Code"), ["AccessDenied"]);
}
