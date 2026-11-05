//! `delimiter` and `encoding-type=url` of ListObjectsV2, through `Server::handle`, over a proxy
//! whose tiers are in memory.

use bytes::Bytes;
use hyper::StatusCode;

use crate::Backend;

use super::testing::{Reply, Rig, texts};

async fn rig_with(keys: &[&str]) -> Rig {
    let rig = Rig::new(1 << 20).await;
    for key in keys {
        assert_eq!(rig.put(key, b"x").await.status, StatusCode::OK, "{key}");
    }
    rig
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

fn keys(reply: &Reply) -> Vec<String> {
    texts(&reply.body, "Key")
}

fn truncated(reply: &Reply) -> bool {
    texts(&reply.body, "IsTruncated") == ["true"]
}

fn token(reply: &Reply) -> String {
    let tokens = texts(&reply.body, "NextContinuationToken");
    assert_eq!(tokens.len(), 1, "{}", reply.body);
    tokens[0].clone()
}

/// Asserts that keys with the delimiter after the prefix fold into one common prefix each and
/// the rest are listed as they are: `a/1`, `a/2`, `b/1/x` and `c` under `/` give the object `c`
/// and the prefixes `a/` and `b/`, `KeyCount` 3, not truncated, with the delimiter echoed.
#[tokio::test(start_paused = true)]
async fn keys_with_the_delimiter_fold_into_common_prefixes() {
    let rig = rig_with(&["a/1", "a/2", "b/1/x", "c"]).await;
    let reply = rig.list("delimiter=%2F").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(keys(&reply), ["c"]);
    assert_eq!(prefixes(&reply), ["a/", "b/"]);
    assert_eq!(texts(&reply.body, "KeyCount"), ["3"]);
    assert_eq!(texts(&reply.body, "Delimiter"), ["/"]);
    assert!(!truncated(&reply));
    assert!(texts(&reply.body, "NextContinuationToken").is_empty());
}

/// Asserts that the prefix of the request is where the folding starts: under `b/` the key
/// `b/1/x` folds to `b/1/` and `b/2` stays an object, and a key that is the prefix itself
/// (`b/`, a folder marker) is an object, since nothing follows the prefix in it.
#[tokio::test(start_paused = true)]
async fn the_folding_starts_after_the_prefix_of_the_request() {
    let rig = rig_with(&["b/", "b/1/x", "b/1/y", "b/2", "c/1"]).await;
    let reply = rig.list("prefix=b%2F&delimiter=%2F").await;
    assert_eq!(keys(&reply), ["b/", "b/2"]);
    assert_eq!(prefixes(&reply), ["b/1/"]);
    assert_eq!(texts(&reply.body, "Prefix")[0], "b/");
    assert_eq!(texts(&reply.body, "KeyCount"), ["3"]);
}

/// Asserts the boundary where the prefix ends at the delimiter: with `prefix=a` the key `a/1`
/// folds to `a/`, and with `prefix=a/` the same key is an object; a prefix that no key has
/// gives an empty page with `KeyCount` 0.
#[tokio::test(start_paused = true)]
async fn a_prefix_that_ends_at_the_delimiter_and_one_that_matches_nothing() {
    let rig = rig_with(&["a/1", "ab"]).await;
    let before = rig.list("prefix=a&delimiter=%2F").await;
    assert_eq!(prefixes(&before), ["a/"]);
    assert_eq!(keys(&before), ["ab"]);
    let at = rig.list("prefix=a%2F&delimiter=%2F").await;
    assert_eq!(keys(&at), ["a/1"]);
    assert!(prefixes(&at).is_empty());
    let none = rig.list("prefix=zzz&delimiter=%2F").await;
    assert_eq!(texts(&none.body, "KeyCount"), ["0"]);
    assert!(keys(&none).is_empty());
    assert!(prefixes(&none).is_empty());
    assert!(!truncated(&none));
}

/// Asserts that a delimiter of several characters folds at its first whole occurrence, and the
/// prefix it makes ends with all of it: `x--1` and `x--2` fold to `x--`, `x-3` has no `--` and is
/// an object, and a delimiter that is not ASCII works too.
#[tokio::test(start_paused = true)]
async fn a_delimiter_of_several_characters_folds_at_its_whole_text() {
    let rig = rig_with(&["x--1", "x--2", "x-3", "y%C3%A9z", "y%C3%A9w", "yq"]).await;
    let reply = rig.list("delimiter=--").await;
    assert_eq!(prefixes(&reply), ["x--"]);
    assert_eq!(keys(&reply), ["x-3", "yq", "y\u{e9}w", "y\u{e9}z"]);
    let accented = rig.list("delimiter=%C3%A9").await;
    assert_eq!(prefixes(&accented), ["y\u{e9}"]);
    assert_eq!(keys(&accented), ["x--1", "x--2", "x-3", "yq"]);
}

/// Asserts that prefixes count against `max-keys` and the pages stop and start between whole
/// prefixes: with `max-keys=1` over `a/1`, `a/2`, `b/1`, `c` the pages are the prefix `a/`, the
/// prefix `b/` and the object `c`, the first two truncated with a token and the last not, each
/// with `KeyCount` 1, and the token of a prefix does not give the prefix again.
#[tokio::test(start_paused = true)]
async fn max_keys_pages_across_common_prefixes() {
    let rig = rig_with(&["a/1", "a/2", "b/1", "c"]).await;
    let first = rig.list("delimiter=%2F&max-keys=1").await;
    assert_eq!(prefixes(&first), ["a/"]);
    assert!(keys(&first).is_empty());
    assert!(truncated(&first));
    assert_eq!(texts(&first.body, "KeyCount"), ["1"]);
    let query = format!(
        "delimiter=%2F&max-keys=1&continuation-token={}",
        token(&first)
    );
    let second = rig.list(&query).await;
    assert_eq!(prefixes(&second), ["b/"]);
    assert!(truncated(&second));
    assert_eq!(texts(&second.body, "ContinuationToken"), [token(&first)]);
    let query = format!(
        "delimiter=%2F&max-keys=1&continuation-token={}",
        token(&second)
    );
    let third = rig.list(&query).await;
    assert_eq!(keys(&third), ["c"]);
    assert!(prefixes(&third).is_empty());
    assert!(!truncated(&third));
    assert_eq!(texts(&third.body, "KeyCount"), ["1"]);
}

/// Asserts that a page of exactly `max-keys` entries that ends the listing is not truncated, and
/// that one entry more is, whichever kind the entries are: two prefixes and `max-keys=2` is
/// complete, three is truncated after the second.
#[tokio::test(start_paused = true)]
async fn truncation_counts_prefixes_and_objects_alike() {
    let rig = rig_with(&["a/1", "a/2", "b/1"]).await;
    let exact = rig.list("delimiter=%2F&max-keys=2").await;
    assert_eq!(prefixes(&exact), ["a/", "b/"]);
    assert!(!truncated(&exact));
    let rig = rig_with(&["a/1", "b/1", "c/1", "d"]).await;
    let cut = rig.list("delimiter=%2F&max-keys=2").await;
    assert_eq!(prefixes(&cut), ["a/", "b/"]);
    assert!(truncated(&cut));
    let query = format!(
        "delimiter=%2F&max-keys=2&continuation-token={}",
        token(&cut)
    );
    let rest = rig.list(&query).await;
    assert_eq!(prefixes(&rest), ["c/"]);
    assert_eq!(keys(&rest), ["d"]);
    assert!(!truncated(&rest));
}

/// Asserts that the token of an object keeps the form it had before common prefixes (the
/// hexadecimal of the key), and that a page that ends with an object resumes after the object.
#[tokio::test(start_paused = true)]
async fn the_token_of_an_object_is_the_hexadecimal_of_its_key() {
    let rig = rig_with(&["a", "b/1", "c"]).await;
    let first = rig.list("delimiter=%2F&max-keys=1").await;
    assert_eq!(keys(&first), ["a"]);
    assert_eq!(token(&first), hex::encode("a"));
    let query = format!("delimiter=%2F&continuation-token={}", hex::encode("a"));
    let rest = rig.list(&query).await;
    assert_eq!(prefixes(&rest), ["b/"]);
    assert_eq!(keys(&rest), ["c"]);
}

/// Asserts that a continuation token past a prefix that holds more keys than one read of the
/// backend (1001) skips all of them: `a/0000`..`a/1099` and `b` with `max-keys=1` give the prefix
/// `a/` and then, with its token, the object `b` and not `a/` again. It catches a skip that
/// stops at the first read.
#[tokio::test(start_paused = true)]
async fn a_token_past_a_prefix_skips_every_key_of_a_large_prefix() {
    let rig = Rig::new(1 << 20).await;
    for number in 0..1100 {
        let key = format!("a/{number:04}");
        rig.hot_mem
            .put(&key, Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    rig.hot_mem
        .put("b", Bytes::from_static(b"x"))
        .await
        .unwrap();
    let first = rig.list("delimiter=%2F&max-keys=1").await;
    assert_eq!(prefixes(&first), ["a/"]);
    assert!(truncated(&first));
    let query = format!(
        "delimiter=%2F&max-keys=1&continuation-token={}",
        token(&first)
    );
    let second = rig.list(&query).await;
    assert!(prefixes(&second).is_empty());
    assert_eq!(keys(&second), ["b"]);
    assert!(!truncated(&second));
}

/// Asserts that `start-after` is only a lower bound on keys, as for S3: after `a/1` the key `a/2`
/// is still listed and folds to the prefix `a/` again, the start key is echoed in `StartAfter`,
/// and after `a/2` only `b` is left.
#[tokio::test(start_paused = true)]
async fn start_after_inside_a_prefix_lists_the_prefix_again() {
    let rig = rig_with(&["a/1", "a/2", "b"]).await;
    let reply = rig.list("delimiter=%2F&start-after=a%2F1").await;
    assert_eq!(prefixes(&reply), ["a/"]);
    assert_eq!(keys(&reply), ["b"]);
    assert_eq!(texts(&reply.body, "StartAfter"), ["a/1"]);
    let past = rig.list("delimiter=%2F&start-after=a%2F2").await;
    assert!(prefixes(&past).is_empty());
    assert_eq!(keys(&past), ["b"]);
}

/// Asserts that a continuation token of the common-prefix kind that is not one the server gave
/// is `InvalidArgument`: not hexadecimal, hexadecimal of bytes that are not text, and the mark
/// alone, which stands for no prefix.
#[tokio::test(start_paused = true)]
async fn a_bad_token_of_a_prefix_is_refused() {
    let rig = rig_with(&["a/1"]).await;
    for token in ["pzz", "pff", "p"] {
        let reply = rig
            .list(&format!("delimiter=%2F&continuation-token={token}"))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{token}");
        assert_eq!(reply.code(), "InvalidArgument", "{token}");
    }
}

/// Asserts that a listing over both tiers folds a prefix that both hold into one entry.
#[tokio::test(start_paused = true)]
async fn a_prefix_in_both_tiers_is_one_entry() {
    let rig = rig_with(&["a/1"]).await;
    rig.cold_mem
        .put("a/2", Bytes::from_static(b"c"))
        .await
        .unwrap();
    let reply = rig.list("delimiter=%2F").await;
    assert_eq!(prefixes(&reply), ["a/"]);
    assert_eq!(texts(&reply.body, "KeyCount"), ["1"]);
}

/// Asserts what `encoding-type=url` does: keys, the prefix, the delimiter, the start key and the
/// common prefixes are percent-encoded (a space is `+`, `/` stays, non-ASCII text and `&` are
/// `%` escapes), `EncodingType` is `url`, and the tokens are not touched. Without it, or with it
/// empty, the same listing is not encoded.
#[tokio::test(start_paused = true)]
async fn encoding_type_url_encodes_the_names_of_a_listing() {
    let rig = rig_with(&["d%20e/caf%C3%A9", "d%20e/g%2Bh", "d%20e%26x"]).await;
    let query = "encoding-type=url&delimiter=%20&prefix=d%20e&start-after=a%26b&max-keys=1";
    let reply = rig.list(query).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(texts(&reply.body, "EncodingType"), ["url"]);
    assert_eq!(texts(&reply.body, "Delimiter"), ["+"]);
    assert_eq!(texts(&reply.body, "StartAfter"), ["a%26b"]);
    assert_eq!(texts(&reply.body, "Prefix")[0], "d+e");
    let all = rig.list("encoding-type=url&prefix=d%20e%2F").await;
    assert_eq!(keys(&all), ["d+e/caf%C3%A9", "d+e/g%2Bh"]);
    let folded = rig
        .list("encoding-type=url&delimiter=%2F&prefix=d%20e")
        .await;
    assert_eq!(prefixes(&folded), ["d+e/"]);
    assert_eq!(keys(&folded), ["d+e%26x"]);
    assert_eq!(texts(&folded.body, "Prefix")[0], "d+e");
    let plain = rig.list("prefix=d%20e%2F").await;
    assert_eq!(keys(&plain), ["d e/caf\u{e9}", "d e/g+h"]);
    assert!(texts(&plain.body, "EncodingType").is_empty());
    let empty = rig.list("encoding-type=&prefix=d%20e%2F").await;
    assert_eq!(keys(&empty), keys(&plain));
    let paged = rig
        .list("encoding-type=url&max-keys=1&prefix=d%20e%2F")
        .await;
    assert_eq!(
        texts(&paged.body, "NextContinuationToken"),
        [hex::encode("d e/caf\u{e9}")]
    );
}

/// Asserts that a key with a character XML 1.0 forbids, which a put refuses but a store may hold
/// (put here straight into the tier), is listed with `encoding-type=url` as `%01` in well-formed
/// XML.
#[tokio::test(start_paused = true)]
async fn encoding_type_url_lists_a_key_xml_cannot_carry() {
    let rig = rig_with(&["a"]).await;
    rig.cold_mem
        .put("b\u{1}c", Bytes::from_static(b"x"))
        .await
        .unwrap();
    let reply = rig.list("encoding-type=url").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(keys(&reply), ["a", "b%01c"]);
}

/// Asserts that an `encoding-type` S3 does not know is `InvalidArgument`, as it is for S3.
#[tokio::test(start_paused = true)]
async fn an_unknown_encoding_type_is_refused() {
    let rig = rig_with(&["a"]).await;
    for value in ["base64", "URL"] {
        let reply = rig.list(&format!("encoding-type={value}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(reply.code(), "InvalidArgument", "{value}");
    }
}
