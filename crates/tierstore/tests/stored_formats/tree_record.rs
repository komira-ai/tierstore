//! The tree record, version 1: the bytes of the first release's example, a root and the directory
//! it holds, and their digests.
//!
//! This is the format's first release, so there is no earlier reader to run on newer records and
//! no earlier record to read; the test that runs the previous release's reader on a newer record
//! and the newer reader on the previous release's arrives with the first change of the format, and
//! starts from these fixtures. The v1 tree manifest (`tree_manifest.rs`, beside this file) is not
//! touched by it: a record is a new object with a new magic, and `parse_manifest` still reads
//! every manifest it read.

use tierstore::{
    BlobSource, Mode, NewEntry, NewKind, RecordChild, RecordError, TreeDigest, TreeLimits,
    TreeRecord, build_tree,
};

const ROOT: &[u8] = include_bytes!("fixtures/tree_record_v1_root.bin");
const CHILD: &[u8] = include_bytes!("fixtures/tree_record_v1_child.bin");

/// The digest of each fixture's bytes. A tree is named by the root's, so a change of one byte of
/// either fixture, which would be a change of what version 1 is, fails here first.
const ROOT_DIGEST: &str = "704da45d28e77b0205d40810455f478caa92ca0b7d31271b49c0794ae901d899";
const CHILD_DIGEST: &str = "2713448e8faaa0c012486a0b2546a74aabff7052a8d1b2cc71e423e574c0100a";

fn digest(text: &str) -> TreeDigest {
    TreeDigest::from_hex(text).unwrap()
}

fn mode(bits: u16) -> Mode {
    Mode::new(bits).unwrap()
}

fn child_record() -> TreeRecord {
    TreeRecord::new(
        mode(0o1777),
        vec![RecordChild::File {
            name: "a.txt".into(),
            mode: mode(0o644),
            size: 3,
            sha256: TreeDigest::of(b"abc"),
        }],
    )
    .unwrap()
}

fn root_record() -> TreeRecord {
    TreeRecord::new(
        mode(0o755),
        vec![
            RecordChild::Dir {
                name: "bin".into(),
                digest: digest(CHILD_DIGEST),
            },
            RecordChild::File {
                name: "cc".into(),
                mode: mode(0o755),
                size: 1_048_576,
                sha256: digest(&"22".repeat(32)),
            },
            RecordChild::File {
                name: "empty.h".into(),
                mode: mode(0o644),
                size: 0,
                sha256: TreeDigest::of(b""),
            },
            RecordChild::Symlink {
                name: "lib".into(),
                target: "bin".into(),
            },
            RecordChild::File {
                name: "su".into(),
                mode: mode(0o4755),
                size: 16,
                sha256: digest(&"33".repeat(32)),
            },
        ],
    )
    .unwrap()
}

/// The fixtures hash to the pinned digests, are read to the values the test states, and the
/// current code writes those values back to the same bytes.
#[test]
fn the_version_1_records_hash_to_their_digests_and_read_and_write_back() {
    assert_eq!(TreeDigest::of(ROOT), digest(ROOT_DIGEST));
    assert_eq!(TreeDigest::of(CHILD), digest(CHILD_DIGEST));
    assert_eq!(TreeRecord::parse(ROOT).unwrap(), root_record());
    assert_eq!(TreeRecord::parse(CHILD).unwrap(), child_record());
    assert_eq!(root_record().encode(), ROOT);
    assert_eq!(child_record().encode(), CHILD);
    assert_eq!(root_record().digest(), digest(ROOT_DIGEST));
}

/// The same tree from a request, with sources and in no order, is the fixture's tree: its digest
/// and its two records are the pinned ones.
#[test]
fn a_request_for_the_version_1_tree_builds_the_fixture_records() {
    let file = |path: &str, mode: Option<Mode>, size: u64, sha256: TreeDigest, from| NewEntry {
        path: path.into(),
        kind: NewKind::File {
            mode,
            sha256,
            size,
            from,
        },
    };
    let entries = vec![
        NewEntry {
            path: "lib".into(),
            kind: NewKind::Symlink {
                target: "bin".into(),
            },
        },
        file(
            "su",
            Some(mode(0o4755)),
            16,
            digest(&"33".repeat(32)),
            Some(BlobSource {
                key: "cas/d/n7/b2/0000000000000041".into(),
                offset: 524_288,
            }),
        ),
        file("empty.h", None, 0, TreeDigest::of(b""), None),
        file(
            "cc",
            Some(mode(0o755)),
            1_048_576,
            digest(&"22".repeat(32)),
            None,
        ),
        file("bin/a.txt", None, 3, TreeDigest::of(b"abc"), None),
        NewEntry {
            path: "bin".into(),
            kind: NewKind::Dir {
                mode: Some(mode(0o1777)),
            },
        },
    ];
    let built = build_tree(None, entries, &TreeLimits::default()).unwrap();
    assert_eq!(built.root, digest(ROOT_DIGEST));
    let bytes: Vec<_> = built.records.iter().map(|(_, r)| r.encode()).collect();
    assert_eq!(bytes, vec![CHILD.to_vec(), ROOT.to_vec()]);
}

/// The fixture cut anywhere, or with a byte added, is refused, so no other bytes read as the
/// same tree.
#[test]
fn the_version_1_root_is_refused_when_cut_or_extended() {
    for len in 0..ROOT.len() {
        assert!(TreeRecord::parse(&ROOT[..len]).is_err(), "cut at {len}");
    }
    let mut longer = ROOT.to_vec();
    longer.push(0);
    assert_eq!(TreeRecord::parse(&longer), Err(RecordError::TrailingBytes));
}
