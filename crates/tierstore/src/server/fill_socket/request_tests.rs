//! The lines of the fill socket: what each form parses to, and every refusal.

use std::path::PathBuf;

use super::{Refusal, Request, parse};
use crate::TreeDigest;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Parses a line given as text.
fn line(text: &str) -> Result<Request, Refusal> {
    parse(text.as_bytes())
}

/// Asserts that a fill of a blob names the digest, the size, the upstream key and the offset, that
/// a key may hold spaces and an `@` (the last ` @ ` ends it), and that the largest offset and size
/// that together fit 64 bits are taken.
#[test]
fn a_fill_of_a_blob_names_its_digest_size_source_and_offset() {
    let want = Request::Blob {
        sha256: SHA.to_owned(),
        size: 4096,
        from: "packs/ab/cd".to_owned(),
        offset: 8192,
    };
    assert_eq!(
        line(&format!("fill {SHA} 4096 from packs/ab/cd @ 8192")),
        Ok(want)
    );
    let spaced = line(&format!("fill {SHA} 1 from a b @ c @ 0")).unwrap();
    let Request::Blob { from, offset, .. } = spaced else {
        panic!("a blob");
    };
    assert_eq!((from.as_str(), offset), ("a b @ c", 0));
    let edge = format!("fill {SHA} {} from k @ 0", u64::MAX);
    assert!(line(&edge).is_ok());
    let edge = format!("fill {SHA} 1 from k @ {}", u64::MAX - 1);
    assert!(line(&edge).is_ok());
}

/// Asserts that a fill with the digest alone is the whole object `cas/l/<sha256>`.
#[test]
fn a_fill_with_a_digest_alone_is_a_whole_object() {
    let want = Request::Manifest {
        sha256: SHA.to_owned(),
    };
    assert_eq!(line(&format!("fill {SHA}")), Ok(want));
}

/// Asserts that a touch names digests, passes over what is not a digest, and is a touch of nothing
/// with none: it is never refused, since it is never answered.
#[test]
fn a_touch_keeps_the_digests_and_is_never_refused() {
    let want = Request::Touch(vec![SHA.to_owned(), OTHER.to_owned()]);
    assert_eq!(line(&format!("touch {SHA} {OTHER}")), Ok(want));
    let kept = Request::Touch(vec![OTHER.to_owned()]);
    assert_eq!(
        line(&format!("touch nonsense {OTHER}  {}", &SHA[1..])),
        Ok(kept)
    );
    assert_eq!(line("touch"), Ok(Request::Touch(Vec::new())));
    assert_eq!(line("touch "), Ok(Request::Touch(Vec::new())));
}

/// Asserts every refusal and its reason.
#[test]
fn every_line_that_is_not_a_request_is_refused_with_its_reason() {
    let upper = SHA.to_uppercase();
    let cases: Vec<(String, Refusal)> = vec![
        (String::new(), Refusal::Unknown),
        ("pin".into(), Refusal::Malformed),
        ("pin m".into(), Refusal::Malformed),
        ("pin m t".into(), Refusal::Malformed),
        (format!("pin m t {}", &SHA[1..]), Refusal::Digest),
        (format!("pin m t {SHA} x"), Refusal::Digest),
        ("pin m t ".into(), Refusal::Digest),
        (format!("pin  t {SHA}"), Refusal::Name),
        (format!("pin m  {SHA}"), Refusal::Name),
        (format!("pin {} t {SHA}", "m".repeat(129)), Refusal::Name),
        (format!("pin m t\u{e9} {SHA}"), Refusal::Name),
        ("unpin".into(), Refusal::Malformed),
        ("unpin m".into(), Refusal::Malformed),
        ("unpin m ".into(), Refusal::Name),
        ("unpin  t".into(), Refusal::Name),
        ("pins-live".into(), Refusal::Name),
        ("pins-live ".into(), Refusal::Name),
        (format!("pins-live m {}", "t".repeat(129)), Refusal::Name),
        ("FILL x".into(), Refusal::Unknown),
        ("fill".into(), Refusal::Digest),
        ("fill ".into(), Refusal::Digest),
        (format!("fill {upper}"), Refusal::Digest),
        (format!("fill {}", &SHA[1..]), Refusal::Digest),
        (format!("fill {SHA}x"), Refusal::Digest),
        (format!("fill {SHA} "), Refusal::Malformed),
        (format!("fill {SHA} 10"), Refusal::Malformed),
        (format!("fill {SHA} 10 packs @ 0"), Refusal::Malformed),
        (format!("fill {SHA} 10 from packs"), Refusal::Malformed),
        (format!("fill {SHA} 10 from packs @0"), Refusal::Malformed),
        (format!("fill {SHA} x from packs @ 0"), Refusal::Number),
        (format!("fill {SHA} -1 from packs @ 0"), Refusal::Number),
        (format!("fill {SHA} +1 from packs @ 0"), Refusal::Number),
        (format!("fill {SHA} 1 from packs @ x"), Refusal::Number),
        (format!("fill {SHA} 1 from packs @ "), Refusal::Number),
        (
            format!("fill {SHA} 18446744073709551616 from packs @ 0"),
            Refusal::Number,
        ),
        (format!("fill {SHA} 0 from packs @ 0"), Refusal::Empty),
        (
            format!("fill {SHA} 2 from packs @ {}", u64::MAX - 1),
            Refusal::Overflow,
        ),
        (format!("fill {SHA} 1 from  @ 0"), Refusal::Key),
        (
            format!("fill {SHA} 1 from {} @ 0", "k".repeat(1025)),
            Refusal::Key,
        ),
        (
            format!("fill {SHA} 1 from _tiering/lease @ 0"),
            Refusal::Reserved,
        ),
        (
            format!("fill {SHA} 1 from _journal/x @ 0"),
            Refusal::Reserved,
        ),
        (
            format!("fill {SHA} 1 from _entries/x @ 0"),
            Refusal::Reserved,
        ),
    ];
    for (text, why) in cases {
        assert_eq!(line(&text), Err(why), "{text:?}");
    }
    assert_eq!(parse(b"fill \xff"), Err(Refusal::NotText));
}

/// Asserts that a key of exactly the longest an S3 key may be is taken.
#[test]
fn a_key_of_the_longest_length_is_taken() {
    let key = "k".repeat(1024);
    assert!(line(&format!("fill {SHA} 1 from {key} @ 0")).is_ok());
}

/// Asserts the words a refusal puts after `refused `.
#[test]
fn a_refusal_says_in_words_what_was_wrong() {
    let words = [
        (Refusal::NotText, "the request is not text"),
        (Refusal::Unknown, "unknown request"),
        (Refusal::Malformed, "malformed request"),
        (Refusal::Digest, "not a sha256 digest"),
        (Refusal::Number, "not a number"),
        (Refusal::Empty, "size is zero"),
        (Refusal::Overflow, "offset and size pass 2^64"),
        (Refusal::Key, "not a usable key"),
        (Refusal::Reserved, "reserved key"),
        (Refusal::Name, "not a usable name"),
        (Refusal::Path, "not an absolute path"),
    ];
    for (refusal, text) in words {
        assert_eq!(refusal.to_string(), text);
    }
}

/// Asserts that a pin names the mount, the tree and every blob, that an unpin names the mount and
/// the tree, and that a pins-live names the mount and any number of trees, spaces between the
/// trees being passed over.
#[test]
fn the_pin_requests_name_the_mount_the_tree_and_the_blobs() {
    let pin = Request::Pin {
        mount: "boot-1".to_owned(),
        tree: SHA.to_owned(),
        blobs: vec![SHA.to_owned(), OTHER.to_owned()],
    };
    assert_eq!(line(&format!("pin boot-1 {SHA} {SHA} {OTHER}")), Ok(pin));
    let unpin = Request::Unpin {
        mount: "boot-1".to_owned(),
        tree: SHA.to_owned(),
    };
    assert_eq!(line(&format!("unpin boot-1 {SHA}")), Ok(unpin));
    let live = Request::PinsLive {
        mount: "boot-1".to_owned(),
        trees: vec![SHA.to_owned(), OTHER.to_owned()],
    };
    assert_eq!(line(&format!("pins-live boot-1 {SHA}  {OTHER}")), Ok(live));
    let none = Request::PinsLive {
        mount: "boot-1".to_owned(),
        trees: Vec::new(),
    };
    assert_eq!(line("pins-live boot-1"), Ok(none));
    let edge = "m".repeat(128);
    assert!(line(&format!("pins-live {edge}")).is_ok());
}

/// The `put` that `text` is, or why it is refused.
fn put_of(key: &str, size: u64, path: &str) -> Request {
    Request::Put {
        key: key.to_owned(),
        sha256: TreeDigest::from_hex(SHA).unwrap(),
        size,
        path: PathBuf::from(path),
    }
}

/// Asserts that a put names its key, digest, size and staged file; that the path is the rest of
/// the line, spaces and ` from ` included; and that a size of zero (an empty object) and the
/// largest size are taken.
#[test]
fn a_put_names_its_key_digest_size_and_staged_file() {
    let line_of = |rest: &str| line(&format!("put cas/b/1/n/pack {SHA} {rest}"));
    let want = put_of("cas/b/1/n/pack", 4096, "/stage/7");
    assert_eq!(line_of("4096 from /stage/7"), Ok(want));
    let spaced = put_of("cas/b/1/n/pack", 1, "/st age/a from b");
    assert_eq!(line_of("1 from /st age/a from b"), Ok(spaced));
    assert_eq!(line_of("0 from /s"), Ok(put_of("cas/b/1/n/pack", 0, "/s")));
    let largest = put_of("cas/b/1/n/pack", u64::MAX, "/s");
    assert_eq!(line_of(&format!("{} from /s", u64::MAX)), Ok(largest));
}

/// Asserts every refusal of a put: missing words, no `from`, a digest that is not one (upper case
/// included), a size that is not a number, a key that is empty, too long or reserved, and a path
/// that is not absolute or is empty.
#[test]
fn a_put_that_is_not_one_is_refused_with_its_reason() {
    let long = "k".repeat(1025);
    let upper = SHA.to_ascii_uppercase();
    let cases = [
        ("put".to_owned(), Refusal::Malformed),
        (format!("put k {SHA} 1"), Refusal::Malformed),
        (format!("put k {SHA} 1 to /s"), Refusal::Malformed),
        (format!("put k {upper} 1 from /s"), Refusal::Digest),
        ("put k nope 1 from /s".to_owned(), Refusal::Digest),
        (format!("put k {SHA} -1 from /s"), Refusal::Number),
        (format!("put  {SHA} 1 from /s"), Refusal::Key),
        (format!("put {long} {SHA} 1 from /s"), Refusal::Key),
        (format!("put _journal/x {SHA} 1 from /s"), Refusal::Reserved),
        (format!("put k {SHA} 1 from stage/7"), Refusal::Path),
        (format!("put k {SHA} 1 from "), Refusal::Path),
    ];
    for (text, refusal) in cases {
        assert_eq!(line(&text), Err(refusal), "{text}");
    }
    let edge = "k".repeat(1024);
    assert!(line(&format!("put {edge} {SHA} 1 from /s")).is_ok());
}
