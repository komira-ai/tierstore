//! ListObjects (version 1: `GET /<bucket>` with no `list-type=2`) through `Server::handle`,
//! over a proxy whose tiers are in memory.

use hyper::StatusCode;

use super::testing::{BUCKET, Reply, Rig, texts};

async fn rig_with(keys: &[&str]) -> Rig {
    let rig = Rig::new(1 << 20).await;
    for key in keys {
        assert_eq!(rig.put(key, b"x").await.status, StatusCode::OK, "{key}");
    }
    rig
}

/// A ListObjects (v1) with this query string.
async fn v1(rig: &Rig, query: &str) -> Reply {
    rig.send("GET", &format!("/{BUCKET}?{query}"), &[], b"")
        .await
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

/// Asserts the shape of a version 1 answer: a request with no `list-type` is a ListObjects, not
/// a refusal, and its answer has `Marker` (empty here), `MaxKeys`, `IsTruncated`, and for each
/// object the `Key`, `ETag` (the tag a HEAD of the object gives), `Size`, an `Owner` (always, in
/// version 1) and the `StorageClass`; and has none of the elements of version 2 (`KeyCount`,
/// `ContinuationToken`, `NextContinuationToken`, `StartAfter`).
#[tokio::test(start_paused = true)]
async fn a_request_without_list_type_is_a_version_1_listing() {
    let rig = rig_with(&["a", "b"]).await;
    let reply = v1(&rig, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("<ListBucketResult"), "{}", reply.body);
    assert_eq!(texts(&reply.body, "Name"), [BUCKET]);
    assert_eq!(texts(&reply.body, "Marker"), [""]);
    assert_eq!(texts(&reply.body, "MaxKeys"), ["1000"]);
    assert_eq!(texts(&reply.body, "IsTruncated"), ["false"]);
    assert_eq!(texts(&reply.body, "Key"), ["a", "b"]);
    assert_eq!(texts(&reply.body, "Size"), ["1", "1"]);
    assert_eq!(texts(&reply.body, "ID"), ["tierstore"; 2]);
    assert_eq!(texts(&reply.body, "StorageClass"), ["STANDARD"; 2]);
    let head = rig.send("HEAD", &format!("/{BUCKET}/a"), &[], b"").await;
    let etag = head.headers["etag"].to_str().unwrap().to_owned();
    assert_eq!(texts(&reply.body, "ETag")[0], etag);
    for absent in [
        "KeyCount",
        "ContinuationToken",
        "NextContinuationToken",
        "StartAfter",
        "NextMarker",
    ] {
        assert!(texts(&reply.body, absent).is_empty(), "{absent}");
    }
}

/// Asserts that `list-type=1` is a version 1 listing too, and that the parameters of version 2
/// (`start-after`, `continuation-token`, `fetch-owner`) mean nothing to it: the listing starts at
/// the first key and echoes none of them.
#[tokio::test(start_paused = true)]
async fn the_parameters_of_version_2_are_ignored_by_version_1() {
    let rig = rig_with(&["a", "b", "c"]).await;
    let query = "list-type=1&start-after=b&continuation-token=zz&fetch-owner=false";
    let reply = v1(&rig, query).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(texts(&reply.body, "Key"), ["a", "b", "c"]);
    assert_eq!(texts(&reply.body, "ID").len(), 3);
    assert!(texts(&reply.body, "StartAfter").is_empty());
    assert!(texts(&reply.body, "ContinuationToken").is_empty());
}

/// Asserts that `marker` starts the listing after that key, which need not exist, and is echoed
/// as `Marker`: after `b` come `c` and `d`; after `bb` (no such key) the same; an empty marker is
/// no marker.
#[tokio::test(start_paused = true)]
async fn the_marker_starts_the_listing_after_it() {
    let rig = rig_with(&["a", "b", "c", "d"]).await;
    let after_b = v1(&rig, "marker=b").await;
    assert_eq!(texts(&after_b.body, "Key"), ["c", "d"]);
    assert_eq!(texts(&after_b.body, "Marker"), ["b"]);
    let after_bb = v1(&rig, "marker=bb").await;
    assert_eq!(texts(&after_bb.body, "Key"), ["c", "d"]);
    assert_eq!(texts(&after_bb.body, "Marker"), ["bb"]);
    let empty = v1(&rig, "marker=").await;
    assert_eq!(texts(&empty.body, "Key"), ["a", "b", "c", "d"]);
}

/// Asserts the paging without a delimiter: a page cut by `max-keys` is `IsTruncated` and has no
/// `NextMarker` (S3 gives it only with a delimiter), and the client that takes the last `Key` as
/// the marker gets every key exactly once.
#[tokio::test(start_paused = true)]
async fn without_a_delimiter_the_last_key_is_the_next_marker() {
    let rig = rig_with(&["a", "b", "c", "d", "e"]).await;
    let first = v1(&rig, "max-keys=2").await;
    assert_eq!(texts(&first.body, "Key"), ["a", "b"]);
    assert_eq!(texts(&first.body, "IsTruncated"), ["true"]);
    assert!(texts(&first.body, "NextMarker").is_empty());
    let mut seen = texts(&first.body, "Key");
    let mut last = seen.last().cloned().unwrap();
    for _ in 0..5 {
        let page = v1(&rig, &format!("max-keys=2&marker={last}")).await;
        let keys = texts(&page.body, "Key");
        seen.extend(keys.clone());
        last = keys.last().cloned().unwrap();
        if texts(&page.body, "IsTruncated") == ["false"] {
            break;
        }
    }
    assert_eq!(seen, ["a", "b", "c", "d", "e"]);
}

/// Asserts the paging with a delimiter: a truncated page has `NextMarker`, the last key of the
/// page or, when the page ends with a common prefix, that prefix; and the client that gives it
/// back as `marker` gets each object and each prefix exactly once. Over `a/1`, `a/2`, `b/1`, `c`
/// with `max-keys=1` the pages are `a/` (`NextMarker` `a/`), `b/`, and `c` (not truncated, no
/// `NextMarker`); and a page that ends with an object has that key as `NextMarker`.
#[tokio::test(start_paused = true)]
async fn with_a_delimiter_next_marker_pages_across_common_prefixes() {
    let rig = rig_with(&["a/1", "a/2", "b/1", "c"]).await;
    let first = v1(&rig, "delimiter=%2F&max-keys=1").await;
    assert_eq!(prefixes(&first), ["a/"]);
    assert_eq!(texts(&first.body, "IsTruncated"), ["true"]);
    assert_eq!(texts(&first.body, "NextMarker"), ["a/"]);
    assert_eq!(texts(&first.body, "Delimiter"), ["/"]);
    let second = v1(&rig, "delimiter=%2F&max-keys=1&marker=a%2F").await;
    assert_eq!(prefixes(&second), ["b/"]);
    assert_eq!(texts(&second.body, "NextMarker"), ["b/"]);
    let third = v1(&rig, "delimiter=%2F&max-keys=1&marker=b%2F").await;
    assert_eq!(texts(&third.body, "Key"), ["c"]);
    assert_eq!(texts(&third.body, "IsTruncated"), ["false"]);
    assert!(texts(&third.body, "NextMarker").is_empty());

    let objects = rig_with(&["p", "q", "r"]).await;
    let page = v1(&objects, "delimiter=%2F&max-keys=2").await;
    assert_eq!(texts(&page.body, "Key"), ["p", "q"]);
    assert_eq!(texts(&page.body, "NextMarker"), ["q"]);
}

/// Asserts that a marker inside a common prefix passes the whole prefix, as `start-after` does
/// for version 2: with the marker `a/1` and the delimiter `/` the prefix `a/` is not listed (it
/// does not sort after the marker) and neither is `a/2`, but `b/` is.
#[tokio::test(start_paused = true)]
async fn a_marker_inside_a_common_prefix_passes_the_prefix() {
    let rig = rig_with(&["a/1", "a/2", "b/1"]).await;
    let reply = v1(&rig, "delimiter=%2F&marker=a%2F1").await;
    assert_eq!(prefixes(&reply), ["b/"]);
    assert!(texts(&reply.body, "Key").is_empty());
}

/// Asserts that `encoding-type=url` percent-encodes `Marker`, `NextMarker`, `Prefix`,
/// `Delimiter` and the keys, and says `EncodingType` `url`; that any other encoding type is 400
/// `InvalidArgument`.
#[tokio::test(start_paused = true)]
async fn encoding_type_url_encodes_the_names_of_a_version_1_answer() {
    let rig = rig_with(&["d%20d/1", "d%20d/2", "e%20e"]).await;
    let query = "encoding-type=url&delimiter=%2F&max-keys=1&marker=a%20b";
    let reply = v1(&rig, query).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(texts(&reply.body, "Marker"), ["a+b"]);
    assert_eq!(prefixes(&reply), ["d+d/"]);
    assert_eq!(texts(&reply.body, "NextMarker"), ["d+d/"]);
    assert_eq!(texts(&reply.body, "EncodingType"), ["url"]);
    let under = v1(&rig, "encoding-type=url&prefix=d%20d%2F").await;
    assert_eq!(texts(&under.body, "Prefix"), ["d+d/"]);
    assert_eq!(texts(&under.body, "Key"), ["d+d/1", "d+d/2"]);
    let bad = v1(&rig, "encoding-type=base64").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(texts(&bad.body, "Code"), ["InvalidArgument"]);
}

/// Asserts that `max-keys=0` is an empty, untruncated page (no `NextMarker`) and that a size
/// that is not a number is 400 `InvalidArgument`, as for version 2.
#[tokio::test(start_paused = true)]
async fn the_page_size_of_a_version_1_listing() {
    let rig = rig_with(&["a", "b"]).await;
    let zero = v1(&rig, "max-keys=0").await;
    assert!(texts(&zero.body, "Key").is_empty());
    assert_eq!(texts(&zero.body, "MaxKeys"), ["0"]);
    assert_eq!(texts(&zero.body, "IsTruncated"), ["false"]);
    let bad = v1(&rig, "max-keys=x").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(texts(&bad.body, "Code"), ["InvalidArgument"]);
}

/// Asserts that `prefix` narrows a version 1 listing and is echoed.
#[tokio::test(start_paused = true)]
async fn the_prefix_narrows_a_version_1_listing() {
    let rig = rig_with(&["a/1", "b/1"]).await;
    let reply = v1(&rig, "prefix=b%2F").await;
    assert_eq!(texts(&reply.body, "Key"), ["b/1"]);
    assert_eq!(texts(&reply.body, "Prefix"), ["b/"]);
}
