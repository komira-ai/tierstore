//! The tree manifest, version 1: the bytes of the first release's example and their digest.
//!
//! This is the format's first release, so there is no earlier reader to run on newer manifests
//! and no earlier manifest to read; the test that runs the previous release's reader on a newer
//! manifest and the newer reader on the previous release's arrives with the first change of the
//! format, and starts from this fixture.

use tierstore::{BlobSource, EntryKind, TreeDigest, TreeError, parse_manifest};

const V1: &[u8] = include_bytes!("fixtures/tree_manifest_v1.json");

/// The digest of the fixture's bytes. A manifest is named by this, so a change of one byte of the
/// fixture, which would be a change of what version 1 is, fails here first.
const V1_DIGEST: &str = "e5ece36930f22ab106798ba275bfbb69a1d1fd6638f5ac652c792b76226237e3";

fn digest(text: &str) -> TreeDigest {
    TreeDigest::from_hex(text).unwrap()
}

/// The fixture hashes to the pinned digest, and is read to its four children: a directory, an
/// executable file with a source, an empty file, a symlink.
#[test]
fn the_version_1_example_hashes_to_its_digest_and_reads_to_its_children() {
    assert_eq!(TreeDigest::of(V1), digest(V1_DIGEST));
    let manifest = parse_manifest(V1, &digest(V1_DIGEST)).unwrap();
    let children: Vec<_> = manifest
        .children
        .iter()
        .map(|c| (c.name.to_string(), c.kind.clone()))
        .collect();
    assert_eq!(
        children,
        vec![
            (
                "bin".to_string(),
                EntryKind::Dir {
                    digest: digest(&"1".repeat(64)),
                    size: 812
                }
            ),
            (
                "cc".to_string(),
                EntryKind::File {
                    digest: digest(&"2".repeat(64)),
                    size: 1_048_576,
                    exec: true,
                    source: Some(BlobSource {
                        key: "cas/d/n7/b2/0000000000000041".into(),
                        offset: 524_288
                    }),
                }
            ),
            (
                "empty.h".to_string(),
                EntryKind::File {
                    digest: digest(
                        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    ),
                    size: 0,
                    exec: false,
                    source: None,
                }
            ),
            (
                "lib".to_string(),
                EntryKind::Symlink {
                    target: "bin".into()
                }
            ),
        ]
    );
}

/// The same bytes asked for under another digest are refused, so a stored manifest cannot be
/// served as another tree.
#[test]
fn the_version_1_example_is_refused_under_another_digest() {
    let other = digest(&"0".repeat(64));
    assert!(matches!(
        parse_manifest(V1, &other),
        Err(TreeError::DigestMismatch { .. })
    ));
}
