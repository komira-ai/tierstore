//! The tree list, version 2: the bytes of the first release's example and their digest, the same
//! list as a later writer may write it, with a longer header and longer records, and the tree both
//! versions of the list make.
//!
//! Pinned here: the bytes of `fixtures/tree_list_v2.bin` (the entries of `tree_list_v1.jsonl`,
//! written by `TreeList::encode_v2`) and their SHA-256; every field of the header and of every
//! record, written out by hand below so that a change of the layout fails with a readable diff;
//! the spans a walk hands out; and that the version 1 and the version 2 list of the same entries
//! make the same tree digest and the same records. The two versions refuse each other's bytes, and
//! neither is read as a v1 tree manifest.
//!
//! This is the first release of version 2, so there is no earlier version 2 reader to run on newer
//! lists and no earlier list to read; `tree_list_v2_newer.bin` stands for a later writer (a header
//! of 48 bytes and records of 72, each with 8 more nonzero bytes at its end, every offset moved),
//! which this reader must read to the same tree and spans. The test that runs the previous
//! release's reader on newer lists arrives with the first change of the format and starts from
//! these fixtures.
//!
//! To regenerate `tree_list_v2.bin` (only when the format is deliberately changed, which is a new
//! version): write `TreeList { entries: entries() }.encode_v2()` below to the file and update
//! `V2_DIGEST`. `tree_list_v2_newer.bin` is that file with the header length (offset 12) set to 48
//! and the record length (offset 16) to 72, the four section offsets (24 to 36) each raised by 8,
//! 8 nonzero bytes inserted at offset 40, and 8 nonzero bytes appended to each record.

use tierstore::{
    BlobSource, LineFault, ListEntry, ListKind, ListSpan, Mode, TreeDigest, TreeLimits, TreeList,
    TreeListError, TreeListV2, TreeListV2Error, build_tree, is_tree_list_v2, parse_tree_list,
    tree_list_key,
};

const V1: &[u8] = include_bytes!("fixtures/tree_list_v1.jsonl");
const V2: &[u8] = include_bytes!("fixtures/tree_list_v2.bin");
const V2_NEWER: &[u8] = include_bytes!("fixtures/tree_list_v2_newer.bin");
const MANIFEST_V1: &[u8] = include_bytes!("fixtures/tree_manifest_v1.json");

/// The digests of the version 1 example's and of the golden's bytes. A tree list is named by this, so a change of one byte of the
/// fixture, which would be a change of what version 2 is, fails here first.
const V1_DIGEST: &str = "023648314980adbd93d7c8645877befef1fe92c36b0cd3cc61d84ec2e31f1c40";
const V2_DIGEST: &str = "a32f3139176563689443d729863cd44b5d226d9ca18779c673eb3314b0f4f9c0";
const V2_NEWER_DIGEST: &str = "314dfe59b9f0ad72b8759c11e1d9f92af69047d862e395e2c3a95769fde97672";

/// The index that names nothing.
const NONE: u32 = u32::MAX;

fn digest(text: &str) -> TreeDigest {
    TreeDigest::from_hex(text).unwrap()
}

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

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

/// One record of the golden, as the layout of the format says it.
struct Expect {
    kind: u8,
    mode: u16,
    name: u32,
    parent: u32,
    key: u32,
    size: u64,
    offset: u64,
    sha256: [u8; 32],
}

/// The records of the golden: four directories (bin, include, share, tmp), the link `lib`, then
/// the files by key and offset (the whole object `cas/l/33..`, then `out/a1/0`), the empty one last.
fn expected_records() -> Vec<Expect> {
    let dir = |mode, name| Expect {
        kind: 1,
        mode,
        name,
        parent: NONE,
        key: NONE,
        size: 0,
        offset: 0,
        sha256: [0; 32],
    };
    vec![
        dir(0o755, 0),
        dir(0o755, 4),
        dir(0o755, 6),
        dir(0o1777, 7),
        Expect {
            kind: 2,
            mode: 0,
            name: 5,
            parent: NONE,
            key: 0,
            size: 0,
            offset: 0,
            sha256: [0; 32],
        },
        Expect {
            kind: 3,
            mode: 0o644,
            name: 2,
            parent: 2,
            key: 0,
            size: 12,
            offset: 0,
            sha256: [0x44; 32],
        },
        Expect {
            kind: 3,
            mode: 0o755,
            name: 1,
            parent: 0,
            key: 1,
            size: 1_048_576,
            offset: 524_288,
            sha256: [0x22; 32],
        },
        Expect {
            kind: 3,
            mode: 0o644,
            name: 3,
            parent: 1,
            key: NONE,
            size: 0,
            offset: 0,
            sha256: [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ],
        },
    ]
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The strings of the table at `at`: its count, its ends, then the bytes.
fn table(bytes: &[u8], at: usize) -> Vec<String> {
    let count = u32_at(bytes, at) as usize;
    let data = at + 4 + count * 4;
    let mut start = 0;
    (0..count)
        .map(|i| {
            let end = u32_at(bytes, at + 4 + i * 4) as usize;
            let text = std::str::from_utf8(&bytes[data + start..data + end]).unwrap();
            start = end;
            text.to_string()
        })
        .collect()
}

fn built(bytes: &[u8]) -> (tierstore::BuiltTree, Vec<ListSpan>) {
    let list = TreeListV2::open(bytes).unwrap();
    let mut spans = Vec::new();
    let tree = list
        .build(&TreeLimits::default(), |span| spans.push(span))
        .unwrap();
    (tree, spans)
}

/// The golden hashes to the pinned digest and is named by it, and the entries of the version 1
/// example encode to exactly its bytes.
#[test]
fn the_version_2_example_is_written_byte_for_byte_and_named_by_its_digest() {
    assert_eq!(TreeDigest::of(V2), digest(V2_DIGEST));
    assert_eq!(tree_list_key(V2), format!("cas/l/{V2_DIGEST}"));
    assert!(is_tree_list_v2(V2));
    assert!(!is_tree_list_v2(V1));
    let list = TreeList { entries: entries() };
    assert_eq!(list.encode_v2().unwrap(), V2);
}

/// The same entries in the reverse order are written as the same bytes, so one tree has one key.
#[test]
fn the_entries_in_any_order_are_written_as_the_same_bytes() {
    let mut reversed = entries();
    reversed.reverse();
    assert_eq!(TreeList { entries: reversed }.encode_v2().unwrap(), V2);
}

/// The header of the golden says: version 2, header 40 bytes, records 64 bytes, 8 records, and the
/// four sections one after another with nothing between them; the tables hold the sorted keys,
/// names and targets once each.
#[test]
fn the_golden_header_and_tables_are_what_the_format_says() {
    assert_eq!(&V2[..8], b"TSLIST\r\n");
    assert_eq!(u32_at(V2, 8), 2);
    assert_eq!(u32_at(V2, 12), 40);
    assert_eq!(u32_at(V2, 16), 64);
    assert_eq!(u32_at(V2, 20), 8);
    assert_eq!(u32_at(V2, 24), 40);
    assert_eq!(u32_at(V2, 28), 130);
    assert_eq!(u32_at(V2, 32), 203);
    assert_eq!(u32_at(V2, 36), 214);
    assert_eq!(V2.len(), 214 + 8 * 64);
    assert_eq!(
        table(V2, 40),
        vec![format!("cas/l/{}", "3".repeat(64)), "out/a1/0".to_string()]
    );
    assert_eq!(
        table(V2, 130),
        [
            "bin", "cc", "doc.txt", "empty.h", "include", "lib", "share", "tmp"
        ]
    );
    assert_eq!(table(V2, 203), ["bin"]);
}

/// Every field of every record of the golden is what the layout says: the kind at 0, the mode at
/// 2, the name at 4, the parent at 8, the key or target at 12, the size at 16, the offset at 24,
/// the SHA-256 at 32, and the byte at 1 zero.
#[test]
fn the_golden_records_are_read_field_by_field() {
    let expected = expected_records();
    for (i, want) in expected.iter().enumerate() {
        let at = 214 + i * 64;
        let record = &V2[at..at + 64];
        assert_eq!(record[0], want.kind, "record {i} kind");
        assert_eq!(record[1], 0, "record {i} reserved");
        assert_eq!(
            u16::from_le_bytes([record[2], record[3]]),
            want.mode,
            "record {i} mode"
        );
        assert_eq!(u32_at(record, 4), want.name, "record {i} name");
        assert_eq!(u32_at(record, 8), want.parent, "record {i} parent");
        assert_eq!(u32_at(record, 12), want.key, "record {i} key");
        assert_eq!(u64_at(record, 16), want.size, "record {i} size");
        assert_eq!(u64_at(record, 24), want.offset, "record {i} offset");
        assert_eq!(&record[32..64], &want.sha256, "record {i} sha256");
    }
    assert_eq!(expected.len(), 8);
}

/// The golden builds to the tree of its entries, and its walk hands out one span for each file
/// with bytes, pack by pack, and none for the empty file.
#[test]
fn the_golden_builds_and_hands_out_its_spans_in_pack_order() {
    let (tree, spans) = built(V2);
    assert_eq!(
        spans,
        vec![
            ListSpan {
                key: format!("cas/l/{}", "3".repeat(64)).into(),
                offset: 0,
                size: 12,
                sha256: digest(&"4".repeat(64)),
            },
            ListSpan {
                key: "out/a1/0".into(),
                offset: 524_288,
                size: 1_048_576,
                sha256: digest(&"2".repeat(64)),
            },
        ]
    );
    assert_eq!(tree.files.len(), 3);
}

/// A later writer's list, with a header of 48 bytes and records of 72, each with nonzero bytes
/// this reader does not know, builds to the same tree and hands out the same spans.
#[test]
fn a_newer_list_with_a_longer_header_and_longer_records_builds_to_the_same_tree() {
    assert_eq!(TreeDigest::of(V2_NEWER), digest(V2_NEWER_DIGEST));
    assert_eq!(u32_at(V2_NEWER, 12), 48);
    assert_eq!(u32_at(V2_NEWER, 16), 72);
    assert_ne!(&V2_NEWER[40..48], &[0; 8]);
    let (tree, spans) = built(V2);
    let (newer_tree, newer_spans) = built(V2_NEWER);
    assert_eq!(newer_tree, tree);
    assert_eq!(newer_spans, spans);
}

/// The version 1 list and the version 2 list of the same entries make the same tree digest, the
/// same records of directories and the same files.
#[test]
fn the_version_1_and_the_version_2_list_of_the_same_entries_make_the_same_tree() {
    let list = parse_tree_list(V1, &digest(V1_DIGEST)).unwrap();
    let mut v1 = build_tree(None, list.into_entries().unwrap(), &TreeLimits::default()).unwrap();
    let (mut v2, _) = built(V2);
    assert_eq!(v1.root, v2.root);
    assert_eq!(v1.records, v2.records);
    v1.files.sort_by(|a, b| a.path.cmp(&b.path));
    v2.files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(v1.files, v2.files);
}

/// The version 1 reader refuses the bytes of version 2 at its first line, and the version 2
/// reader refuses a version 1 list and a tree manifest as not its magic.
#[test]
fn the_two_versions_and_the_manifest_are_never_read_as_each_other() {
    assert_eq!(
        parse_tree_list(V2, &TreeDigest::of(V2)),
        Err(TreeListError::Line {
            line: 2,
            fault: LineFault::NoEnd
        })
    );
    for bytes in [V1, MANIFEST_V1] {
        let error = TreeListV2::open(bytes).unwrap_err();
        assert_eq!(error, TreeListV2Error::BadMagic);
        assert_eq!(error.to_string(), "not a version 2 tree list: bad magic");
    }
}
