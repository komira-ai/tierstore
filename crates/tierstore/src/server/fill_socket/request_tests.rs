//! The lines of the fill socket: what each form parses to, and every refusal.

use std::path::PathBuf;

use super::{MountRequest, Refusal, Request, parse};
use crate::TreeDigest;
use crate::server::local_cache::attaches::{Attach, Fetches, Levels, Outcome};
use crate::server::local_cache::descriptors::Descriptors;
use crate::server::local_cache::opens::{Opens, Spans};
use crate::server::local_cache::tree_lists::{Totals, TreeLists};
use crate::server::local_cache::trees::{PlacementId, placement_id};

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

const PLACEMENT: &str = "0123456789abcdef0123456789abcdef";

fn id() -> PlacementId {
    placement_id(PLACEMENT).unwrap()
}

/// Asserts that a place names the mount and the path, that a path with spaces in it is kept whole,
/// and that a path of one name or of several is taken.
#[test]
fn a_place_names_the_mount_and_the_path() {
    let place = |mount: &str, path: &str| {
        Ok(Request::Mount(MountRequest::Place {
            mount: mount.to_owned(),
            path: path.to_owned(),
        }))
    };
    assert_eq!(
        line("place boot-1 jobs/1/root"),
        place("boot-1", "jobs/1/root")
    );
    assert_eq!(line("place boot-1 jobs"), place("boot-1", "jobs"));
    assert_eq!(line("place boot-1 a b/c  d"), place("boot-1", "a b/c  d"));
    assert_eq!(line("place boot-1 treesx/a"), place("boot-1", "treesx/a"));
}

/// Asserts every refusal of a place: no path, a mount that is not a name, and a path that is
/// empty, begins with a name of the mount's own, has an empty name, a dot name, or a slash at
/// either end.
#[test]
fn a_place_that_is_not_one_is_refused_with_its_reason() {
    let cases = [
        ("place", Refusal::Malformed),
        ("place boot-1", Refusal::Malformed),
        ("place  jobs/1", Refusal::Name),
        ("place bo\u{e9} jobs/1", Refusal::Name),
        ("place boot-1 ", Refusal::MountPath),
        ("place boot-1 trees/x", Refusal::MountPath),
        ("place boot-1 views", Refusal::MountPath),
        ("place boot-1 put/a", Refusal::MountPath),
        ("place boot-1 a//b", Refusal::MountPath),
        ("place boot-1 /a", Refusal::MountPath),
        ("place boot-1 a/", Refusal::MountPath),
        ("place boot-1 a/../b", Refusal::MountPath),
        ("place boot-1 a/./b", Refusal::MountPath),
    ];
    for (text, why) in cases {
        assert_eq!(line(text), Err(why), "{text:?}");
    }
    let long = format!("place {} jobs/1", "m".repeat(129));
    assert_eq!(line(&long), Err(Refusal::Name));
}

/// Asserts that mounted is the request with no words after it, and that anything after it, a space
/// alone included, or a longer word that begins with it, is not.
#[test]
fn mounted_takes_no_argument() {
    assert_eq!(line("mounted"), Ok(Request::Mount(MountRequest::Mounted)));
    for text in ["mounted ", "mounted x", "mounted boot-1"] {
        assert_eq!(line(text), Err(Refusal::Malformed), "{text:?}");
    }
    assert_eq!(line("mountedx"), Err(Refusal::Unknown));
}

/// Asserts that a flat names the placement, and refuses a missing, short, long, upper case or
/// non-hex id.
#[test]
fn a_flat_names_a_placement_and_refuses_what_is_not_one() {
    let want = Request::Mount(MountRequest::Flat { placement: id() });
    assert_eq!(line(&format!("flat {PLACEMENT}")), Ok(want));
    let bad = [
        "flat".to_owned(),
        "flat ".to_owned(),
        format!("flat {}", &PLACEMENT[1..]),
        format!("flat {PLACEMENT}0"),
        format!("flat {}", PLACEMENT.to_uppercase()),
        format!("flat {}g", &PLACEMENT[1..]),
        format!("flat {PLACEMENT} x"),
    ];
    for text in bad {
        assert_eq!(line(&text), Err(Refusal::Placement), "{text:?}");
    }
}

/// Asserts that an image names the placement, and refuses a missing, short, long, upper case or
/// non-hex id, and extra words.
#[test]
fn an_image_names_a_placement_and_refuses_what_is_not_one() {
    let want = Request::Mount(MountRequest::Image { placement: id() });
    assert_eq!(line(&format!("image {PLACEMENT}")), Ok(want));
    let bad = [
        "image".to_owned(),
        "image ".to_owned(),
        format!("image {}", &PLACEMENT[1..]),
        format!("image {PLACEMENT}0"),
        format!("image {}", PLACEMENT.to_uppercase()),
        format!("image {}g", &PLACEMENT[1..]),
        format!("image {PLACEMENT} x"),
    ];
    for text in bad {
        assert_eq!(line(&text), Err(Refusal::Placement), "{text:?}");
    }
}

/// Asserts that a span has the words of a fill of a blob and names the digest, the size, the pack
/// and the offset, that a pack key may hold spaces, and that it has the refusals of a fill: size
/// zero, an end past 64 bits, a bad digest, a reserved key, and missing words.
#[test]
fn a_span_has_the_words_and_the_refusals_of_a_blob_fill() {
    let want = Request::Mount(MountRequest::Span {
        sha256: TreeDigest::from_hex(SHA).unwrap(),
        size: 4096,
        pack: "cas/pack/a b".to_owned(),
        offset: 8192,
    });
    assert_eq!(
        line(&format!("span {SHA} 4096 from cas/pack/a b @ 8192")),
        Ok(want)
    );
    let upper = SHA.to_uppercase();
    let cases = [
        (format!("span {SHA} 0 from p @ 0"), Refusal::Empty),
        (
            format!("span {SHA} 2 from p @ {}", u64::MAX - 1),
            Refusal::Overflow,
        ),
        (format!("span {upper} 1 from p @ 0"), Refusal::Digest),
        ("span nonsense 1 from p @ 0".to_owned(), Refusal::Digest),
        (
            format!("span {SHA} 1 from _journal/x @ 0"),
            Refusal::Reserved,
        ),
        (format!("span {SHA} 1 from  @ 0"), Refusal::Key),
        (format!("span {SHA} x from p @ 0"), Refusal::Number),
        (format!("span {SHA} 1 from p @ x"), Refusal::Number),
        (format!("span {SHA} 1 from p"), Refusal::Malformed),
        (format!("span {SHA}"), Refusal::Malformed),
        ("span".to_owned(), Refusal::Malformed),
    ];
    for (text, why) in cases {
        assert_eq!(line(&text), Err(why), "{text}");
    }
}

/// Asserts that an unplace names the mount and the placement, and refuses a missing id, a mount
/// that is not a name, and an id that is not a placement.
#[test]
fn an_unplace_names_the_mount_and_the_placement() {
    let want = Request::Mount(MountRequest::Unplace {
        mount: "boot-1".to_owned(),
        placement: id(),
    });
    assert_eq!(line(&format!("unplace boot-1 {PLACEMENT}")), Ok(want));
    let cases = [
        ("unplace".to_owned(), Refusal::Malformed),
        ("unplace boot-1".to_owned(), Refusal::Malformed),
        (format!("unplace  {PLACEMENT}"), Refusal::Name),
        ("unplace boot-1 ".to_owned(), Refusal::Placement),
        (
            format!("unplace boot-1 {}", &PLACEMENT[1..]),
            Refusal::Placement,
        ),
        (
            format!("unplace boot-1 {}", PLACEMENT.to_uppercase()),
            Refusal::Placement,
        ),
        (format!("unplace boot-1 {PLACEMENT} x"), Refusal::Placement),
    ];
    for (text, why) in cases {
        assert_eq!(line(&text), Err(why), "{text:?}");
    }
}

/// Asserts the words the refusals of the mount's requests put after `refused `.
#[test]
fn the_refusals_of_the_mount_requests_say_what_was_wrong() {
    assert_eq!(Refusal::MountPath.to_string(), "not a path of the mount");
    assert_eq!(Refusal::Placement.to_string(), "not a placement id");
}

/// Asserts that `places-live` names the mount and the placements it keeps, none included, and
/// that a mount or a placement id that is not one is refused.
#[test]
fn places_live_names_the_mount_and_the_placements_it_keeps() {
    assert_eq!(
        line("places-live boot-1"),
        Ok(Request::Mount(MountRequest::PlacesLive {
            mount: "boot-1".to_owned(),
            keep: Vec::new(),
        }))
    );
    assert_eq!(
        line(&format!("places-live boot-1 {PLACEMENT} {PLACEMENT}")),
        Ok(Request::Mount(MountRequest::PlacesLive {
            mount: "boot-1".to_owned(),
            keep: vec![id(), id()],
        }))
    );
    assert_eq!(line("places-live"), Err(Refusal::Name));
    assert_eq!(line("places-live boot-1 nope"), Err(Refusal::Placement));
}

/// Asserts that a `started` names the path of the tree the mount first looked into, kept whole,
/// and that a path that is not one a place could name is refused as a place's is.
#[test]
fn a_started_names_the_path_and_refuses_what_a_place_refuses() {
    let started = |path: &str| {
        Ok(Request::Started {
            path: path.to_owned(),
        })
    };
    assert_eq!(
        line("started mount-provider/61/6a73/0"),
        started("mount-provider/61/6a73/0")
    );
    assert_eq!(line("started a b/c  d"), started("a b/c  d"));
    let cases = [
        "started",
        "started ",
        "started trees/x",
        "started a//b",
        "started /a",
        "started a/",
    ];
    for text in cases {
        assert_eq!(line(text), Err(Refusal::MountPath), "{text:?}");
    }
}

/// Asserts that an `attached` and a `fetched` are the reports they write, and that one that is
/// not is refused as malformed.
#[test]
fn an_attached_and_a_fetched_are_the_reports_they_write() {
    let attach = Attach {
        outcome: Outcome::Absent,
        ms: 12,
    };
    assert_eq!(line(&attach.line()), Ok(Request::Attached(attach)));
    let fetches = Fetches {
        cache: 3,
        filled: 2,
        fill_ms: 9,
        ..Fetches::default()
    };
    assert_eq!(line(&fetches.line()), Ok(Request::Fetched(fetches)));
    for text in ["attached", "attached gone 1", "fetched", "fetched 1 2 3 4"] {
        assert_eq!(line(text), Err(Refusal::Malformed), "{text:?}");
    }
}

/// Asserts that a `descriptors` is the report it writes, and that one that is not, a bad mount
/// name included, is refused as malformed.
#[test]
fn a_descriptors_is_the_report_it_writes() {
    let report = Descriptors {
        mount: "boot-1".to_owned(),
        process: 77,
        held: 12,
        short: 3,
    };
    assert_eq!(line(&report.line()), Ok(Request::Descriptors(report)));
    for text in [
        "descriptors",
        "descriptors boot-1 7 1",
        "descriptors boot-1 7 1 2 3",
        "descriptors boot-1 7 -1 2",
        "descriptors boot-1 7 1 x",
        "descriptors  7 1 2",
    ] {
        assert_eq!(line(text), Err(Refusal::Malformed), "{text:?}");
    }
}

/// Asserts that a `treelists` is the report it writes, and that one that is not, one word short or
/// with a bad number included, is refused as malformed.
#[test]
fn a_treelists_is_the_report_it_writes() {
    let report = TreeLists {
        mount: "boot-1".to_owned(),
        process: 77,
        mapped: 2,
        bytes: 8192,
        cap: 536_870_912,
        totals: Totals {
            cap: 1,
            idle: 3,
            over_cap: 0,
        },
    };
    assert_eq!(line(&report.line()), Ok(Request::TreeLists(report)));
    for text in [
        "treelists",
        "treelists boot-1 7 1 2 3 4 5",
        "treelists boot-1 7 1 2 3 4 5 x",
    ] {
        assert_eq!(line(text), Err(Refusal::Malformed), "{text:?}");
    }
}

/// Asserts that an `opens` is the report it writes, and that one that is not, one of another number
/// of buckets or with a bad mount name included, is refused as malformed.
#[test]
fn an_opens_is_the_report_it_writes() {
    let mut spans = [Spans::default(); 4];
    spans[1].ms = 30;
    spans[1].buckets[5] = 1;
    let report = Opens {
        mount: "boot-1".to_owned(),
        process: 77,
        waiting: 2,
        spans,
    };
    let written = report.line();
    assert_eq!(line(&written), Ok(Request::Opens(Box::new(report))));
    let fewer = written.rsplit_once(' ').unwrap().0.to_owned();
    let more = format!("{written} 0");
    let unnamed = written.replacen("boot-1", "", 1);
    for text in ["opens", "opens boot-1 7 1", &fewer, &more, &unnamed] {
        assert_eq!(line(text), Err(Refusal::Malformed), "{text:?}");
    }
}

/// Asserts that a `fill-tree` names the root manifest and the milliseconds it may take, the
/// largest number of them that fits 64 bits included, and that one without both, with a digest
/// that is not one, or with a time that is not a number, is refused for it.
#[test]
fn a_fill_tree_names_its_root_and_its_time() {
    let root = TreeDigest::from_hex(SHA).unwrap();
    assert_eq!(
        line(&format!("fill-tree {SHA} 30000")),
        Ok(Request::Tree { root, ms: 30000 })
    );
    assert_eq!(
        line(&format!("fill-tree {SHA} {}", u64::MAX)),
        Ok(Request::Tree { root, ms: u64::MAX })
    );
    let refused = [
        ("fill-tree".to_owned(), Refusal::Malformed),
        (format!("fill-tree {SHA}"), Refusal::Malformed),
        (
            format!("fill-tree {} 1", SHA.to_uppercase()),
            Refusal::Digest,
        ),
        ("fill-tree abc 1".to_owned(), Refusal::Digest),
        (format!("fill-tree {SHA} -1"), Refusal::Number),
        (format!("fill-tree {SHA} 1 2"), Refusal::Number),
        (format!("fill-tree {SHA} "), Refusal::Number),
        (
            format!("fill-tree {SHA} 18446744073709551616"),
            Refusal::Number,
        ),
    ];
    for (text, refusal) in refused {
        assert_eq!(line(&text), Err(refusal), "{text:?}");
    }
}

/// Asserts that a `levels` is the report it writes, and that one that is not is refused as
/// malformed.
#[test]
fn a_levels_is_the_report_it_writes() {
    let levels = Levels {
        levels: 9,
        rounds: 1,
    };
    assert_eq!(line(&levels.line()), Ok(Request::Levels(levels)));
    for text in ["levels", "levels 9", "levels 9 1 2", "levels x 1"] {
        assert_eq!(line(text), Err(Refusal::Malformed), "{text:?}");
    }
}
