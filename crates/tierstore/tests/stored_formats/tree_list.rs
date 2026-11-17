//! The tree list, version 1: the bytes of the first release's example and their digest, and the
//! same list as a later writer may write it, with members this reader does not know.
//!
//! This is the format's first release, so there is no earlier reader to run on newer lists and no
//! earlier list to read; the newer fixture stands for the lists of a later writer, which this
//! reader must read to the same tree. The test that runs the previous release's reader on a newer
//! list and the newer reader on the previous release's arrives with the first change of the
//! format, and starts from these fixtures. The v1 tree manifest (`tree_manifest.rs`) is not
//! touched: its reader refuses a tree list (it has no `v`), and this reader refuses a manifest (it
//! has no `format`).

use tierstore::{
    BlobSource, LineFault, ListEntry, ListKind, Mode, TreeDigest, TreeError, TreeList,
    TreeListError, parse_manifest, parse_tree_list, tree_list_key,
};

const V1: &[u8] = include_bytes!("fixtures/tree_list_v1.jsonl");
const NEWER: &[u8] = include_bytes!("fixtures/tree_list_v1_newer.jsonl");
const MANIFEST_V1: &[u8] = include_bytes!("fixtures/tree_manifest_v1.json");

/// The digest of each fixture's bytes. A tree list is named by this, so a change of one byte of
/// the fixture, which would be a change of what version 1 is, fails here first.
const V1_DIGEST: &str = "29a47e905a35a65ce17e0a56133b556a9e4e396af3fd27574363377cb36dd548";
const NEWER_DIGEST: &str = "d28c8944ac921263226022db9c001672634eac4851963ae3a1822c549f7197bb";

fn digest(text: &str) -> TreeDigest {
    TreeDigest::from_hex(text).unwrap()
}

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

/// The entries of both fixtures.
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
                sha256: None,
            },
        },
        ListEntry {
            path: "tmp".into(),
            kind: ListKind::Dir { mode: mode(0o1777) },
        },
    ]
}

/// The fixture hashes to the pinned digest and is named by it, reads to its five entries (an
/// executable file with a source and a digest, an empty file, a link, a file with a source and no
/// digest, a sticky directory), and those entries encode to exactly the fixture's bytes.
#[test]
fn the_version_1_example_reads_to_its_entries_and_is_written_back_byte_for_byte() {
    assert_eq!(TreeDigest::of(V1), digest(V1_DIGEST));
    assert_eq!(tree_list_key(V1), format!("cas/l/{V1_DIGEST}"));
    let list = parse_tree_list(V1, &digest(V1_DIGEST)).unwrap();
    assert_eq!(list.entries, entries());
    assert_eq!(list.encode(), V1);
}

/// A later writer's list, with members this reader does not know in the header and in every kind,
/// reads to the same entries.
#[test]
fn a_newer_list_with_members_this_reader_does_not_know_reads_to_the_same_entries() {
    assert_eq!(TreeDigest::of(NEWER), digest(NEWER_DIGEST));
    let list = parse_tree_list(NEWER, &digest(NEWER_DIGEST)).unwrap();
    assert_eq!(list, TreeList { entries: entries() });
}

/// The two formats stored under `cas/l/` are never read as each other: the manifest reader refuses
/// the list, and the list reader refuses the manifest, each at its first line.
#[test]
fn a_tree_list_and_a_tree_manifest_are_never_read_as_each_other() {
    assert!(matches!(
        parse_manifest(V1, &TreeDigest::of(V1)),
        Err(TreeError::Malformed(_))
    ));
    assert_eq!(
        parse_tree_list(MANIFEST_V1, &TreeDigest::of(MANIFEST_V1)),
        Err(TreeListError::Line {
            line: 1,
            fault: LineFault::Missing {
                what: "header",
                member: "format"
            }
        })
    );
}
