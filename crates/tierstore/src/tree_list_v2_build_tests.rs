//! The walk of a version 2 tree list: every refusal of an entry by name and index, the limits at
//! their exact values, the spans handed out, the tree equal to the one a version 1 list builds,
//! and hostile bytes.

use std::sync::Arc;

use crate::tree_list::{ListEntry, ListKind, MAX_TARGET_BYTES, TreeList};
use crate::tree_list_v2::{
    EntryFault, HeaderFault, ListSpan, Section, TreeListV2, TreeListV2Error,
};
use crate::{
    BlobSource, BuiltFile, BuiltTree, Mode, RecordError, TreeDigest, TreeLimits, build_tree,
};

fn digest(byte: u8) -> TreeDigest {
    TreeDigest::from_bytes([byte; 32])
}

fn key(text: &str, offset: u64) -> Option<BlobSource> {
    Some(BlobSource {
        key: Arc::from(text),
        offset,
    })
}

fn file(path: &str, size: u64, source: Option<BlobSource>, sha: u8) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::FILE,
            size,
            source,
            sha256: Some(digest(sha)),
        },
    }
}

fn empty_file(path: &str) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::FILE,
            size: 0,
            source: None,
            sha256: None,
        },
    }
}

fn dir(path: &str, mode: u16) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::Dir {
            mode: Mode::new(mode).unwrap(),
        },
    }
}

fn link(path: &str, target: &str) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::Symlink {
            target: target.to_owned(),
        },
    }
}

fn encode(entries: Vec<ListEntry>) -> Vec<u8> {
    TreeList { entries }.encode_v2().unwrap()
}

/// The list the patches below work on. Entries: 0 root, 1 `d`, 2 `f`, 3 `g` (size 0), 4 `l`
/// (a link to `d/x`), 5 `d/x`. Keys: `k1` is 0, `k2` is 1.
fn base() -> Vec<u8> {
    encode(vec![
        file("d/x", 5, key("k1", 0), 1),
        file("f", 3, key("k2", 10), 2),
        empty_file("g"),
        link("l", "d/x"),
    ])
}

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

/// Where entry `index` begins: its parent (4 bytes), name offset (4), first child or key index
/// (4), kind and mode (2), name length (1), padding (1), offset or child count (8), size (8).
fn rec(bytes: &[u8], index: usize) -> usize {
    le32(bytes, 16) + index * 32
}

fn digest_at(bytes: &[u8], index: usize) -> usize {
    le32(bytes, 24) + index * 32
}

fn names_at(bytes: &[u8]) -> usize {
    le32(bytes, 48)
}

fn names_len(bytes: &[u8]) -> usize {
    le32(bytes, 52)
}

fn set_parent(bytes: &mut [u8], index: usize, parent: u32) {
    let at = rec(bytes, index);
    put(bytes, at, &parent.to_le_bytes());
}

fn set_name(bytes: &mut [u8], index: usize, offset: u32, len: u8) {
    let at = rec(bytes, index);
    put(bytes, at + 4, &offset.to_le_bytes());
    bytes[at + 14] = len;
}

fn set_x(bytes: &mut [u8], index: usize, x: u32) {
    let at = rec(bytes, index);
    put(bytes, at + 8, &x.to_le_bytes());
}

fn set_kind(bytes: &mut [u8], index: usize, kind: u16) {
    let at = rec(bytes, index);
    let old = u16::from_le_bytes([bytes[at + 12], bytes[at + 13]]);
    put(
        bytes,
        at + 12,
        &((kind << 12) | (old & 0o7777)).to_le_bytes(),
    );
}

fn set_a(bytes: &mut [u8], index: usize, a: u64) {
    let at = rec(bytes, index);
    put(bytes, at + 16, &a.to_le_bytes());
}

fn set_b(bytes: &mut [u8], index: usize, b: u64) {
    let at = rec(bytes, index);
    put(bytes, at + 24, &b.to_le_bytes());
}

fn walk(bytes: &[u8], limits: &TreeLimits) -> (Result<BuiltTree, TreeListV2Error>, Vec<ListSpan>) {
    let list = TreeListV2::open(bytes).unwrap();
    let mut spans = Vec::new();
    let built = list.build(limits, |span| spans.push(span));
    (built, spans)
}

fn built(bytes: &[u8]) -> BuiltTree {
    walk(bytes, &TreeLimits::default()).0.unwrap()
}

fn refusal(bytes: &[u8]) -> TreeListV2Error {
    walk(bytes, &TreeLimits::default()).0.unwrap_err()
}

/// Asserts that `bytes` are refused as entry `index` with `fault`, whose message is `message`.
fn expect(bytes: &[u8], index: u32, fault: EntryFault, message: &str) {
    let error = refusal(bytes);
    assert_eq!(error, TreeListV2Error::Entry { index, fault });
    assert_eq!(
        error.to_string(),
        format!("tree list entry {index}: {message}")
    );
}

fn one_file(name: &str) -> Vec<u8> {
    encode(vec![file(name, 1, key("k", 0), 1)])
}

/// A path of `len` bytes: twenty names of 200 bytes, then a last name.
fn long_path(len: usize) -> String {
    let mut parts = vec!["a".repeat(200); 20];
    parts.push("b".repeat(len - 20 * 201));
    parts.join("/")
}

const BASE_MESSAGE_ROOT: &str = "the root is not a directory with no name and parent 0";

/// Asserts that the base list is walked into its six entries, and that the later tests refuse
/// copies of it, not a list that was never valid.
#[test]
fn the_base_list_is_a_valid_tree_of_five_entries_and_a_root() {
    let bytes = base();
    let list = TreeListV2::open(&bytes).unwrap();
    assert_eq!(list.len(), 6);
    let tree = built(&bytes);
    let paths: Vec<_> = tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["f", "g", "d/x"]);
    assert_eq!(tree.records.len(), 2);
}

/// Asserts that a root that is a file is refused as entry 0 `Root`.
#[test]
fn a_root_that_is_not_a_directory_is_refused() {
    let mut bytes = base();
    set_kind(&mut bytes, 0, 0);
    expect(&bytes, 0, EntryFault::Root, BASE_MESSAGE_ROOT);
}

/// Asserts that a root whose parent is not 0 is refused as `Root`.
#[test]
fn a_root_with_a_parent_is_refused() {
    let mut bytes = base();
    set_parent(&mut bytes, 0, 1);
    expect(&bytes, 0, EntryFault::Root, BASE_MESSAGE_ROOT);
}

/// Asserts that a root with a name is refused as `Root`.
#[test]
fn a_root_with_a_name_is_refused() {
    let mut bytes = base();
    set_name(&mut bytes, 0, 0, 1);
    expect(&bytes, 0, EntryFault::Root, BASE_MESSAGE_ROOT);
}

/// Asserts that kinds 3 and 15 are refused as `UnknownKind` with the kind, at the entry.
#[test]
fn a_kind_above_the_link_is_refused() {
    for kind in [3u16, 15] {
        let mut bytes = base();
        set_kind(&mut bytes, 2, kind);
        expect(
            &bytes,
            2,
            EntryFault::UnknownKind(kind as u8),
            &format!("unknown kind {kind}"),
        );
    }
}

/// Asserts that a directory no children slice before it holds is refused as `Unreachable`.
#[test]
fn a_directory_outside_every_children_slice_is_unreachable() {
    let mut bytes = base();
    set_a(&mut bytes, 1, 0);
    set_kind(&mut bytes, 5, 1);
    expect(
        &bytes,
        5,
        EntryFault::Unreachable,
        "a directory before the children of the directories before it",
    );
}

/// Asserts that a directory whose first child is not the next free entry is refused as `Children`.
#[test]
fn a_directory_whose_first_child_is_not_next_is_refused() {
    let mut bytes = base();
    set_x(&mut bytes, 1, 4);
    expect(
        &bytes,
        1,
        EntryFault::Children,
        "children not where the breadth-first order puts them",
    );
}

/// Asserts that children running past the last entry are refused as `Children`, one past the end
/// and 2^64 - 1, and that exactly the last entry is accepted.
#[test]
fn children_past_the_end_are_refused() {
    let mut bytes = base();
    set_a(&mut bytes, 1, 2);
    expect(
        &bytes,
        1,
        EntryFault::Children,
        "children not where the breadth-first order puts them",
    );
    let mut bytes = base();
    set_a(&mut bytes, 0, u64::MAX);
    assert_eq!(
        refusal(&bytes),
        TreeListV2Error::Entry {
            index: 0,
            fault: EntryFault::Children
        }
    );
    let mut bytes = base();
    set_a(&mut bytes, 1, 1);
    built(&bytes);
}

/// Asserts that a child whose parent is not the directory it is among is refused as `Parent`.
#[test]
fn a_child_of_another_parent_is_refused() {
    let mut bytes = base();
    set_parent(&mut bytes, 5, 3);
    expect(
        &bytes,
        5,
        EntryFault::Parent(3),
        "parent 3 is not the directory that holds it",
    );
}

/// Asserts that a name past the end of the names is refused as `NameBounds`, one byte past and
/// at the largest offset.
#[test]
fn a_name_past_the_names_is_refused() {
    let message = "name past the end of the names";
    let mut bytes = base();
    let end = names_len(&bytes) as u32;
    set_name(&mut bytes, 2, end, 1);
    expect(&bytes, 2, EntryFault::NameBounds, message);
    let mut bytes = base();
    set_name(&mut bytes, 2, u32::MAX, 255);
    expect(&bytes, 2, EntryFault::NameBounds, message);
    let mut bytes = base();
    set_name(&mut bytes, 2, end - 1, 2);
    expect(&bytes, 2, EntryFault::NameBounds, message);
}

/// Asserts that an empty name is refused as `BadName`.
#[test]
fn an_empty_name_is_refused() {
    let mut bytes = one_file("a");
    set_name(&mut bytes, 1, 0, 0);
    expect(&bytes, 1, EntryFault::BadName, "bad name");
}

/// Asserts that `.`, `..`, a name beginning `.wh.`, and a name with a NUL are refused as `BadName`.
#[test]
fn names_a_tree_may_not_have_are_refused() {
    for name in [".", "..", ".wh.x", "a\0b"] {
        expect(&one_file(name), 1, EntryFault::BadName, "bad name");
    }
}

/// Asserts that a name with a `/` and a name that is not UTF-8 are refused as `BadName`.
#[test]
fn a_slash_and_invalid_utf8_in_a_name_are_refused() {
    for byte in [b'/', 0xFF] {
        let mut bytes = one_file("a");
        let at = names_at(&bytes);
        bytes[at] = byte;
        expect(&bytes, 1, EntryFault::BadName, "bad name");
    }
}

/// Asserts that two siblings of one name are refused as `NotAscending`.
#[test]
fn siblings_of_one_name_are_refused() {
    let mut bytes = encode(vec![
        file("a", 1, key("k", 0), 1),
        file("b", 1, key("k", 1), 2),
    ]);
    set_name(&mut bytes, 2, 0, 1);
    expect(
        &bytes,
        2,
        EntryFault::NotAscending,
        "name not after its sibling's",
    );
}

/// Asserts that siblings in descending order are refused as `NotAscending`, at the second.
#[test]
fn siblings_in_descending_order_are_refused() {
    let mut bytes = encode(vec![
        file("a", 1, key("k", 0), 1),
        file("b", 1, key("k", 1), 2),
    ]);
    let at = names_at(&bytes);
    put(&mut bytes, at, b"ba");
    expect(
        &bytes,
        2,
        EntryFault::NotAscending,
        "name not after its sibling's",
    );
}

/// Asserts that a path of 4096 bytes is walked and one of 4097 is refused as `PathTooLong` at the
/// entry that makes it.
#[test]
fn a_path_over_4096_bytes_is_refused() {
    let ok = encode(vec![file(&long_path(4096), 1, key("k", 0), 1)]);
    let tree = built(&ok);
    assert_eq!(tree.files[0].path.len(), 4096);
    let long = encode(vec![file(&long_path(4097), 1, key("k", 0), 1)]);
    let last = TreeListV2::open(&long).unwrap().len() as u32 - 1;
    expect(&long, last, EntryFault::PathTooLong, "path over 4096 bytes");
}

/// Asserts that a file of size above 0 whose key index is the key count, or higher, is refused as
/// `Key`, and the last key is accepted.
#[test]
fn a_file_with_no_such_key_is_refused() {
    let mut bytes = base();
    set_x(&mut bytes, 2, 2);
    expect(&bytes, 2, EntryFault::Key(2), "no key 2");
    set_x(&mut bytes, 2, u32::MAX);
    assert_eq!(
        refusal(&bytes),
        TreeListV2Error::Entry {
            index: 2,
            fault: EntryFault::Key(u32::MAX)
        }
    );
    set_x(&mut bytes, 2, 1);
    built(&bytes);
}

/// Asserts that a file whose offset plus size passes 2^64 is refused as `RangeOverflow`, and one
/// that ends at 2^64 - 1 is walked.
#[test]
fn a_file_range_past_2_pow_64_is_refused() {
    let mut bytes = base();
    set_a(&mut bytes, 2, u64::MAX);
    expect(&bytes, 2, EntryFault::RangeOverflow, "range overflow");
    set_a(&mut bytes, 2, u64::MAX - 3);
    built(&bytes);
}

/// Asserts that a file of size 0 whose digest is not that of no bytes is refused as `EmptyDigest`.
#[test]
fn an_empty_file_with_another_digest_is_refused() {
    let mut bytes = base();
    let at = digest_at(&bytes, 3);
    bytes[at] ^= 1;
    expect(
        &bytes,
        3,
        EntryFault::EmptyDigest,
        "a file of size 0 has the digest of no bytes",
    );
}

/// Asserts that a file of size 0 ignores its key index and offset: nonsense in them gives the
/// tree, the files and the spans of the list without it.
#[test]
fn an_empty_file_ignores_its_key_and_offset() {
    let bytes = base();
    let (clean, clean_spans) = walk(&bytes, &TreeLimits::default());
    let mut patched = bytes.clone();
    set_x(&mut patched, 3, u32::MAX);
    set_a(&mut patched, 3, u64::MAX);
    let (tree, spans) = walk(&patched, &TreeLimits::default());
    assert_eq!(tree.unwrap(), clean.unwrap());
    assert_eq!(spans, clean_spans);
}

/// Asserts that a link whose target has length 0 is refused as `Target`.
#[test]
fn a_link_with_an_empty_target_is_refused() {
    let mut bytes = base();
    set_b(&mut bytes, 4, 0);
    expect(&bytes, 4, EntryFault::Target, "bad target");
}

/// Asserts that a target of 4096 bytes is walked and one of 4097 is refused as `Target`.
#[test]
fn a_link_target_over_4096_bytes_is_refused() {
    let ok = encode(vec![link("l", &"t".repeat(MAX_TARGET_BYTES))]);
    built(&ok);
    let long = encode(vec![link("l", &"t".repeat(MAX_TARGET_BYTES + 1))]);
    expect(&long, 1, EntryFault::Target, "bad target");
}

/// Asserts that a target past the end of the names, and one whose offset plus length overflows,
/// are refused as `Target`, and that one ending at the end of the names is walked.
#[test]
fn a_link_target_outside_the_names_is_refused() {
    let mut bytes = base();
    let end = names_len(&bytes) as u64;
    set_a(&mut bytes, 4, end);
    set_b(&mut bytes, 4, 1);
    expect(&bytes, 4, EntryFault::Target, "bad target");
    set_a(&mut bytes, 4, end - 1);
    built(&bytes);
    set_a(&mut bytes, 4, u64::MAX - 1);
    set_b(&mut bytes, 4, 3);
    expect(&bytes, 4, EntryFault::Target, "bad target");
}

/// Asserts that a target that is not UTF-8, and one with a NUL, are refused as `Target`.
#[test]
fn a_link_target_that_is_not_text_is_refused() {
    for byte in [0xFF, 0] {
        let mut bytes = base();
        let at = names_at(&bytes) + le32(&bytes, rec(&bytes, 4) + 16);
        bytes[at] = byte;
        expect(&bytes, 4, EntryFault::Target, "bad target");
    }
}

/// Asserts that entries no directory holds are refused as `Orphans` with the first of them.
#[test]
fn entries_no_directory_holds_are_orphans() {
    let mut bytes = encode(vec![
        file("a", 1, key("k", 0), 1),
        file("b", 1, key("k", 1), 2),
        file("c", 1, key("k", 2), 3),
    ]);
    set_a(&mut bytes, 0, 2);
    let error = refusal(&bytes);
    assert_eq!(error, TreeListV2Error::Orphans(3));
    assert_eq!(
        error.to_string(),
        "tree list entries from 3 on are in no directory"
    );
}

/// Asserts that the entry limit is judged from the count alone, before any entry is read: a
/// list one over it is refused as `TooManyEntries` even when its root is bad, and a list at it is
/// walked.
#[test]
fn too_many_entries_is_refused_before_the_walk() {
    let limits = |max_entries| TreeLimits {
        max_entries,
        ..TreeLimits::default()
    };
    let bytes = base();
    assert!(walk(&bytes, &limits(5)).0.is_ok());
    let error = walk(&bytes, &limits(4)).0.unwrap_err();
    assert_eq!(error, TreeListV2Error::Tree(RecordError::TooManyEntries(4)));
    assert_eq!(
        error.to_string(),
        "the tree is refused: tree too large: more than 4 entries"
    );
    let mut bad_root = base();
    set_kind(&mut bad_root, 0, 0);
    assert_eq!(
        walk(&bad_root, &limits(4)).0.unwrap_err(),
        TreeListV2Error::Tree(RecordError::TooManyEntries(4))
    );
}

/// Asserts that a path of more names than `max_depth` is refused as `TooDeep` with the path, and
/// one of exactly `max_depth` is walked.
#[test]
fn a_tree_deeper_than_the_limit_is_refused() {
    let bytes = encode(vec![file("a/b/c/f", 1, key("k", 0), 1)]);
    let limits = |max_depth| TreeLimits {
        max_depth,
        ..TreeLimits::default()
    };
    assert!(walk(&bytes, &limits(4)).0.is_ok());
    let error = walk(&bytes, &limits(3)).0.unwrap_err();
    assert_eq!(
        error,
        TreeListV2Error::Tree(RecordError::TooDeep("a/b/c/f".to_owned()))
    );
    assert_eq!(
        error.to_string(),
        "the tree is refused: tree too deep at \"a/b/c/f\""
    );
}

/// Asserts that a directory with more children than `max_dir_entries` is refused as
/// `DirectoryTooLarge` with its path (empty for the root), and one with exactly that many is
/// walked.
#[test]
fn a_directory_over_the_limit_is_refused() {
    let limits = |max_dir_entries| TreeLimits {
        max_dir_entries,
        ..TreeLimits::default()
    };
    let bytes = encode(vec![
        file("p/q/x", 1, key("k", 0), 1),
        file("p/q/y", 1, key("k", 1), 2),
    ]);
    assert!(walk(&bytes, &limits(2)).0.is_ok());
    let error = walk(&bytes, &limits(1)).0.unwrap_err();
    assert_eq!(
        error,
        TreeListV2Error::Tree(RecordError::DirectoryTooLarge("p/q".to_owned()))
    );
    assert_eq!(
        error.to_string(),
        "the tree is refused: directory too large at \"p/q\""
    );
    let wide = encode(vec![
        file("a", 1, key("k", 0), 1),
        file("b", 1, key("k", 1), 2),
    ]);
    assert_eq!(
        walk(&wide, &limits(1)).0.unwrap_err(),
        TreeListV2Error::Tree(RecordError::DirectoryTooLarge(String::new()))
    );
}

fn some_files() -> Vec<ListEntry> {
    vec![
        file("z/deep", 4, key("k1", 100), 3),
        file("a", 5, key("k1", 0), 1),
        file("b", 6, key("k2", 7), 1),
        empty_file("c"),
        file("m", 7, key("k2", 50), 2),
    ]
}

/// Asserts that the spans are handed out in entry order, once for a digest (the second file of
/// digest 1 gets none), none for a file of size 0, each with the key of its file.
#[test]
fn spans_come_in_entry_order_once_per_digest() {
    let bytes = encode(some_files());
    let (tree, spans) = walk(&bytes, &TreeLimits::default());
    tree.unwrap();
    let want = |k: &str, offset, size, sha| ListSpan {
        key: Arc::from(k),
        offset,
        size,
        sha256: digest(sha),
    };
    assert_eq!(
        spans,
        [
            want("k1", 0, 5, 1),
            want("k2", 50, 7, 2),
            want("k1", 100, 4, 3)
        ]
    );
}

/// Asserts that a span's key is the list's own key, shared and not copied.
#[test]
fn a_span_shares_the_key_of_the_list() {
    let bytes = encode(vec![file("a", 5, key("k1", 0), 1)]);
    let list = TreeListV2::open(&bytes).unwrap();
    let mut spans = Vec::new();
    list.build(&TreeLimits::default(), |span| spans.push(span))
        .unwrap();
    assert!(Arc::ptr_eq(&spans[0].key, &list.keys[0]));
    assert_eq!(&*spans[0].key, "k1");
}

/// Asserts that the files of the result are in entry order with their full paths, and that each
/// has its source, except a file of size 0.
#[test]
fn files_are_in_entry_order_with_full_paths() {
    let tree = built(&encode(some_files()));
    let paths: Vec<_> = tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a", "b", "c", "m", "z/deep"]);
    assert_eq!(
        tree.files[0],
        BuiltFile {
            path: "a".to_owned(),
            sha256: digest(1),
            size: 5,
            source: key("k1", 0),
        }
    );
    assert_eq!(tree.files[2].source, None);
    assert_eq!(tree.files[2].size, 0);
    assert_eq!(tree.files[4].source, key("k1", 100));
}

/// Asserts that the spans handed out before a fault stay handed out: the files before the bad
/// entry have theirs.
#[test]
fn spans_before_a_fault_were_handed_out() {
    let mut bytes = encode(vec![
        file("a", 1, key("k", 0), 1),
        file("b", 1, key("k", 1), 2),
        file("c", 1, key("k", 2), 3),
    ]);
    set_a(&mut bytes, 3, u64::MAX);
    let (tree, spans) = walk(&bytes, &TreeLimits::default());
    assert_eq!(
        tree.unwrap_err(),
        TreeListV2Error::Entry {
            index: 3,
            fault: EntryFault::RangeOverflow
        }
    );
    assert_eq!(spans.len(), 2);
}

/// Asserts that the tree built from `entries` has the root, the records (as a set) and the files
/// (by path) of the tree the version 1 builder makes of the same entries.
fn assert_same_as_v1(entries: Vec<ListEntry>) {
    let list = TreeList { entries };
    let bytes = list.encode_v2().unwrap();
    let v2 = built(&bytes);
    let v1 = build_tree(None, list.into_entries().unwrap(), &TreeLimits::default()).unwrap();
    assert_eq!(v2.root, v1.root);
    let mut r2 = v2.records;
    let mut r1 = v1.records;
    r2.sort_by_key(|(d, _)| *d.as_bytes());
    r1.sort_by_key(|(d, _)| *d.as_bytes());
    assert_eq!(r2, r1);
    let mut f2 = v2.files;
    f2.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(f2, v1.files);
}

/// Asserts that a list of only the root builds the tree the version 1 builder makes of no entries.
#[test]
fn only_the_root_matches_version_1() {
    assert_same_as_v1(vec![]);
}

/// Asserts that files at the root build the tree version 1 does.
#[test]
fn root_files_match_version_1() {
    assert_same_as_v1(
        some_files()
            .into_iter()
            .filter(|e| !e.path.contains('/'))
            .collect(),
    );
}

/// Asserts that nested directories with explicit modes build the tree version 1 does.
#[test]
fn nested_directories_with_modes_match_version_1() {
    assert_same_as_v1(vec![
        dir("a", 0o700),
        dir("a/b", 0o750),
        file("a/b/f", 3, key("k", 0), 1),
        file("a/g", 3, key("k", 3), 2),
        file("h", 3, key("k", 6), 3),
    ]);
}

/// Asserts that empty directories build the tree version 1 does.
#[test]
fn empty_directories_match_version_1() {
    assert_same_as_v1(vec![dir("e", 0o755), dir("f/g", 0o700), dir("h", 0o555)]);
}

/// Asserts that links build the tree version 1 does.
#[test]
fn links_match_version_1() {
    assert_same_as_v1(vec![
        link("l", "../up"),
        link("d/m", "/abs/target"),
        file("d/f", 2, key("k", 0), 1),
    ]);
}

/// Asserts that files of one digest, and subtrees that are equal, build the tree version 1 does,
/// each record once.
#[test]
fn shared_digests_match_version_1() {
    let entries = vec![
        file("p/x", 3, key("k1", 0), 9),
        file("q/x", 3, key("k2", 8), 9),
        file("r", 3, key("k1", 90), 9),
    ];
    assert_same_as_v1(entries.clone());
    let bytes = encode(entries);
    let tree = built(&bytes);
    assert_eq!(tree.records.len(), 2);
}

/// Asserts that setuid, setgid and sticky bits of files and directories build the tree version 1
/// does.
#[test]
fn special_mode_bits_match_version_1() {
    let mut setuid = file("s", 3, key("k", 0), 1);
    setuid.kind = ListKind::File {
        mode: Mode::new(0o4755).unwrap(),
        size: 3,
        source: key("k", 0),
        sha256: Some(digest(1)),
    };
    assert_same_as_v1(vec![
        setuid,
        dir("t", 0o1777),
        dir("g", 0o2755),
        file("t/f", 3, key("k", 4), 2),
    ]);
}

/// Asserts that a count or an offset near 2^32 is refused without a panic or an allocation of that
/// size: each section with 2^32 - 1 records is past the end; an offset of 2^32 - 8 is past the end,
/// and one that is not a multiple of 8 is misaligned.
#[test]
fn counts_and_offsets_near_2_pow_32_are_refused() {
    let sections = [
        Section::Entries,
        Section::Digests,
        Section::KeyOffsets,
        Section::Keys,
        Section::Names,
        Section::Ranges,
    ];
    let valid = encode(vec![]);
    for (at, section) in sections.into_iter().enumerate() {
        let mut bytes = valid.clone();
        put(&mut bytes, 20 + 8 * at, &u32::MAX.to_le_bytes());
        assert_eq!(
            TreeListV2::open(&bytes).unwrap_err(),
            TreeListV2Error::Header(HeaderFault::PastEnd(section))
        );
        let mut bytes = valid.clone();
        put(&mut bytes, 16 + 8 * at, &(u32::MAX - 7).to_le_bytes());
        assert_eq!(
            TreeListV2::open(&bytes).unwrap_err(),
            TreeListV2Error::Header(HeaderFault::PastEnd(section))
        );
        let mut bytes = valid.clone();
        put(&mut bytes, 16 + 8 * at, &u32::MAX.to_le_bytes());
        assert_eq!(
            TreeListV2::open(&bytes).unwrap_err(),
            TreeListV2Error::Header(HeaderFault::Misaligned(section))
        );
    }
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// A valid list of about sixty entries: directories, links, and files of three keys, some of
/// size 0.
fn hostile_base() -> Vec<u8> {
    let mut entries = Vec::new();
    for d in 0..6u8 {
        entries.push(dir(&format!("d{d}"), 0o755));
        for f in 0..5u8 {
            let path = format!("d{d}/f{f}");
            let n = d * 5 + f;
            entries.push(match f {
                0 => empty_file(&path),
                1 => link(&path, &format!("../d{}/f2", (d + 1) % 6)),
                _ => file(
                    &path,
                    u64::from(n) + 1,
                    key(&format!("key{}", n % 3), u64::from(n) * 100),
                    n,
                ),
            });
        }
    }
    entries.push(file("top", 9, key("key1", 5), 200));
    encode(entries)
}

fn run(bytes: &[u8]) {
    if let Ok(list) = TreeListV2::open(bytes) {
        let _ = list.build(&TreeLimits::default(), |_| {});
    }
}

/// Asserts that the list the hostile tests mutate is valid and has about sixty entries.
#[test]
fn the_hostile_base_is_valid() {
    let bytes = hostile_base();
    let list = TreeListV2::open(&bytes).unwrap();
    assert!(list.len() > 35);
    let (tree, spans) = walk(&bytes, &TreeLimits::default());
    tree.unwrap();
    assert!(!spans.is_empty());
}

/// Asserts that 20,000 lists with one to four bytes changed, half of them in the header and the
/// entries and every fiftieth cut short, are each opened and walked to a result, never a panic.
#[test]
fn mutated_lists_never_panic() {
    let base = hostile_base();
    let head = le32(&base, 16) + le32(&base, 20) * 32;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for round in 1..=20_000u32 {
        let mut bytes = base.clone();
        for _ in 0..1 + xorshift(&mut state) % 4 {
            let region = if xorshift(&mut state).is_multiple_of(2) {
                head
            } else {
                bytes.len()
            };
            let at = (xorshift(&mut state) % region as u64) as usize;
            match xorshift(&mut state) % 3 {
                0 => bytes[at] ^= 1 << (xorshift(&mut state) % 8),
                1 => bytes[at] = xorshift(&mut state) as u8,
                _ => bytes[at] = 0xFF,
            }
        }
        if round % 50 == 0 {
            let len = (xorshift(&mut state) % (bytes.len() as u64 + 1)) as usize;
            bytes.truncate(len);
        }
        run(&bytes);
    }
}

/// Asserts that every truncation of a valid list, to every length from 0, is opened and walked to
/// a result, never a panic, and that only the whole list is walked to a tree.
#[test]
fn every_truncation_is_refused_without_a_panic() {
    let base = hostile_base();
    for len in 0..base.len() {
        run(&base[..len]);
    }
    for len in 0..le32(&base, 16) {
        assert!(TreeListV2::open(&base[..len]).is_err());
    }
    assert!(walk(&base, &TreeLimits::default()).0.is_ok());
}

/// Asserts that a 0xFF at every offset of a valid list, one at a time, is opened and walked to a
/// result, never a panic.
#[test]
fn a_0xff_at_every_offset_never_panics() {
    let base = hostile_base();
    for at in 0..base.len() {
        let mut bytes = base.clone();
        bytes[at] = 0xFF;
        run(&bytes);
    }
}

/// Asserts that a list whose paths together are over 256 MiB is refused by the walk, though the
/// object is a few MiB: fifteen directories of 255-byte names hold 70,000 files, so each file's
/// path is about 3,850 bytes, from names stored once.
#[test]
fn paths_over_the_bound_together_are_refused() {
    let deep = (0..15)
        .map(|level| format!("{level:x}").repeat(255))
        .collect::<Vec<_>>()
        .join("/");
    let entries = (0..70_000)
        .map(|n| empty_file(&format!("{deep}/f{n:05}")))
        .collect();
    let bytes = TreeList { entries }.encode_v2().unwrap();
    assert!(bytes.len() < 8 << 20);
    let refused = TreeListV2::open(&bytes)
        .unwrap()
        .build(&TreeLimits::default(), drop)
        .unwrap_err();
    assert_eq!(
        refused,
        TreeListV2Error::PathBytes {
            max: crate::MAX_TREE_LIST_BYTES
        }
    );
    assert_eq!(
        refused.to_string(),
        "tree list paths over 268435456 bytes together"
    );
}
