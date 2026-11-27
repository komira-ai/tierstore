//! The refusals of the checks of a fill, a span and a put, each by its words.

use std::path::Path;

use super::{Refusal, check_blob, check_put};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Asserts that a blob is refused for a digest that is not one (too short, upper case, not hex),
/// for no bytes, for an end past 2^64, for an empty key, a key one byte longer than an S3 key and
/// a reserved key; and that a blob that ends at the last byte 64 bits count, under a key of 1024
/// bytes, passes.
#[test]
fn a_blob_is_refused_by_the_words_of_each_check() {
    let upper = SHA.to_uppercase();
    let long = "k".repeat(1025);
    let longest = "k".repeat(1024);
    let not_hex = "x".repeat(64);
    let refused = [
        ((&SHA[1..], 1, "packs", 0), Refusal::Digest),
        ((upper.as_str(), 1, "packs", 0), Refusal::Digest),
        ((not_hex.as_str(), 1, "packs", 0), Refusal::Digest),
        ((SHA, 0, "packs", 0), Refusal::Empty),
        ((SHA, 2, "packs", u64::MAX), Refusal::Overflow),
        ((SHA, 1, "", 0), Refusal::Key),
        ((SHA, 1, long.as_str(), 0), Refusal::Key),
        ((SHA, 1, "_journal/x", 0), Refusal::Reserved),
    ];
    for ((sha256, size, from, offset), why) in refused {
        assert_eq!(
            check_blob((sha256, size), (from, offset)),
            Err(why),
            "{sha256} {size} {from} {offset}"
        );
    }
    assert_eq!(
        check_blob((SHA, 1), (longest.as_str(), u64::MAX - 1)),
        Ok(())
    );
    assert_eq!(Refusal::Overflow.to_string(), "offset and size pass 2^64");
}

/// Asserts that a put is refused for a key that is not usable or is reserved, and for a path that
/// is not absolute, with the words of each; and that an absolute path under a usable key passes.
#[test]
fn a_put_is_refused_for_its_key_and_for_a_relative_path() {
    let staged = Path::new("/stage/one");
    assert_eq!(check_put("", staged), Err(Refusal::Key));
    assert_eq!(check_put("_journal/x", staged), Err(Refusal::Reserved));
    assert_eq!(check_put("cas/b/k", Path::new("rel")), Err(Refusal::Path));
    assert_eq!(check_put("cas/b/k", staged), Ok(()));
    let words: Vec<String> = [
        Refusal::Digest,
        Refusal::Empty,
        Refusal::Key,
        Refusal::Reserved,
        Refusal::Path,
        Refusal::Placement,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        words,
        [
            "not a sha256 digest",
            "size is zero",
            "not a usable key",
            "reserved key",
            "not an absolute path",
            "not a placement id",
        ]
    );
}
