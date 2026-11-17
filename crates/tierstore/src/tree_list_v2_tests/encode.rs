//! The writer of the version 2 tree list: the bytes `TreeList::encode_v2` makes, field by field,
//! its order, its refusals, and the tree those bytes read as.

use std::collections::HashSet;

use super::*;
use crate::tree_list::{EMPTY_SHA256, ListEntry, ListKind, TreeList, TreeListError};
use crate::tree_list_v2_encode::RANGE_MERGE_GAP;
use crate::{BlobSource, BuiltTree, Mode, NewKind, TreeDigest, TreeLimits, build_tree};

const SHA: [u8; 32] = [7; 32];

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

fn file(path: &str, size: u64, source: Option<(&str, u64)>, sha: Option<[u8; 32]>) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::File {
            mode: mode(0o644),
            size,
            source: source.map(|(key, offset)| BlobSource {
                key: key.into(),
                offset,
            }),
            sha256: sha.map(TreeDigest::from_bytes),
        },
    }
}

/// A file of `size` bytes at `offset` of pack `key`, with a digest of its own.
fn packed(path: &str, key: &str, offset: u64, size: u64) -> ListEntry {
    let mut sha = [0u8; 32];
    sha[..8].copy_from_slice(&offset.to_le_bytes());
    sha[8..16].copy_from_slice(&size.to_le_bytes());
    sha[16] = path.len() as u8;
    file(path, size, Some((key, offset)), Some(sha))
}

fn dir(path: &str, bits: u16) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::Dir { mode: mode(bits) },
    }
}

fn link(path: &str, target: &str) -> ListEntry {
    ListEntry {
        path: path.into(),
        kind: ListKind::Symlink {
            target: target.into(),
        },
    }
}

fn list(entries: Vec<ListEntry>) -> TreeList {
    TreeList { entries }
}

/// One record of an encoded list, decoded.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    name: String,
    parent: u32,
    name_off: u32,
    /// A file's key, a directory's first child.
    x: u32,
    kind: u8,
    mode: u16,
    flags: u8,
    /// A file's offset, a directory's child count, a link's target offset.
    a: u64,
    /// A file's size, a link's target length.
    b: u64,
    sha: [u8; 32],
}

fn rows(bytes: &[u8]) -> Vec<Row> {
    let list = TreeListV2::open(bytes).unwrap();
    (0..list.len())
        .map(|i| {
            let r = &list.entries[i * 32..i * 32 + 32];
            let name_off = le_u32(r, 4);
            let len = r[14] as usize;
            let kind_mode = le_u16(r, 12);
            Row {
                name: String::from_utf8(list.names[name_off as usize..][..len].to_vec()).unwrap(),
                parent: le_u32(r, 0),
                name_off,
                x: le_u32(r, 8),
                kind: (kind_mode >> 12) as u8,
                mode: kind_mode & 0o7777,
                flags: r[15],
                a: le_u64(r, 16),
                b: le_u64(r, 24),
                sha: list.digests[i * 32..i * 32 + 32].try_into().unwrap(),
            }
        })
        .collect()
}

fn names_of(bytes: &[u8]) -> Vec<String> {
    rows(bytes).into_iter().map(|row| row.name).collect()
}

fn encode(entries: Vec<ListEntry>) -> Vec<u8> {
    list(entries).encode_v2().unwrap()
}

/// The ranges `encode_v2` wrote for `entries`, as (key, offset, len).
fn ranges_of(entries: Vec<ListEntry>) -> Vec<(String, u64, u64)> {
    let bytes = encode(entries);
    let opened = TreeListV2::open(&bytes).unwrap();
    opened
        .ranges()
        .map(|r| (opened.keys()[r.key as usize].to_string(), r.offset, r.len))
        .collect()
}

fn range(key: &str, offset: u64, len: u64) -> (String, u64, u64) {
    (key.to_owned(), offset, len)
}

/// Asserts that a list of one file in one pack is written as exactly these bytes, field by field:
/// the header, the root and the file's record, the digests, the key offsets, the key, the name and
/// the range, each section at the next multiple of 8 with zero between.
#[test]
fn a_tiny_list_is_written_as_these_bytes() {
    let bytes = encode(vec![file("a", 3, Some(("k", 5)), Some(SHA))]);
    let mut expected: Vec<u8> = Vec::new();
    // Header: magic, major 1, minor 0, flags 0.
    expected.extend_from_slice(b"TSTREE\0\0");
    expected.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    // Six (offset, count): entries 64 x2, digests 128 x2, key offsets 192 x1, keys 200 x1,
    // names 208 x1, ranges 216 x1.
    for (offset, count) in [
        (64u32, 2u32),
        (128, 2),
        (192, 1),
        (200, 1),
        (208, 1),
        (216, 1),
    ] {
        expected.extend_from_slice(&offset.to_le_bytes());
        expected.extend_from_slice(&count.to_le_bytes());
    }
    assert_eq!(expected.len(), 64);
    // The root: parent 0, name offset 0, first child 1, directory 0755, name length 0, flags 0,
    // one child, size 0.
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(&0x11edu16.to_le_bytes());
    expected.extend_from_slice(&[0, 0]);
    expected.extend_from_slice(&1u64.to_le_bytes());
    expected.extend_from_slice(&0u64.to_le_bytes());
    // File a: parent 0, name offset 0, key 0, file 0644, name length 1, flags 0, offset 5, size 3.
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&0x01a4u16.to_le_bytes());
    expected.extend_from_slice(&[1, 0]);
    expected.extend_from_slice(&5u64.to_le_bytes());
    expected.extend_from_slice(&3u64.to_le_bytes());
    assert_eq!(expected.len(), 128);
    // Digests: zero for the root, the file's.
    expected.extend_from_slice(&[0; 32]);
    expected.extend_from_slice(&SHA);
    assert_eq!(expected.len(), 192);
    // Key offsets 0 and 1, then the key, padded to 208.
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(b"k");
    expected.extend_from_slice(&[0; 7]);
    // The names, padded to 216.
    expected.extend_from_slice(b"a");
    expected.extend_from_slice(&[0; 7]);
    // The range: key 0, 4 zero bytes, offset 5, length 3.
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&[0; 4]);
    expected.extend_from_slice(&5u64.to_le_bytes());
    expected.extend_from_slice(&3u64.to_le_bytes());
    assert_eq!(bytes, expected);
}

/// Asserts that a list of nothing is the root alone, with no keys, no names and no ranges.
#[test]
fn an_empty_list_is_the_root_alone() {
    let bytes = encode(Vec::new());
    let found = rows(&bytes);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, 1);
    assert_eq!(found[0].mode, 0o755);
    assert_eq!(found[0].a, 0);
    assert_eq!(found[0].x, 1);
    assert!(TreeListV2::open(&bytes).unwrap().is_empty());
    assert_eq!(le_u32(&bytes, count_at(RANGES)), 0);
    assert_eq!(le_u32(&bytes, count_at(NAMES)), 0);
}

/// Asserts that a directory only the paths imply is an entry of its own with mode 0755, at each
/// level, with its children counted and its parent set.
#[test]
fn an_implied_directory_is_an_entry_of_mode_0755() {
    let bytes = encode(vec![file("a/b/c", 0, None, None)]);
    let found = rows(&bytes);
    assert_eq!(names_of(&bytes), ["", "a", "b", "c"]);
    assert_eq!(
        (found[1].kind, found[1].mode, found[1].parent),
        (1, 0o755, 0)
    );
    assert_eq!(
        (found[2].kind, found[2].mode, found[2].parent),
        (1, 0o755, 1)
    );
    assert_eq!((found[3].kind, found[3].parent), (0, 2));
    assert_eq!((found[1].x, found[1].a), (2, 1));
    assert_eq!((found[2].x, found[2].a), (3, 1));
}

/// Asserts that a directory the list gives a mode, before or after the paths below it, keeps that
/// mode over the 0755 the paths imply, and that an explicit directory with nothing in it is an
/// entry with no children.
#[test]
fn an_explicit_directory_mode_overrides_the_implied_one() {
    for entries in [
        vec![dir("a", 0o700), file("a/f", 0, None, None)],
        vec![file("a/f", 0, None, None), dir("a", 0o700)],
    ] {
        let found = rows(&encode(entries));
        assert_eq!((found[1].name.as_str(), found[1].mode), ("a", 0o700));
    }
    let found = rows(&encode(vec![dir("x/y", 0o1777), dir("e", 0o500)]));
    let by_name = |name: &str| found.iter().find(|row| row.name == name).unwrap();
    assert_eq!(by_name("x").mode, 0o755);
    assert_eq!(by_name("y").mode, 0o1777);
    assert_eq!((by_name("e").mode, by_name("e").a), (0o500, 0));
}

/// Asserts that the entries are breadth first from the root, siblings sorted bytewise (an upper
/// case name before a lower case one, a prefix before its extension), each directory's children
/// right after those of the directories before it.
#[test]
fn entries_are_breadth_first_with_siblings_sorted_bytewise() {
    let bytes = encode(vec![
        file("b/x", 0, None, None),
        file("a/z", 0, None, None),
        file("a/y/q", 0, None, None),
        file("B", 0, None, None),
        file("ab", 0, None, None),
        file("a/y2", 0, None, None),
    ]);
    assert_eq!(
        names_of(&bytes),
        ["", "B", "a", "ab", "b", "y", "y2", "z", "x", "q"]
    );
    let found = rows(&bytes);
    // Children of a: entries 5..8, of b: 8..9, of a/y: 9..10.
    assert_eq!((found[2].x, found[2].a), (5, 3));
    assert_eq!((found[4].x, found[4].a), (8, 1));
    assert_eq!((found[5].x, found[5].a), (9, 1));
    let parents: Vec<u32> = found.iter().map(|row| row.parent).collect();
    assert_eq!(parents, [0, 0, 0, 0, 0, 2, 2, 2, 4, 5]);
    assert_eq!((found[0].x, found[0].a), (1, 4));
}

/// Asserts that a name is in the names once, however many directories hold an entry of it, so two
/// files of one name in two directories share an offset.
#[test]
fn a_name_is_in_the_names_once() {
    let bytes = encode(vec![
        file("d1/same", 0, None, None),
        file("d2/same", 0, None, None),
    ]);
    let found = rows(&bytes);
    assert_eq!(names_of(&bytes), ["", "d1", "d2", "same", "same"]);
    assert_eq!(found[3].name_off, found[4].name_off);
    assert_eq!(found[3].name_off, 4);
    let opened = TreeListV2::open(&bytes).unwrap();
    assert_eq!(opened.names, b"d1d2same");
}

/// Asserts that a link's target is in the names right after the link's name, with its offset and
/// length in the record, kind 2, mode 0 and no digest, and that two links of one target each have
/// their own copy of it.
#[test]
fn a_link_target_follows_its_name() {
    let bytes = encode(vec![link("l1", "t"), link("l2", "t")]);
    let opened = TreeListV2::open(&bytes).unwrap();
    assert_eq!(opened.names, b"l1tl2t");
    let found = rows(&bytes);
    assert_eq!((found[1].kind, found[1].mode), (2, 0));
    assert_eq!((found[1].name_off, found[1].a, found[1].b), (0, 2, 1));
    assert_eq!((found[2].name_off, found[2].a, found[2].b), (3, 5, 1));
    assert_eq!(found[1].sha, [0; 32]);
    assert_eq!(found[1].x, 0);
}

/// Asserts that a link's name is interned like a file's of the same name in another directory, and
/// its target is read back whole from after the name.
#[test]
fn a_link_name_is_interned_like_any_other() {
    let bytes = encode(vec![link("a/n", "../far/away"), file("b/n", 0, None, None)]);
    let found = rows(&bytes);
    let opened = TreeListV2::open(&bytes).unwrap();
    assert_eq!(names_of(&bytes), ["", "a", "b", "n", "n"]);
    assert_eq!(found[3].name_off, found[4].name_off);
    assert_eq!(
        &opened.names[found[3].a as usize..][..found[3].b as usize],
        b"../far/away"
    );
}

/// Asserts that the keys are sorted and each is in the key table once, and a file's key number is
/// its place in that order.
#[test]
fn keys_are_sorted_and_each_is_once() {
    let bytes = encode(vec![
        packed("a", "z", 0, 1),
        packed("b", "a", 0, 2),
        packed("c", "m", 0, 3),
        packed("d", "a", 9, 4),
        file("e", 0, None, None),
    ]);
    let opened = TreeListV2::open(&bytes).unwrap();
    let keys: Vec<&str> = opened.keys.iter().map(|k| &**k).collect();
    assert_eq!(keys, ["a", "m", "z"]);
    let found = rows(&bytes);
    let key_of = |name: &str| found.iter().find(|r| r.name == name).unwrap().x;
    assert_eq!(
        [
            key_of("a"),
            key_of("b"),
            key_of("c"),
            key_of("d"),
            key_of("e")
        ],
        [2, 0, 1, 0, 0]
    );
}

/// Asserts that ranges are sorted by key then offset whatever the order of the entries, and that
/// bytes of two keys are two ranges even when they touch.
#[test]
fn ranges_are_sorted_and_two_keys_never_merge() {
    let found = ranges_of(vec![
        packed("a", "b", 10, 10),
        packed("b", "a", 100_000_000, 5),
        packed("c", "a", 0, 10),
        packed("d", "b", 0, 10),
    ]);
    assert_eq!(
        found,
        [
            range("a", 0, 10),
            range("a", 100_000_000, 5),
            range("b", 0, 20)
        ]
    );
    let touching = ranges_of(vec![packed("a", "a", 0, 10), packed("b", "b", 10, 10)]);
    assert_eq!(touching, [range("a", 0, 10), range("b", 10, 10)]);
}

/// Asserts that adjacent bytes of a pack are one range.
#[test]
fn adjacent_spans_merge() {
    let found = ranges_of(vec![packed("a", "k", 0, 10), packed("b", "k", 10, 10)]);
    assert_eq!(found, [range("k", 0, 20)]);
}

/// Asserts that a gap of exactly 64 KiB between two files of a pack is merged, with the gap in
/// the range, and a gap of 64 KiB and one byte is not.
#[test]
fn a_gap_of_64_kib_merges_and_one_byte_more_does_not() {
    assert_eq!(RANGE_MERGE_GAP, 65_536);
    let at = |gap: u64| ranges_of(vec![packed("a", "k", 0, 10), packed("b", "k", 10 + gap, 5)]);
    assert_eq!(at(RANGE_MERGE_GAP), [range("k", 0, 10 + 65_536 + 5)]);
    assert_eq!(
        at(RANGE_MERGE_GAP + 1),
        [range("k", 0, 10), range("k", 10 + 65_537, 5)]
    );
}

/// Asserts that files whose bytes overlap, or lie within the bytes of another, are one range that
/// ends at the furthest end, and a merged range goes on merging what follows it.
#[test]
fn overlapping_and_contained_spans_merge() {
    let overlap = ranges_of(vec![packed("a", "k", 0, 10), packed("b", "k", 5, 10)]);
    assert_eq!(overlap, [range("k", 0, 15)]);
    let contained = ranges_of(vec![packed("a", "k", 0, 100), packed("b", "k", 10, 5)]);
    assert_eq!(contained, [range("k", 0, 100)]);
    let chain = ranges_of(vec![
        packed("a", "k", 0, 10),
        packed("b", "k", 50_000, 10),
        packed("c", "k", 110_000, 10),
    ]);
    assert_eq!(chain, [range("k", 0, 110_010)]);
}

/// Asserts that a file whose offset and size pass 2^64 is written with the range ending at 2^64 - 1
/// rather than wrapping, and the list opens.
#[test]
fn a_span_past_2_64_ends_at_the_most() {
    let found = ranges_of(vec![packed("a", "k", u64::MAX - 1, 10)]);
    assert_eq!(found, [range("k", u64::MAX - 1, 1)]);
}

/// Asserts that the same entries in any order are the same bytes.
#[test]
fn the_same_entries_in_any_order_are_the_same_bytes() {
    let entries = vec![
        packed("z/a", "k2", 7, 100),
        packed("z/b", "k1", 0, 50),
        link("l", "z/a"),
        dir("e", 0o700),
        file("empty", 0, None, None),
        packed("a/b/c", "k1", 60, 5),
    ];
    let expected = encode(entries.clone());
    let mut reversed = entries.clone();
    reversed.reverse();
    assert_eq!(encode(reversed), expected);
    let mut rotated = entries;
    rotated.rotate_left(2);
    assert_eq!(encode(rotated), expected);
}

/// Asserts that a file of size 0 with no digest gets the digest of no bytes and no key, a file of
/// size 0 with a digest keeps it, and a directory and a link have the zero digest.
#[test]
fn a_file_of_size_0_without_a_digest_has_the_empty_digest() {
    let bytes = encode(vec![
        file("e", 0, None, None),
        file("f", 0, None, Some(SHA)),
        dir("d", 0o755),
        link("l", "e"),
    ]);
    let found = rows(&bytes);
    let row = |name: &str| found.iter().find(|r| r.name == name).unwrap();
    assert_eq!(row("e").sha, EMPTY_SHA256);
    assert_eq!(row("f").sha, SHA);
    assert_eq!(row("d").sha, [0; 32]);
    assert_eq!(row("l").sha, [0; 32]);
    assert_eq!((row("e").x, row("e").a, row("e").b), (0, 0, 0));
    assert!(found.iter().all(|r| r.flags == 0));
    assert_eq!(le_u32(&bytes, count_at(RANGES)), 0);
}

/// Asserts that a file of size above 0 with no digest is refused, naming its path, and that the
/// refusal says so in words.
#[test]
fn a_file_without_a_digest_is_refused() {
    let error = list(vec![file("a/b", 1, Some(("k", 0)), None)])
        .encode_v2()
        .unwrap_err();
    assert_eq!(error, TreeListError::MissingSha256("a/b".to_owned()));
    assert_eq!(error.to_string(), "file \"a/b\" has no sha256");
}

/// Asserts that a name of 255 bytes is written and one of 256 is refused, naming the path, for a
/// file, for a directory the path implies and for a link.
#[test]
fn a_name_over_255_bytes_is_refused() {
    let ok = "n".repeat(255);
    let long = "n".repeat(256);
    let bytes = encode(vec![file(&ok, 0, None, None)]);
    assert_eq!(rows(&bytes)[1].name, ok);

    let error = list(vec![file(&long, 0, None, None)])
        .encode_v2()
        .unwrap_err();
    assert_eq!(error, TreeListError::NameTooLong(long.clone()));
    assert_eq!(error.to_string(), format!("name too long in {long:?}"));

    let in_dir = format!("{long}/f");
    let error = list(vec![file(&in_dir, 0, None, None)])
        .encode_v2()
        .unwrap_err();
    assert_eq!(error, TreeListError::NameTooLong(long.clone()));
    let error = list(vec![link(&long, "t")]).encode_v2().unwrap_err();
    assert_eq!(error, TreeListError::NameTooLong(long));
}

/// Asserts that an object that would be over the bound is refused as too large, with the size it
/// would have: here a link whose target is a little under the bound.
#[test]
fn an_object_over_the_bound_is_not_written() {
    let target = "t".repeat(MAX_TREE_LIST_BYTES - 100);
    let error = list(vec![link("l", &target)]).encode_v2().unwrap_err();
    // The entries and digests end at 192, the key offsets at 196, the keys (none) at 200, the
    // names (`l` and the target) end at 201 + target, and the ranges (none) begin at the next
    // multiple of 8.
    let size = (201 + target.len()).next_multiple_of(8);
    assert_eq!(
        error,
        TreeListError::TooLarge {
            size,
            max: MAX_TREE_LIST_BYTES,
        }
    );
    assert!(size > MAX_TREE_LIST_BYTES);
    assert_eq!(
        error.to_string(),
        format!("tree list too large: {size} bytes, at most 268435456")
    );
}

/// Asserts that a file of size 0 that names a pack (a list made in code) is written with no key
/// and no range, as a list the reader opens: the bytes are those of the same file with no pack
/// beside a key table that still names the pack.
#[test]
fn a_file_of_size_0_with_a_source_is_written_as_a_list_that_opens() {
    let bytes = encode(vec![file("e", 0, Some(("k", 3)), None)]);
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.ranges().count(), 0);
    let built = list.build(&TreeLimits::default(), drop).unwrap();
    assert_eq!(built.files[0].source, None);
    assert_eq!(built.files[0].size, 0);
}

/// The root, the records as a sorted set and the files as a sorted set of what `built` holds.
fn summary(built: BuiltTree) -> (TreeDigest, Vec<String>, Vec<String>) {
    let mut records: Vec<String> = built.records.iter().map(|r| format!("{r:?}")).collect();
    records.sort();
    let mut files: Vec<String> = built.files.iter().map(|f| format!("{f:?}")).collect();
    files.sort();
    (built.root, records, files)
}

/// Asserts that opening and building what `encode_v2` writes gives the tree that `build_tree`
/// makes of the version 1 entries, digest for digest: the root, the records as a set and the
/// files, for several trees, and that the spans handed out are those of the files of size above 0,
/// one for each digest.
#[test]
fn what_encode_v2_writes_builds_the_tree_the_entries_make() {
    let trees = vec![
        list(Vec::new()),
        list(vec![file("only", 0, None, None)]),
        list(vec![
            packed("bin/cc", "p1", 0, 100),
            packed("bin/ld", "p1", 100, 50),
            packed("lib/x/y/libz.so", "p2", 0, 70_000),
            link("bin/cc2", "cc"),
            dir("tmp", 0o1777),
            dir("lib/x/empty", 0o700),
            file("etc/e", 0, None, None),
            file("etc/dup1", 4, Some(("p3", 9)), Some(SHA)),
            file("etc/dup2", 4, Some(("p3", 400_000)), Some(SHA)),
        ]),
        list(vec![
            dir("a", 0o750),
            packed("a/B", "k", 5, 5),
            packed("a/b", "k", 0, 5),
            packed("é/ü", "k", 20, 5),
            link("a/l", "../é"),
        ]),
    ];
    for tree in trees {
        let bytes = tree.encode_v2().unwrap();
        let opened = TreeListV2::open(&bytes).unwrap();
        let mut spans = Vec::new();
        let built = opened
            .build(&TreeLimits::default(), |span| spans.push(span))
            .unwrap();
        let entries = tree.clone().into_entries().unwrap();
        let expected = build_tree(None, entries.clone(), &TreeLimits::default()).unwrap();
        assert_eq!(summary(built), summary(expected));

        let digests: HashSet<[u8; 32]> = spans.iter().map(|s| *s.sha256.as_bytes()).collect();
        assert_eq!(digests.len(), spans.len());
        let wanted: HashSet<[u8; 32]> = entries
            .iter()
            .filter_map(|e| match &e.kind {
                NewKind::File { sha256, size, .. } if *size > 0 => Some(*sha256.as_bytes()),
                _ => None,
            })
            .collect();
        assert_eq!(digests, wanted);
    }
}

/// Asserts that of two files of one digest the first in the order of the entries gives the span:
/// `etc/dup1` sorts before `etc/dup2` whatever the order they were listed in.
#[test]
fn the_span_of_a_digest_is_that_of_its_first_file() {
    let tree = list(vec![
        file("etc/dup2", 4, Some(("p3", 400_000)), Some(SHA)),
        file("etc/dup1", 4, Some(("p3", 9)), Some(SHA)),
    ]);
    let bytes = tree.encode_v2().unwrap();
    let mut spans = Vec::new();
    TreeListV2::open(&bytes)
        .unwrap()
        .build(&TreeLimits::default(), |span| spans.push(span))
        .unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].offset, spans[0].size), (9, 4));
    assert_eq!(spans[0].key, 0);
    assert_eq!(&*list_keys(&bytes)[0], "p3");
}

/// The keys of the list in `bytes`.
fn list_keys(bytes: &[u8]) -> Vec<std::sync::Arc<str>> {
    TreeListV2::open(bytes).unwrap().keys().to_vec()
}
