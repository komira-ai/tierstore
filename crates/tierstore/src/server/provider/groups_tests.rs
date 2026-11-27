//! The groups of a preparation's reads: which spans of a version 2 list are read together, where a
//! group is cut, and which spans are read alone.

use std::sync::Arc;

use super::groups::{GROUP_MAX_BYTES, Group, groups};
use crate::{ListRange, ListSpan, TreeDigest};

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

/// The span of key number `key` at `offset` of `size` bytes with the digest of `bytes`.
fn span_of(key: u32, offset: u64, size: u64, bytes: &[u8]) -> ListSpan {
    ListSpan {
        key,
        offset,
        size,
        sha256: TreeDigest::of(bytes),
    }
}

/// Asserts that spans that are identical (the same key, offset, size and digest), wherever they are
/// among the others, are one part of a group, in a range and out of every range.
#[test]
fn identical_spans_are_one_part() {
    let spans = vec![
        span(0, 10, 10),
        span(0, 0, 10),
        span(0, 10, 10),
        span(0, 0, 10),
        span(1, 50, 5),
        span(1, 50, 5),
    ];
    let found = groups(&keys(), spans, [range(0, 0, 20)].into_iter());
    assert_eq!(
        found,
        [group("a", &[(0, 10), (10, 10)]), group("b", &[(50, 5)])]
    );
}

/// Asserts that spans equal but for their size are two parts, and spans equal but for their digest
/// are two parts: only a span equal in all four is read once.
#[test]
fn spans_that_differ_in_size_or_digest_stay_two_parts() {
    let (one, other) = (TreeDigest::of(b"one"), TreeDigest::of(b"other"));
    let spans = vec![span_of(0, 0, 10, b"one"), span_of(0, 0, 12, b"one")];
    let found = groups(&keys(), spans, std::iter::empty());
    assert_eq!(
        found,
        [
            Group {
                pack: Arc::from("a"),
                parts: vec![(0, 10, one)]
            },
            Group {
                pack: Arc::from("a"),
                parts: vec![(0, 12, one)]
            },
        ]
    );
    let spans = vec![span_of(0, 0, 10, b"one"), span_of(0, 0, 10, b"other")];
    let found = groups(&keys(), spans, std::iter::empty());
    let mut digests = [one, other];
    digests.sort_by_key(|digest| *digest.as_bytes());
    let expected: Vec<Group> = digests
        .into_iter()
        .map(|digest| Group {
            pack: Arc::from("a"),
            parts: vec![(0, 10, digest)],
        })
        .collect();
    assert_eq!(found, expected);
}

/// Asserts that spans are ordered by key, then offset, then size, then digest, whatever the order
/// they came in, so that the groups of one list are always the same: ties at one offset are the
/// smaller size first, and at one size the smaller digest first; identical spans in the run of ties
/// are dropped.
#[test]
fn the_spans_are_sorted_by_key_offset_size_and_digest_with_ties() {
    let mut low = [TreeDigest::of(b"x"), TreeDigest::of(b"y")];
    low.sort_by_key(|digest| *digest.as_bytes());
    let spans = vec![
        span_of(1, 0, 5, b"x"),
        ListSpan {
            sha256: low[1],
            ..span(0, 0, 5)
        },
        ListSpan {
            sha256: low[0],
            ..span(0, 0, 5)
        },
        span(0, 0, 3),
        span(0, 0, 5),
        ListSpan {
            sha256: low[0],
            ..span(0, 0, 5)
        },
        span(0, 0, 9),
    ];
    let found = groups(&keys(), spans, std::iter::empty());
    let parts: Vec<(u32, u64, u64, TreeDigest)> = found
        .iter()
        .map(|group| {
            let (offset, size, sha) = group.parts[0];
            (u32::from(&*group.pack == "b"), offset, size, sha)
        })
        .collect();
    let mut at_five = [low[0], low[1], digest_at(0)];
    at_five.sort_by_key(|digest| *digest.as_bytes());
    let mut expected = vec![(0, 0, 3, digest_at(0))];
    expected.extend(at_five.iter().map(|digest| (0, 0, 5, *digest)));
    expected.push((0, 0, 9, digest_at(0)));
    expected.push((1, 0, 5, TreeDigest::of(b"x")));
    assert_eq!(parts, expected);
}
