//! The walk of a version 2 tree list as `check` and `Checked::build`: the same refusals and spans
//! as `build`, the memo of names checked, and the set of digests whose span was handed out.

use super::*;

/// A file of one byte repeated as its digest.
fn flat(path: &str, key_of: &str, offset: u64, byte: u8) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::FILE,
            size: 4,
            source: key(key_of, offset),
            sha256: Some(TreeDigest::from_bytes([byte; 32])),
        },
    }
}

fn spans_of(bytes: &[u8]) -> Vec<ListSpan> {
    walk(bytes, &TreeLimits::default()).1
}

/// Asserts that `keys` gives the keys of the list, sorted, which a span's number names.
#[test]
fn keys_gives_the_keys_of_the_list_in_order() {
    let bytes = base();
    let list = TreeListV2::open(&bytes).unwrap();
    let keys: Vec<&str> = list.keys().iter().map(|key| &**key).collect();
    assert_eq!(keys, ["k1", "k2"]);
}

/// Asserts that `check` followed by `Checked::build` gives the tree and the spans that `build`
/// gives, the spans going to the caller during `check` and none during `Checked::build`.
#[test]
fn check_then_build_is_build() {
    let bytes = base();
    let list = TreeListV2::open(&bytes).unwrap();
    let mut spans = Vec::new();
    let checked = list
        .check(&TreeLimits::default(), |span| spans.push(span))
        .unwrap();
    let (whole, whole_spans) = walk(&bytes, &TreeLimits::default());
    assert_eq!(spans, whole_spans);
    assert_eq!(spans.len(), 2);
    let tree = checked.build();
    let whole = whole.unwrap();
    assert_eq!(tree.root, whole.root);
    assert_eq!(tree.records, whole.records);
    assert_eq!(tree.files, whole.files);
}

/// Asserts that `check` refuses what `build` refuses, with the same error, and has handed out the
/// same spans, for a bad entry, for more entries than the limit and for a bad root.
#[test]
fn check_refuses_as_build_does() {
    let mut bad_entry = base();
    set_parent(&mut bad_entry, 5, 3);
    let mut bad_root = base();
    set_kind(&mut bad_root, 0, 0);
    let limited = TreeLimits {
        max_entries: 2,
        ..TreeLimits::default()
    };
    for (bytes, limits) in [
        (bad_entry, TreeLimits::default()),
        (bad_root, TreeLimits::default()),
        (base(), limited),
    ] {
        let list = TreeListV2::open(&bytes).unwrap();
        let mut spans = Vec::new();
        let error = list
            .check(&limits, |span| spans.push(span))
            .err()
            .expect("check refuses");
        let (tree, built_spans) = walk(&bytes, &limits);
        assert_eq!(error, tree.unwrap_err());
        assert_eq!(spans, built_spans);
    }
}

/// Asserts that two entries that name one stretch of the names, in two directories, are both read
/// with that name and have their paths.
#[test]
fn two_entries_sharing_a_name_both_read() {
    let bytes = encode(vec![
        file("d/x", 5, key("k", 0), 1),
        file("e/x", 5, key("k", 8), 2),
    ]);
    let at = |index| le32(&bytes, rec(&bytes, index) + 4);
    assert_eq!(at(3), at(4));
    let tree = built(&bytes);
    let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["d/x", "e/x"]);
}

/// Asserts that an entry at the offset of a name checked, with a longer length, is checked as its
/// own name and is the longer name; and that when that longer slice is not a name it is `BadName`
/// though the shorter one at the same offset was good.
#[test]
fn a_name_at_a_known_offset_of_another_length_is_checked_again() {
    let bytes = encode(vec![
        file("a", 1, key("k", 0), 1),
        file("b", 1, key("k", 1), 2),
    ]);
    let first = le32(&bytes, rec(&bytes, 1) + 4);
    assert_eq!(&bytes[names_at(&bytes) + first..][..2], b"ab");
    let mut longer = bytes.clone();
    set_name(&mut longer, 2, first as u32, 2);
    let tree = built(&longer);
    let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a", "ab"]);

    let mut bad = bytes.clone();
    let at = names_at(&bad) + first;
    bad[at + 1] = b'/';
    set_name(&mut bad, 2, first as u32, 2);
    expect(&bad, 2, EntryFault::BadName, "bad name");
}

/// Asserts that of two files of one digest only the first has a span, that two files of two
/// digests both have one, and that the first of a digest in the order of the entries gives it.
#[test]
fn a_digest_has_one_span_and_two_digests_have_two() {
    let same = encode(vec![
        file("a", 5, key("k", 0), 1),
        file("b", 5, key("k", 100), 1),
    ]);
    let spans = spans_of(&same);
    assert_eq!(
        spans,
        [ListSpan {
            key: 0,
            offset: 0,
            size: 5,
            sha256: digest(1)
        }]
    );
    let two = encode(vec![
        file("a", 5, key("k", 0), 1),
        file("b", 5, key("k", 100), 2),
    ]);
    let spans = spans_of(&two);
    assert_eq!(
        spans
            .iter()
            .map(|s| (s.offset, s.sha256))
            .collect::<Vec<_>>(),
        [(0, digest(1)), (100, digest(2))]
    );
}

/// Asserts that digests of one byte repeated, which are equal word for word, each give a span:
/// the folding of a digest for the set of those handed out does not make them alike.
#[test]
fn digests_of_one_repeated_byte_each_have_a_span() {
    let bytes = encode(vec![
        flat("a", "k", 0, 1),
        flat("b", "k", 100, 2),
        flat("c", "k", 200, 3),
    ]);
    let spans = spans_of(&bytes);
    let at = |offset, byte| ListSpan {
        key: 0,
        offset,
        size: 4,
        sha256: TreeDigest::from_bytes([byte; 32]),
    };
    assert_eq!(spans, [at(0, 1), at(100, 2), at(200, 3)]);
}
