//! The tree list, version 2 (the `TSTREE` binary format): the bytes of the first release's
//! example, their digest, the header and every entry record decoded field by field against a table
//! written by hand, and the same list as a later writer may write it.
//!
//! This is the format's first release, so there is no earlier version 2 reader to run on newer
//! lists; the newer fixture stands for the lists of a later writer (minor version 3, a flag set in
//! every entry record, and bytes after the last section: the three things "Changing the format" of
//! `docs/tree-manifest.md` lets a later writer do), which this reader must read to the same tree,
//! files and spans. The test that runs the previous release's reader on a newer list and the newer
//! reader on the previous release's arrives with the first change of the format, and starts from
//! these fixtures. The entries are those of `tree_list.rs`, and the version 1 reader and this one
//! make the same tree of them; each refuses the other's bytes.
//!
//! To regenerate `fixtures/tree_list_v2.bin` (only when the format's first release is still
//! unreleased): write `TreeList { entries: entries() }.encode_v2().unwrap()` to the file, set
//! `V2_DIGEST` to its SHA-256, and update the tables below. `fixtures/tree_list_v2_newer.bin` is
//! the golden with bytes 10..12 set to 3 (the minor version), byte 15 of each of the nine entry
//! records set to `0xa5`, and the 16 bytes `0xf0..=0xff` appended.

use tierstore::{
    BlobSource, ListEntry, ListKind, ListRange, ListSpan, Mode, TreeDigest, TreeLimits, TreeList,
    TreeListError, TreeListV2, TreeListV2Error, build_tree, parse_tree_list, tree_list_key,
};

const V2: &[u8] = include_bytes!("fixtures/tree_list_v2.bin");
const NEWER: &[u8] = include_bytes!("fixtures/tree_list_v2_newer.bin");
const V1: &[u8] = include_bytes!("fixtures/tree_list_v1.jsonl");
const MANIFEST_V1: &[u8] = include_bytes!("fixtures/tree_manifest_v1.json");

/// The digest of the golden's bytes. A tree list is named by this, so a change of one byte of the
/// fixture, which would be a change of what version 2 is, fails here first.
const V2_DIGEST: &str = "fec08016bb468cc39dc5155cada5ebfe6806cdb10b5d0cb58b7df724f6ac72dc";

fn digest(text: &str) -> TreeDigest {
    TreeDigest::from_hex(text).unwrap()
}

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

/// The entries of the golden: those of the version 1 fixture.
fn entries() -> Vec<ListEntry> {
    vec![
        ListEntry {
            path: "bin/cc".into(),
            kind: ListKind::File {
                mode: mode(0o755),
                size: 1_048_576,
                source: Some(BlobSource {
                    key: "out/a1/0".into(),
                    offset: 524_288,
                }),
                sha256: Some(digest(&"2".repeat(64))),
            },
        },
        ListEntry {
            path: "include/empty.h".into(),
            kind: ListKind::File {
                mode: mode(0o644),
                size: 0,
                source: None,
                sha256: None,
            },
        },
        ListEntry {
            path: "lib".into(),
            kind: ListKind::Symlink {
                target: "bin".into(),
            },
        },
        ListEntry {
            path: "share/doc.txt".into(),
            kind: ListKind::File {
                mode: mode(0o644),
                size: 12,
                source: Some(BlobSource {
                    key: format!("cas/l/{}", "3".repeat(64)).into(),
                    offset: 0,
                }),
                sha256: Some(digest(&"4".repeat(64))),
            },
        },
        ListEntry {
            path: "tmp".into(),
            kind: ListKind::Dir { mode: mode(0o1777) },
        },
    ]
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// One entry record, field by field.
#[derive(Debug, PartialEq, Eq)]
struct Rec {
    parent: u32,
    name: &'static str,
    x: u32,
    kind: u8,
    mode: u16,
    name_len: u8,
    a: u64,
    b: u64,
}

fn rec(parent: u32, name: &'static str, x: u32, kind: u8, mode: u16, a: u64, b: u64) -> Rec {
    Rec {
        parent,
        name,
        x,
        kind,
        mode,
        name_len: name.len() as u8,
        a,
        b,
    }
}

/// The nine entry records of the golden, breadth first: the root, its children (`bin`, `include`,
/// `lib`, `share`, `tmp`), then the children of `bin`, `include` and `share`. A directory's `x`
/// is its first child and `a` its number of children; a file's `x` is its key (0 is the object of
/// `share/doc.txt`, 1 `out/a1/0`), `a` its offset and `b` its size; a link's `x` is unused, `a` is
/// the offset of its target in the names and `b` its length.
fn expected_records() -> Vec<Rec> {
    vec![
        rec(0, "", 1, 1, 0o755, 5, 0),
        rec(0, "bin", 6, 1, 0o755, 1, 0),
        rec(0, "include", 7, 1, 0o755, 1, 0),
        rec(0, "lib", 0, 2, 0, 13, 3),
        rec(0, "share", 8, 1, 0o755, 1, 0),
        rec(0, "tmp", 9, 1, 0o1777, 0, 0),
        rec(1, "cc", 1, 0, 0o755, 524_288, 1_048_576),
        rec(2, "empty.h", 0, 0, 0o644, 0, 0),
        rec(4, "doc.txt", 0, 0, 0o644, 0, 12),
    ]
}

/// The entry records of `bytes`, decoded by the offsets of the reference, the names looked up in
/// the names section.
fn records_of(bytes: &[u8]) -> Vec<Rec> {
    let at = u32_at(bytes, 16) as usize;
    let count = u32_at(bytes, 20) as usize;
    let names_at = u32_at(bytes, 16 + 8 * 4) as usize;
    (0..count)
        .map(|i| {
            let r = at + 32 * i;
            let name_off = u32_at(bytes, r + 4) as usize;
            let name_len = bytes[r + 14];
            let name = std::str::from_utf8(
                &bytes[names_at + name_off..names_at + name_off + name_len as usize],
            )
            .unwrap();
            let kind_mode = u16_at(bytes, r + 12);
            Rec {
                parent: u32_at(bytes, r),
                // Leaked: a test's few names, to compare with the table's.
                name: Box::leak(name.to_string().into_boxed_str()),
                x: u32_at(bytes, r + 8),
                kind: (kind_mode >> 12) as u8,
                mode: kind_mode & 0o7777,
                name_len,
                a: u64_at(bytes, r + 16),
                b: u64_at(bytes, r + 24),
            }
        })
        .collect()
}

fn span(key: u32, offset: u64, size: u64, sha: char) -> ListSpan {
    ListSpan {
        key,
        offset,
        size,
        sha256: digest(&sha.to_string().repeat(64)),
    }
}

/// The spans the walk hands out for the golden: `bin/cc` (key 1, `out/a1/0`). `share/doc.txt`
/// (key 0, `cas/l/3..`, 12 bytes, digest of `4` repeated) has size above 0 too, but
/// The spans the golden's walk hands out: each file of size above 0, in the order of the entries.
fn expected_spans() -> Vec<ListSpan> {
    vec![span(1, 524_288, 1_048_576, '2'), span(0, 0, 12, '4')]
}

fn build(bytes: &[u8]) -> (tierstore::BuiltTree, Vec<ListSpan>) {
    let list = TreeListV2::open(bytes).unwrap();
    let mut spans = Vec::new();
    let built = list
        .build(&TreeLimits::default(), |s| spans.push(s))
        .unwrap();
    (built, spans)
}

/// Asserts that the fixture hashes to the pinned digest and is named by it, and that the five
/// entries of the version 1 example encode to exactly the fixture's bytes.
#[test]
fn the_version_2_example_is_written_byte_for_byte_and_named_by_its_digest() {
    assert_eq!(TreeDigest::of(V2), digest(V2_DIGEST));
    assert_eq!(tree_list_key(V2), format!("cas/l/{V2_DIGEST}"));
    assert_eq!(TreeList { entries: entries() }.encode_v2().unwrap(), V2);
}

/// Asserts that the header of the golden is the magic, major version 1, minor version 0, no flags,
/// and six sections at the offsets and counts the reference gives: nine entries at 64, nine digests
/// at 352, two key offsets (three words) at 640, 78 bytes of keys at 656, 40 bytes of names at 736
/// and two ranges at 776, which end the object at 824.
#[test]
fn the_header_of_the_example_is_pinned_field_by_field() {
    assert_eq!(&V2[..8], b"TSTREE\0\0");
    assert_eq!(u16_at(V2, 8), 1);
    assert_eq!(u16_at(V2, 10), 0);
    assert_eq!(u32_at(V2, 12), 0);
    let sections: Vec<(u32, u32)> = (0..6)
        .map(|i| (u32_at(V2, 16 + 8 * i), u32_at(V2, 20 + 8 * i)))
        .collect();
    assert_eq!(
        sections,
        vec![(64, 9), (352, 9), (640, 2), (656, 78), (736, 40), (776, 2)]
    );
    assert_eq!(V2.len(), 776 + 2 * 24);
}

/// Asserts that every entry record of the golden decodes, field by field, to the table written by
/// hand, so a change of layout fails with a readable difference of the records.
#[test]
fn the_entry_records_of_the_example_are_pinned_field_by_field() {
    assert_eq!(records_of(V2), expected_records());
    // The flags byte of each record is zero in what this writer writes.
    for i in 0..9 {
        assert_eq!(V2[64 + 32 * i + 15], 0, "flags of record {i}");
    }
}

/// Asserts that the digests, keys, names and ranges sections of the golden hold what the reference
/// says: zero digests for the directories and the link, the digest of no bytes for the empty file,
/// the keys sorted and back to back, the names once each with the link's target after its name,
/// and the two ranges, one per pack, that read the two files.
#[test]
fn the_other_sections_of_the_example_are_pinned() {
    let empty = TreeDigest::of(b"");
    let digests: Vec<&[u8]> = (0..9).map(|i| &V2[352 + 32 * i..384 + 32 * i]).collect();
    for (i, d) in digests.iter().enumerate() {
        let want: [u8; 32] = match i {
            6 => [0x22; 32],
            7 => *empty.as_bytes(),
            8 => [0x44; 32],
            _ => [0; 32],
        };
        assert_eq!(*d, want, "digest of entry {i}");
    }
    assert_eq!(
        [u32_at(V2, 640), u32_at(V2, 644), u32_at(V2, 648)],
        [0, 70, 78]
    );
    assert_eq!(
        &V2[656..734],
        format!("cas/l/{}out/a1/0", "3".repeat(64)).as_bytes()
    );
    assert_eq!(&V2[736..776], b"binincludelibbinsharetmpccempty.hdoc.txt");
    let ranges: Vec<(u32, u32, u64, u64)> = (0..2)
        .map(|i| {
            let r = 776 + 24 * i;
            (
                u32_at(V2, r),
                u32_at(V2, r + 4),
                u64_at(V2, r + 8),
                u64_at(V2, r + 16),
            )
        })
        .collect();
    assert_eq!(ranges, vec![(0, 0, 0, 12), (1, 0, 524_288, 1_048_576)]);
    let listed: Vec<ListRange> = TreeListV2::open(V2).unwrap().ranges().collect();
    assert_eq!(
        listed,
        vec![
            ListRange {
                key: 0,
                offset: 0,
                len: 12
            },
            ListRange {
                key: 1,
                offset: 524_288,
                len: 1_048_576
            },
        ]
    );
}

/// Asserts that the golden opens to nine entries and builds to the same root and the same
/// directory records as the version 1 example built from the same entries, with the same files
/// (version 2 lists them in the order of its entries, version 1 sorted by path), and hands out the
/// span of each file of size above 0 once, in the order of the entries.
#[test]
fn the_version_2_example_makes_the_tree_the_version_1_example_makes() {
    let (built, spans) = build(V2);
    assert_eq!(spans, expected_spans());
    assert_eq!(TreeListV2::open(V2).unwrap().len(), 9);

    let v1 = parse_tree_list(V1, &TreeDigest::of(V1)).unwrap();
    let from_v1 = build_tree(None, v1.into_entries().unwrap(), &TreeLimits::default()).unwrap();
    assert_eq!(built.root, from_v1.root);
    assert_eq!(built.records, from_v1.records);
    let mut files = built.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(files, from_v1.files);
}

/// Asserts that a later writer's list (minor version 3, a nonzero flags byte in every entry
/// record, 16 bytes after the last section) opens, has the same ranges, and builds to the same
/// root, records, files and spans as the golden.
#[test]
fn a_newer_list_with_a_minor_version_flags_and_trailing_bytes_reads_to_the_same_tree() {
    assert_eq!(NEWER.len(), V2.len() + 16);
    assert_eq!(u16_at(NEWER, 10), 3);
    for i in 0..9 {
        assert_ne!(NEWER[64 + 32 * i + 15], 0, "flags of record {i}");
    }
    assert_ne!(TreeDigest::of(NEWER), TreeDigest::of(V2));

    let (golden, golden_spans) = build(V2);
    let (newer, newer_spans) = build(NEWER);
    assert_eq!(newer, golden);
    assert_eq!(newer_spans, golden_spans);
    let ranges = |bytes| {
        TreeListV2::open(bytes)
            .unwrap()
            .ranges()
            .collect::<Vec<_>>()
    };
    assert_eq!(ranges(NEWER), ranges(V2));
    assert_eq!(records_of(NEWER).len(), 9);
}

/// Asserts that the two lists are never read as each other: the version 1 reader refuses the
/// version 2 bytes at line 2 (one byte of the records is a newline), which has no end, and the version 2 reader refuses the version 1
/// list and the tree manifest at the magic.
#[test]
fn the_two_versions_and_the_tree_manifest_are_never_read_as_each_other() {
    assert_eq!(
        parse_tree_list(V2, &TreeDigest::of(V2)),
        Err(TreeListError::Line {
            line: 2,
            fault: tierstore::LineFault::NoEnd
        })
    );
    assert_eq!(TreeListV2::open(V1).unwrap_err(), TreeListV2Error::BadMagic);
    assert_eq!(
        TreeListV2::open(MANIFEST_V1).unwrap_err(),
        TreeListV2Error::BadMagic
    );
    assert_eq!(
        TreeListV2Error::BadMagic.to_string(),
        "not a version 2 tree list: bad magic"
    );
}
