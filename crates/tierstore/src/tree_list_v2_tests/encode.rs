//! The writer of the version 2 tree list: the bytes `TreeList::encode_v2` makes, its refusals,
//! and the tree those bytes read as.

use crate::tree_list::{
    EMPTY_SHA256, ListEntry, ListKind, MAX_TREE_LIST_BYTES, TreeList, TreeListError,
};
use crate::tree_list_v2::{
    KIND_DIR, KIND_FILE, KIND_SYMLINK, NONE, TREE_LIST_V2_MAGIC, TreeListV2, TreeListV2Error,
    le_u64, read_u32,
};
use crate::tree_record::RecordError;
use crate::{BlobSource, Mode, TreeDigest, TreeLimits, build_tree};

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
    kind: u8,
    pad: u8,
    mode: u16,
    name: String,
    parent: u32,
    key: u32,
    size: u64,
    offset: u64,
    sha: [u8; 32],
}

/// The records of `bytes`, with each name looked up.
fn rows(bytes: &[u8]) -> Vec<Row> {
    let list = TreeListV2::open(bytes).unwrap();
    list.records
        .chunks_exact(list.record_len)
        .map(|r| Row {
            kind: r[0],
            pad: r[1],
            mode: u16::from_le_bytes([r[2], r[3]]),
            name: list.names.get(read_u32(r, 4).unwrap()).to_owned(),
            parent: read_u32(r, 8).unwrap(),
            key: read_u32(r, 12).unwrap(),
            size: le_u64(&r[16..]),
            offset: le_u64(&r[24..]),
            sha: r[32..64].try_into().unwrap(),
        })
        .collect()
}

/// The names of `rows` in order, with their kind.
fn names(bytes: &[u8]) -> Vec<(u8, String)> {
    rows(bytes).into_iter().map(|r| (r.kind, r.name)).collect()
}

/// Asserts that the bytes of a small list are exactly the layout the reference gives: the header,
/// the key, name and target tables and one record, with nothing between them and zero in every
/// ignored field.
#[test]
fn a_small_list_is_written_as_the_reference_lays_it_out() {
    let bytes = list(vec![file("a", 0, None, None)]).encode_v2().unwrap();
    let mut want = TREE_LIST_V2_MAGIC.to_vec();
    for field in [2u32, 40, 64, 1, 40, 44, 53, 57] {
        want.extend_from_slice(&field.to_le_bytes());
    }
    want.extend_from_slice(&0u32.to_le_bytes()); // keys: none
    want.extend_from_slice(&1u32.to_le_bytes()); // names: one, ending at 1
    want.extend_from_slice(&1u32.to_le_bytes());
    want.push(b'a');
    want.extend_from_slice(&0u32.to_le_bytes()); // targets: none
    want.push(KIND_FILE);
    want.push(0);
    want.extend_from_slice(&0o644u16.to_le_bytes());
    want.extend_from_slice(&0u32.to_le_bytes()); // name 0
    want.extend_from_slice(&NONE.to_le_bytes()); // root
    want.extend_from_slice(&NONE.to_le_bytes()); // no key
    want.extend_from_slice(&0u64.to_le_bytes());
    want.extend_from_slice(&0u64.to_le_bytes());
    want.extend_from_slice(&EMPTY_SHA256);
    assert_eq!(bytes, want);
}

/// Asserts that a list with no entries is written as a header and three empty tables that open
/// as an empty list.
#[test]
fn an_empty_list_is_written_and_read_as_empty() {
    let bytes = TreeList::default().encode_v2().unwrap();
    assert_eq!(bytes.len(), 40 + 12);
    let opened = TreeListV2::open(&bytes).unwrap();
    assert!(opened.is_empty());
}

/// Asserts that every directory a path implies is a record of its own, in mode 0755, each after
/// its parent, the first with no parent.
#[test]
fn implied_directories_get_records_of_mode_0755() {
    let bytes = list(vec![file("a/b/c.txt", 3, Some(("k", 0)), Some(SHA))])
        .encode_v2()
        .unwrap();
    let rows = rows(&bytes);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        (rows[0].kind, rows[0].mode, rows[0].parent),
        (KIND_DIR, 0o755, NONE)
    );
    assert_eq!(rows[0].name, "a");
    assert_eq!(
        (rows[1].kind, rows[1].mode, rows[1].parent),
        (KIND_DIR, 0o755, 0)
    );
    assert_eq!(rows[1].name, "b");
    assert_eq!(
        (rows[2].kind, rows[2].parent, rows[2].mode),
        (KIND_FILE, 1, 0o644)
    );
    assert_eq!(rows[2].name, "c.txt");
}

/// Asserts that an explicit directory sets the mode of the record an implied one would have been,
/// whether it is listed before or after the entries below it, and a directory listed alone, empty,
/// gets its record.
#[test]
fn an_explicit_directory_sets_the_mode_of_an_implied_one() {
    for order in [false, true] {
        let mut entries = vec![
            dir("a", 0o700),
            file("a/b/f", 1, Some(("k", 0)), Some(SHA)),
            dir("a/b", 0o1777),
            dir("empty", 0o500),
        ];
        if order {
            entries.reverse();
        }
        let bytes = list(entries).encode_v2().unwrap();
        let rows = rows(&bytes);
        let dirs: Vec<(&str, u16, u32)> = rows
            .iter()
            .filter(|r| r.kind == KIND_DIR)
            .map(|r| (r.name.as_str(), r.mode, r.parent))
            .collect();
        assert_eq!(
            dirs,
            vec![("a", 0o700, NONE), ("b", 0o1777, 0), ("empty", 0o500, NONE)]
        );
    }
}

/// Asserts that the directories come sorted by path, so a directory follows the one it is in, and
/// a child names its parent by the parent's record number.
#[test]
fn directories_are_sorted_by_path_and_name_their_parents_by_number() {
    let bytes = list(vec![
        dir("z", 0o755),
        dir("a/x/y", 0o755),
        dir("a-b", 0o755),
        file("a/x/y/f", 1, Some(("k", 0)), Some(SHA)),
    ])
    .encode_v2()
    .unwrap();
    let rows = rows(&bytes);
    let dirs: Vec<(&str, u32)> = rows
        .iter()
        .filter(|r| r.kind == KIND_DIR)
        .map(|r| (r.name.as_str(), r.parent))
        .collect();
    // Paths sorted: "a", "a-b", "a/x", "a/x/y", "z" ('-' sorts before '/').
    assert_eq!(
        dirs,
        vec![("a", NONE), ("a-b", NONE), ("x", 0), ("y", 2), ("z", NONE)]
    );
    assert_eq!(rows[5].parent, 3);
}

/// Asserts that the same entries in any order are the same bytes.
#[test]
fn the_same_entries_in_any_order_are_the_same_bytes() {
    let entries = vec![
        file("b/two", 2, Some(("pk/2", 10)), Some([2; 32])),
        file("b/one", 1, Some(("pk/1", 0)), Some([1; 32])),
        file("a/three", 3, Some(("pk/1", 5)), Some([3; 32])),
        file("empty", 0, None, None),
        link("l/y", "../b"),
        link("l/x", "../a"),
        dir("a", 0o700),
        dir("hollow", 0o755),
    ];
    let want = list(entries.clone()).encode_v2().unwrap();
    let mut reversed = entries.clone();
    reversed.reverse();
    assert_eq!(list(reversed).encode_v2().unwrap(), want);
    for turn in 1..entries.len() {
        let mut rotated = entries.clone();
        rotated.rotate_left(turn);
        assert_eq!(
            list(rotated).encode_v2().unwrap(),
            want,
            "rotated by {turn}"
        );
    }
    let mut swapped = entries;
    swapped.swap(0, 5);
    swapped.swap(2, 7);
    assert_eq!(list(swapped).encode_v2().unwrap(), want);
}

/// Asserts that the records are the directories, then the links sorted by path, then the files
/// with bytes sorted by key, offset and path, then the files of size 0 sorted by path.
#[test]
fn the_records_come_directories_links_files_then_empty_files() {
    let bytes = list(vec![
        file("zz-empty", 0, None, None),
        file("m", 4, Some(("pack-b", 0)), Some([1; 32])),
        file("n", 4, Some(("pack-a", 40)), Some([2; 32])),
        file("o", 4, Some(("pack-a", 8)), Some([3; 32])),
        file("p", 4, Some(("pack-a", 8)), Some([4; 32])),
        file("aa-empty", 0, None, None),
        link("lk-b", "t"),
        link("lk-a", "t"),
        dir("d", 0o755),
    ])
    .encode_v2()
    .unwrap();
    let order: Vec<String> = rows(&bytes)
        .into_iter()
        .map(|r| format!("{}:{}", r.kind, r.name))
        .collect();
    assert_eq!(
        order,
        [
            "1:d",
            "2:lk-a",
            "2:lk-b",
            "3:o",
            "3:p",
            "3:n",
            "3:m",
            "3:aa-empty",
            "3:zz-empty"
        ]
    );
}

/// Asserts that a key and a target are in their tables once however many entries use them, the
/// tables are ascending, and each record names its key and target by number.
#[test]
fn each_key_and_target_is_in_its_table_once() {
    let bytes = list(vec![
        file("f1", 1, Some(("pk/b", 0)), Some([1; 32])),
        file("f2", 1, Some(("pk/a", 0)), Some([2; 32])),
        file("f3", 1, Some(("pk/b", 1)), Some([3; 32])),
        link("l1", "z"),
        link("l2", "a"),
        link("l3", "z"),
    ])
    .encode_v2()
    .unwrap();
    let opened = TreeListV2::open(&bytes).unwrap();
    let keys: Vec<&str> = opened.keys.iter().map(|k| &**k).collect();
    assert_eq!(keys, ["pk/a", "pk/b"]);
    assert_eq!(opened.targets.len(), 2);
    assert_eq!(opened.targets.get(0), "a");
    assert_eq!(opened.targets.get(1), "z");
    let rows = rows(&bytes);
    let links: Vec<u32> = rows
        .iter()
        .filter(|r| r.kind == KIND_SYMLINK)
        .map(|r| r.key)
        .collect();
    assert_eq!(links, [1, 0, 1]);
    let files: Vec<(&str, u32, u64)> = rows
        .iter()
        .filter(|r| r.kind == KIND_FILE)
        .map(|r| (r.name.as_str(), r.key, r.offset))
        .collect();
    assert_eq!(files, [("f2", 0, 0), ("f1", 1, 0), ("f3", 1, 1)]);
}

/// Asserts that a name used in several directories is in the name table once.
#[test]
fn a_name_in_several_directories_is_in_the_table_once() {
    let bytes = list(vec![
        file("a/x", 1, Some(("k", 0)), Some([1; 32])),
        file("b/x", 1, Some(("k", 1)), Some([2; 32])),
        link("x", "a"),
    ])
    .encode_v2()
    .unwrap();
    let opened = TreeListV2::open(&bytes).unwrap();
    assert_eq!(opened.names.len(), 3);
    let all: Vec<&str> = (0..3).map(|i| opened.names.get(i)).collect();
    assert_eq!(all, ["a", "b", "x"]);
}

/// Asserts that a link has mode 0 and no size, offset or digest, and a directory has no key,
/// size, offset or digest: every ignored field is zero (and a directory's key is `NONE`).
#[test]
fn ignored_fields_are_written_as_zero() {
    let bytes = list(vec![link("l", "t"), dir("d", 0o700)])
        .encode_v2()
        .unwrap();
    let rows = rows(&bytes);
    assert_eq!(
        rows[0],
        Row {
            kind: KIND_DIR,
            pad: 0,
            mode: 0o700,
            name: "d".into(),
            parent: NONE,
            key: NONE,
            size: 0,
            offset: 0,
            sha: [0; 32],
        }
    );
    assert_eq!(
        rows[1],
        Row {
            kind: KIND_SYMLINK,
            pad: 0,
            mode: 0,
            name: "l".into(),
            parent: NONE,
            key: 0,
            size: 0,
            offset: 0,
            sha: [0; 32],
        }
    );
}

/// Asserts that a file of size 0 with no `sha256` is written with the SHA-256 of no bytes and no
/// key, and one that gives a `sha256` keeps it.
#[test]
fn an_empty_file_without_a_digest_gets_the_digest_of_no_bytes() {
    let bytes = list(vec![
        file("a", 0, None, None),
        file("b", 0, None, Some([5; 32])),
    ])
    .encode_v2()
    .unwrap();
    let rows = rows(&bytes);
    assert_eq!(
        (rows[0].key, rows[0].size, rows[0].sha),
        (NONE, 0, EMPTY_SHA256)
    );
    assert_eq!((rows[1].key, rows[1].sha), (NONE, [5; 32]));
    assert_eq!(
        EMPTY_SHA256,
        *TreeDigest::of(b"").as_bytes(),
        "the digest of no bytes"
    );
}

/// Asserts that a file of size above 0 with no `sha256` is refused with its path, with the exact
/// message.
#[test]
fn a_file_with_bytes_and_no_digest_is_refused() {
    let error = list(vec![
        file("ok", 1, Some(("k", 0)), Some(SHA)),
        file("d/no-digest", 5, Some(("k", 1)), None),
    ])
    .encode_v2()
    .unwrap_err();
    assert_eq!(error, TreeListError::MissingSha256("d/no-digest".into()));
    assert_eq!(error.to_string(), "file \"d/no-digest\" has no sha256");
}

/// Asserts that a list whose bytes would pass the largest size is refused with the size it would
/// have and the bound, and that one a few bytes under is written: a single link target is bounded
/// only by the reader.
#[test]
fn a_list_over_the_largest_size_is_refused() {
    // header 40 + keys 4 + names (4 + 4 + 1) + targets (4 + 4 + n) + one record of 64.
    let fixed = 40 + 4 + 9 + 8 + 64;
    let target = "t".repeat(MAX_TREE_LIST_BYTES - fixed + 1);
    let error = list(vec![link("l", &target)]).encode_v2().unwrap_err();
    assert_eq!(
        error,
        TreeListError::TooLarge {
            size: MAX_TREE_LIST_BYTES + 1,
            max: MAX_TREE_LIST_BYTES,
        }
    );
    assert_eq!(
        error.to_string(),
        "tree list too large: 268435457 bytes, at most 268435456"
    );
    let target = "t".repeat(MAX_TREE_LIST_BYTES - fixed);
    let bytes = list(vec![link("l", &target)]).encode_v2().unwrap();
    assert_eq!(bytes.len(), MAX_TREE_LIST_BYTES);
}

/// Asserts that what the writer writes reads back, open and build, as the tree `build_tree` makes
/// of the same entries: the same root digest, the same directory records, the same files and
/// sources.
#[test]
fn what_the_writer_writes_reads_back_as_the_same_tree() {
    let source = list(vec![
        file("bin/cc", 10, Some(("out/a1/0", 4096)), Some([1; 32])),
        file("bin/ld", 20, Some(("out/a1/0", 8192)), Some([2; 32])),
        file("bin/ld2", 20, Some(("out/a0/0", 0)), Some([2; 32])),
        file("share/doc/readme", 7, Some(("out/b0/1", 16)), Some([3; 32])),
        file("share/empty", 0, None, None),
        link("lib", "bin"),
        link("share/doc/up", "../.."),
        dir("tmp", 0o1777),
        dir("share/private", 0o700),
    ]);
    let bytes = source.encode_v2().unwrap();
    let opened = TreeListV2::open(&bytes).unwrap();
    assert_eq!(opened.len(), 12);
    let mut spans = Vec::new();
    let built = opened
        .build(&TreeLimits::default(), |span| spans.push(span))
        .unwrap();

    let want = build_tree(None, source.into_entries().unwrap(), &TreeLimits::default()).unwrap();
    assert_eq!(built.root, want.root);
    // The records are the same set; only the order of sibling directories may differ.
    assert_eq!(built.records.last().map(|r| r.0), Some(want.root));
    let mut got = built.records;
    let mut expected = want.records;
    got.sort_by_key(|(digest, _)| *digest.as_bytes());
    expected.sort_by_key(|(digest, _)| *digest.as_bytes());
    assert_eq!(got, expected);
    let mut files = built.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(files, want.files);
    // The digest [2; 32] is read once, for its first file in the order of the records.
    assert_eq!(spans.len(), 3);
}

/// Asserts that an empty list reads back as the tree of no entries.
#[test]
fn an_empty_list_reads_back_as_the_empty_tree() {
    let bytes = TreeList::default().encode_v2().unwrap();
    let mut spans = Vec::new();
    let built = TreeListV2::open(&bytes)
        .unwrap()
        .build(&TreeLimits::default(), |span| spans.push(span))
        .unwrap();
    let want = build_tree(None, Vec::new(), &TreeLimits::default()).unwrap();
    assert_eq!(built, want);
    assert_eq!(spans, Vec::new());
}

/// Asserts that bytes the writer makes of a list the reader would refuse are written as they
/// are (the writer judges nothing the reader judges), and the reader then refuses them: a path
/// twice is a tree the walk refuses.
#[test]
fn the_writer_writes_what_the_reader_refuses_and_the_reader_refuses_it() {
    let bytes = list(vec![
        file("a", 1, Some(("k", 0)), Some([1; 32])),
        file("a", 1, Some(("k", 1)), Some([2; 32])),
    ])
    .encode_v2()
    .unwrap();
    let error = TreeListV2::open(&bytes)
        .unwrap()
        .build(&TreeLimits::default(), |_| {})
        .unwrap_err();
    assert_eq!(
        error,
        TreeListV2Error::Tree(RecordError::Duplicate("a".into()))
    );
}

/// Asserts that the order of the names the writer keeps is by kind then name (the names helper
/// reads names back in record order).
#[test]
fn names_read_back_in_record_order() {
    let bytes = list(vec![
        file("f", 1, Some(("k", 0)), Some(SHA)),
        dir("d", 0o755),
        link("l", "d"),
    ])
    .encode_v2()
    .unwrap();
    assert_eq!(
        names(&bytes),
        [
            (KIND_DIR, "d".to_owned()),
            (KIND_SYMLINK, "l".to_owned()),
            (KIND_FILE, "f".to_owned())
        ]
    );
}
