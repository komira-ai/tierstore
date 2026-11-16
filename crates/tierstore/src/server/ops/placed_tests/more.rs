//! More of the S3 front under a placement: the reading of a cache file a chunk at a time, the
//! escaping of a link's target, the `response-*` overrides and the date conditions, the paging of
//! a delimited listing, and the time a listing shows.

use futures::StreamExt as _;
use hyper::StatusCode;

use super::super::placed::{CHUNK, span_body};
use super::{Fixture, ROOT, at, link, raw, texts};
use crate::server::time::{http_date, iso8601};

/// `len` bytes of a pattern that does not repeat within a chunk's alignment.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// The bytes of a body, or the error that ended it, piece by piece.
async fn pieces(body: crate::ReadBody) -> Vec<Result<Vec<u8>, String>> {
    body.map(|piece| {
        piece
            .map(|bytes| bytes.to_vec())
            .map_err(|why| why.to_string())
    })
    .collect()
    .await
}

/// Asserts that a body read from a cache file of more than two chunks gives exactly the bytes of
/// the window asked for, in pieces of at most a chunk: the whole file, and a window that crosses a
/// chunk's edge.
#[tokio::test]
async fn a_body_across_chunks_gives_exactly_its_window() {
    let fx = Fixture::new("placed-chunks").await;
    let id = fx.placed();
    let chunk = usize::try_from(CHUNK).unwrap();
    let bytes = pattern(2 * chunk + 5);
    let path = fx.cache.config.path.join("big");
    std::fs::write(&path, &bytes).unwrap();
    for (offset, len) in [(0, bytes.len()), (chunk - 3, 10), (1, 2 * chunk)] {
        let hold = fx.store.open_read(&id).unwrap();
        let got = pieces(span_body(&path, offset as u64, len as u64, hold).unwrap()).await;
        assert!(
            got.iter()
                .all(|piece| piece.as_ref().is_ok_and(|p| p.len() <= chunk))
        );
        let joined: Vec<u8> = got.into_iter().flat_map(Result::unwrap).collect();
        assert_eq!(joined, &bytes[offset..offset + len], "{offset}+{len}");
    }
}

/// Asserts that a body read from a cache file shorter than its window gives the whole chunks the
/// file holds and then ends with the error of the chunk it could not read, so the answer is cut
/// and never completed with other bytes.
#[tokio::test]
async fn a_short_cache_file_gives_its_whole_chunks_and_then_an_error() {
    let fx = Fixture::new("placed-short-chunks").await;
    let id = fx.placed();
    let chunk = usize::try_from(CHUNK).unwrap();
    let bytes = pattern(chunk + 10);
    let path = fx.cache.config.path.join("short");
    std::fs::write(&path, &bytes).unwrap();
    let hold = fx.store.open_read(&id).unwrap();
    let got = pieces(span_body(&path, 0, CHUNK + 100, hold).unwrap()).await;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].as_deref(), Ok(&bytes[..chunk]));
    let why = got[1].clone().unwrap_err();
    assert!(why.contains("reading a tree file: "), "{why}");
}

/// Asserts that every byte of a link's target that is not printable ASCII (a tab, a newline,
/// DEL, a control character), and `%`, is written `%XX` in `x-amz-meta-symlink-target`, and that
/// the printable bytes at both ends of the range (space and `~`) are kept; on GET and HEAD.
#[tokio::test]
async fn a_links_target_is_escaped_at_the_edges_of_printable_ascii() {
    let fx = Fixture::new("placed-escape").await;
    let mut entries = fx.entries();
    entries.push(link("ctl", "a\tb\nc ~!%\u{7f}\u{1f}"));
    fx.place("jobs/3/", entries);
    for method in ["GET", "HEAD"] {
        let reply = raw(&fx.rig, method, "jobs/3/ctl", &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{method}");
        let target = reply.header("x-amz-meta-symlink-target");
        assert_eq!(target, "a%09b%0Ac ~!%25%7F%1F", "{method}");
    }
}

/// Asserts that the `response-*` parameters set their headers on a GET and a HEAD of a file under
/// a placement, and that a value that is not a header value is `400 InvalidArgument`.
#[tokio::test]
async fn the_response_overrides_set_the_headers_of_a_placed_file() {
    let fx = Fixture::new("placed-overrides").await;
    fx.placed();
    let key =
        at("bin/tool?response-content-type=text%2Fplain&response-content-disposition=attachment");
    for method in ["GET", "HEAD"] {
        let reply = raw(&fx.rig, method, &key, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{method}");
        assert_eq!(reply.header("content-type"), "text/plain", "{method}");
        assert_eq!(
            reply.header("content-disposition"),
            "attachment",
            "{method}"
        );
    }
    let bad = raw(
        &fx.rig,
        "GET",
        &at("bin/tool?response-content-type=a%0Ab"),
        &[],
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.code(), "InvalidArgument");
}

/// Asserts that the date conditions are judged on the time the placement was made: a GET
/// `If-Modified-Since` that time is `304`, one a second before it is `200`; and
/// `If-Unmodified-Since` a second before it is `412`, that time itself `200`.
#[tokio::test]
async fn the_date_conditions_are_judged_on_the_time_of_the_placement() {
    let fx = Fixture::new("placed-dates").await;
    // A time with a second before it, on the cache's clock.
    fx.cache.clock.advance(1_000_000);
    let id = fx.placed();
    let made = fx.made(&id);
    let (now, before) = (http_date(made), http_date(made - 1000));
    let key = at("bin/other");
    let cases = [
        ("if-modified-since", &now, StatusCode::NOT_MODIFIED),
        ("if-modified-since", &before, StatusCode::OK),
        (
            "if-unmodified-since",
            &before,
            StatusCode::PRECONDITION_FAILED,
        ),
        ("if-unmodified-since", &now, StatusCode::OK),
    ];
    for (name, date, status) in cases {
        let reply = raw(&fx.rig, "GET", &key, &[(name, date.as_str())]).await;
        assert_eq!(reply.status, status, "{name}: {date}");
    }
}

/// The keys and common prefixes of every page of a ListObjectsV2 of `query` of one entry each,
/// in order, and how many pages it took.
async fn walk_entries(fx: &Fixture, query: &str) -> (Vec<String>, usize) {
    let mut entries = Vec::new();
    let mut token = String::new();
    let mut pages = 0;
    loop {
        let mut asked = format!("{query}&max-keys=1");
        if !token.is_empty() {
            asked.push_str(&format!("&continuation-token={token}"));
        }
        let reply = fx.rig.list(&asked).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        pages += 1;
        entries.extend(texts(&reply.body, "Key"));
        entries.extend(super::common_prefixes(&reply.body));
        token = texts(&reply.body, "NextContinuationToken").concat();
        if token.is_empty() {
            return (entries, pages);
        }
        assert!(pages < 50, "the walk does not end");
    }
}

/// Asserts that a delimited listing pages over the merged keys one entry at a time with each
/// directory of the tree and each common prefix of the ordinary objects given once, in key
/// order; and that a prefix that is the placement's without its `/` folds the placement into one
/// common prefix.
#[tokio::test]
async fn a_delimited_listing_pages_over_the_merged_keys() {
    let fx = Fixture::new("placed-delimited").await;
    for key in ["jobs/0/a", "jobs/9/z"] {
        assert_eq!(fx.rig.put(key, b"x").await.status, StatusCode::OK);
    }
    fx.placed();
    let inside = walk_entries(&fx, &format!("prefix={ROOT}&delimiter=/")).await;
    let expected = ["bin/", "lib/", "link", "zero"].map(at).to_vec();
    assert_eq!(inside, (expected, 4));
    let above = walk_entries(&fx, "prefix=jobs/&delimiter=/").await;
    let expected = ["jobs/0/", "jobs/1/", "jobs/9/"]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(above, (expected, 3));
    let reply = fx.rig.list("prefix=jobs/1/root&delimiter=/").await;
    assert_eq!(super::common_prefixes(&reply.body), vec![ROOT.to_owned()]);
    assert!(texts(&reply.body, "Key").is_empty());
}

/// Asserts that every key a placement shows in a listing has the time the placement was made as
/// its `LastModified`, the time a GET of it gives as `Last-Modified`.
#[tokio::test]
async fn a_listing_shows_the_time_of_the_placement() {
    let fx = Fixture::new("placed-listed-time").await;
    let id = fx.placed();
    let made = fx.made(&id);
    let reply = fx.rig.list(&format!("prefix={ROOT}")).await;
    let times = texts(&reply.body, "LastModified");
    assert_eq!(times, vec![iso8601(made); 5]);
    let got = raw(&fx.rig, "HEAD", &at("bin/tool"), &[]).await;
    assert_eq!(got.header("last-modified"), http_date(made));
}
