//! The groups of a preparation's reads: which spans of a version 2 list are read together, where a
//! group is cut, which spans are read alone, and the reads of a version 1 list's files.

use std::sync::Arc;

use super::groups::{GROUP_MAX_BYTES, Group, alone, groups};
use crate::{BlobSource, BuiltFile, ListRange, ListSpan, TreeDigest};

/// The digest the tests give the span at `offset`: distinct for each offset.
fn digest_at(offset: u64) -> TreeDigest {
    TreeDigest::of(&offset.to_le_bytes())
}

/// The span of key number `key` at `offset` of `size` bytes.
fn span(key: u32, offset: u64, size: u64) -> ListSpan {
    ListSpan {
        key,
        offset,
        size,
        sha256: digest_at(offset),
    }
}

/// The range of key number `key` at `offset` of `len` bytes.
fn range(key: u32, offset: u64, len: u64) -> ListRange {
    ListRange { key, offset, len }
}

/// The part a group has for `span`.
fn part(offset: u64, size: u64) -> (u64, u64, TreeDigest) {
    (offset, size, digest_at(offset))
}

/// The keys `a` and `b`.
fn keys() -> Vec<Arc<str>> {
    vec![Arc::from("a"), Arc::from("b")]
}

/// The group of pack `pack` of `parts`.
fn group(pack: &str, parts: &[(u64, u64)]) -> Group {
    Group {
        pack: Arc::from(pack),
        parts: parts.iter().map(|(o, s)| part(*o, *s)).collect(),
    }
}

/// Asserts that spans inside one range are one group whose parts are in offset order, whatever the
/// order the spans came in.
#[test]
fn spans_of_one_range_are_one_group_in_offset_order() {
    let spans = vec![span(0, 20, 10), span(0, 0, 10), span(0, 10, 10)];
    let found = groups(&keys(), spans, [range(0, 0, 30)].into_iter());
    assert_eq!(found, [group("a", &[(0, 10), (10, 10), (20, 10)])]);
}

/// Asserts that a span of another key than the range's is not taken by it, and that the spans of
/// each key are grouped by the range of their own key.
#[test]
fn a_range_of_another_key_takes_no_span() {
    let alone_found = groups(
        &keys(),
        vec![span(0, 10, 10)],
        [range(1, 0, 100)].into_iter(),
    );
    assert_eq!(alone_found, [group("a", &[(10, 10)])]);

    let both = groups(
        &keys(),
        vec![
            span(1, 0, 10),
            span(0, 0, 10),
            span(1, 10, 10),
            span(0, 10, 10),
        ],
        [range(0, 0, 20), range(1, 0, 20)].into_iter(),
    );
    assert_eq!(
        both,
        [
            group("a", &[(0, 10), (10, 10)]),
            group("b", &[(0, 10), (10, 10)])
        ]
    );
}

/// Asserts that a span outside every range is a group of its own: one before the first range, one
/// between two ranges, one after the last, one that begins before a range and ends inside it, and
/// one that begins inside a range and ends past its end; the spans inside the ranges still group.
#[test]
fn a_span_outside_every_range_is_read_alone() {
    let ranges = || [range(0, 100, 100), range(0, 300, 100)].into_iter();
    let spans = vec![
        span(0, 0, 10),
        span(0, 90, 20),
        span(0, 120, 10),
        span(0, 130, 10),
        span(0, 190, 20),
        span(0, 220, 20),
        span(0, 320, 10),
        span(0, 330, 10),
        span(0, 500, 10),
    ];
    let found = groups(&keys(), spans, ranges());
    assert_eq!(
        found,
        [
            group("a", &[(0, 10)]),
            group("a", &[(90, 20)]),
            group("a", &[(120, 10), (130, 10)]),
            group("a", &[(190, 20)]),
            group("a", &[(220, 20)]),
            group("a", &[(320, 10), (330, 10)]),
            group("a", &[(500, 10)]),
        ]
    );
}

/// Asserts that a span outside a range does not close the group of the range it follows: spans
/// after it, inside that range again, are not joined to the group before it, and a span alone in a
/// range reads as a group of one.
#[test]
fn a_span_alone_between_the_spans_of_a_range_cuts_the_group() {
    let found = groups(
        &keys(),
        vec![
            span(0, 0, 10),
            span(0, 10, 10),
            span(0, 15, 10),
            span(0, 30, 5),
        ],
        [range(0, 0, 20), range(0, 30, 10)].into_iter(),
    );
    assert_eq!(
        found,
        [
            group("a", &[(0, 10), (10, 10)]),
            group("a", &[(15, 10)]),
            group("a", &[(30, 5)]),
        ]
    );
}

/// Asserts that the spans of two ranges of one key are two groups even when the ranges touch.
#[test]
fn two_ranges_of_one_key_are_two_groups() {
    let spans = vec![
        span(0, 0, 25),
        span(0, 25, 25),
        span(0, 50, 25),
        span(0, 75, 25),
    ];
    let found = groups(
        &keys(),
        spans,
        [range(0, 0, 50), range(0, 50, 50)].into_iter(),
    );
    assert_eq!(
        found,
        [
            group("a", &[(0, 25), (25, 25)]),
            group("a", &[(50, 25), (75, 25)]),
        ]
    );
}

/// Asserts that a group is cut where its bytes, from its first span's start to its last span's end,
/// would pass `GROUP_MAX_BYTES`: a span that ends exactly that far from the start joins, a span one
/// byte further begins a new group, and the new group is open to the spans after it.
#[test]
fn a_group_is_cut_at_the_most_bytes_it_reads() {
    let half = GROUP_MAX_BYTES / 2;
    let spans = vec![
        span(0, 0, half),
        span(0, half, half),
        span(0, GROUP_MAX_BYTES, 1),
        span(0, GROUP_MAX_BYTES + 1, 1),
    ];
    let found = groups(
        &keys(),
        spans,
        [range(0, 0, 3 * GROUP_MAX_BYTES)].into_iter(),
    );
    assert_eq!(
        found,
        [
            group("a", &[(0, half), (half, half)]),
            group("a", &[(GROUP_MAX_BYTES, 1), (GROUP_MAX_BYTES + 1, 1)]),
        ]
    );
}

/// Asserts that the ranges may be given in any order: each span is grouped by the range it is in.
#[test]
fn ranges_given_unsorted_group_the_same_spans() {
    let spans = vec![
        span(1, 0, 10),
        span(0, 100, 10),
        span(0, 0, 10),
        span(0, 110, 10),
        span(0, 10, 10),
    ];
    let ranges = [range(1, 0, 10), range(0, 100, 20), range(0, 0, 20)];
    let found = groups(&keys(), spans, ranges.into_iter());
    assert_eq!(
        found,
        [
            group("a", &[(0, 10), (10, 10)]),
            group("a", &[(100, 10), (110, 10)]),
            group("b", &[(0, 10)]),
        ]
    );
}

/// Asserts that with no ranges every span is a group of its own, in the order of the keys and the
/// offsets, and that with no spans there is no group.
#[test]
fn no_ranges_is_every_span_alone_and_no_spans_is_no_group() {
    let found = groups(
        &keys(),
        vec![span(1, 0, 10), span(0, 10, 10), span(0, 0, 10)],
        std::iter::empty(),
    );
    assert_eq!(
        found,
        [
            group("a", &[(0, 10)]),
            group("a", &[(10, 10)]),
            group("b", &[(0, 10)])
        ]
    );
    assert_eq!(
        groups(&keys(), Vec::new(), [range(0, 0, 10)].into_iter()),
        []
    );
}

/// Asserts that the pack of a group is the list's key its spans name, the same allocation.
#[test]
fn the_pack_of_a_group_is_the_key_its_spans_name() {
    let keys = keys();
    let found = groups(
        &keys,
        vec![span(1, 0, 10), span(0, 0, 10)],
        std::iter::empty(),
    );
    assert_eq!(found.len(), 2);
    assert!(Arc::ptr_eq(&found[0].pack, &keys[0]));
    assert!(Arc::ptr_eq(&found[1].pack, &keys[1]));
}

/// The file `path` of `size` bytes with the digest of `at`, whose bytes are in `key` at `offset`
/// (none if `key` is empty).
fn file(path: &str, at: u64, size: u64, key: &str, offset: u64) -> BuiltFile {
    BuiltFile {
        path: path.to_owned(),
        sha256: digest_at(at),
        size,
        source: (!key.is_empty()).then(|| BlobSource {
            key: Arc::from(key),
            offset,
        }),
    }
}

/// Asserts that a version 1 list's files are read one group each, one per digest at the span of the
/// digest's first file, and that a file with no source (an empty one) is not read.
#[test]
fn each_digest_of_the_files_is_one_group_at_its_first_file() {
    let files = vec![
        file("a", 1, 10, "p", 0),
        file("zero", 9, 0, "", 0),
        file("b", 1, 10, "p", 50),
        file("c", 2, 5, "q", 7),
        file("d", 1, 10, "q", 0),
    ];
    let expected = |pack: &str, offset: u64, size: u64, at: u64| Group {
        pack: Arc::from(pack),
        parts: vec![(offset, size, digest_at(at))],
    };
    assert_eq!(
        alone(&files),
        [expected("p", 0, 10, 1), expected("q", 7, 5, 2)]
    );
    assert_eq!(alone(&[]), []);
}
