//! What the outputs of a result make below the checkout, and the results that are refused
//! before anything is written.

use super::*;
use crate::mac_farm::proto::reapi::{
    DirectoryNode, FileNode, OutputDirectory, OutputFile, OutputSymlink, SymlinkNode,
};

fn declared(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

fn file_node(name: &str, bytes: &[u8], executable: bool) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest_of(bytes)),
        is_executable: executable,
    }
}

/// The tree of an output directory whose root is `root` and whose other directories are
/// `children`, encoded, and its digest: what the farm stores for it.
fn tree_of(root: &Directory, children: &[Directory]) -> (Vec<u8>, Digest) {
    let raw = RawTree {
        root: root.encode_to_vec(),
        children: children.iter().map(prost::Message::encode_to_vec).collect(),
    };
    let bytes = raw.encode_to_vec();
    let digest = digest_of(&bytes);
    (bytes, digest)
}

fn dir_node(name: &str, dir: &Directory) -> DirectoryNode {
    DirectoryNode {
        name: name.to_owned(),
        digest: Some(digest_of(&dir.encode_to_vec())),
    }
}

/// `dist/` with a file and an empty subdirectory, and the result that names it.
fn dist() -> (ActionResult, BTreeMap<String, Vec<u8>>) {
    let empty = Directory::default();
    let root = Directory {
        files: vec![file_node("a.tar.gz", b"tar", false)],
        directories: vec![dir_node("sub", &empty)],
        symlinks: Vec::new(),
    };
    let (bytes, digest) = tree_of(&root, &[empty]);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest.clone()),
        }],
        ..ActionResult::default()
    };
    (result, BTreeMap::from([(digest.hash, bytes)]))
}

/// Catches an output directory's files lost, its empty directories not made, a child placed
/// before its parent, an executable bit lost, and a file the farm sent inline read from the CAS
/// instead (or the CAS's digest dropped, so the inline bytes go unchecked).
#[test]
fn a_result_makes_its_directories_before_their_files() {
    let (mut result, trees) = dist();
    result.output_files.push(OutputFile {
        path: "pkg/t.pkg".to_owned(),
        digest: Some(digest_of(b"pkg")),
        is_executable: true,
        contents: b"pkg".to_vec(),
    });
    result.output_files.push(OutputFile {
        path: "pkg/notes".to_owned(),
        digest: Some(digest_of(b"n")),
        is_executable: false,
        contents: Vec::new(),
    });
    let mut wanted = declared(&["dist", "pkg/t.pkg", "pkg/notes"]);
    wanted.sort();
    let items = plan(&result, &wanted, &trees).expect("a plan");
    assert_eq!(
        items,
        [
            Item::File {
                path: "pkg/t.pkg".to_owned(),
                digest: digest_of(b"pkg"),
                inline: Some(b"pkg".to_vec()),
                executable: true,
            },
            Item::File {
                path: "pkg/notes".to_owned(),
                digest: digest_of(b"n"),
                inline: None,
                executable: false,
            },
            Item::Dir {
                path: "dist".to_owned()
            },
            Item::File {
                path: "dist/a.tar.gz".to_owned(),
                digest: digest_of(b"tar"),
                inline: None,
                executable: false,
            },
            Item::Dir {
                path: "dist/sub".to_owned()
            },
        ]
    );
    assert_eq!(tree_digests(&result).expect("digests").len(), 1);
}

/// Catches a farm that returns a path outside the checkout, an absolute one, one no `--output`
/// declared (a sibling whose name starts like a declared one included), or a declared output it
/// did not make, taken as written.
#[test]
fn a_path_the_command_did_not_declare_is_refused() {
    for (path, want) in [
        ("../x", OutputError::BadPath("../x".to_owned())),
        ("/etc/x", OutputError::BadPath("/etc/x".to_owned())),
        ("dist/./x", OutputError::BadPath("dist/./x".to_owned())),
        ("distx", OutputError::Undeclared("distx".to_owned())),
        ("other", OutputError::Undeclared("other".to_owned())),
    ] {
        let result = ActionResult {
            output_files: vec![OutputFile {
                path: path.to_owned(),
                digest: Some(digest_of(b"")),
                ..OutputFile::default()
            }],
            ..ActionResult::default()
        };
        assert_eq!(
            plan(&result, &declared(&["dist"]), &BTreeMap::new()),
            Err(want),
            "{path}"
        );
    }
    let (result, trees) = dist();
    assert_eq!(
        plan(&result, &declared(&["dist", "pkg"]), &trees),
        Err(OutputError::Missing("pkg".to_owned()))
    );
}

/// Catches a symlink copied out, as an output of its own (any of the three lists REAPI has for
/// them) or inside an output directory, where it could name a file outside the outputs.
#[test]
fn a_symlink_is_refused() {
    let link = || OutputSymlink {
        path: "dist/l".to_owned(),
        target: "/etc/passwd".to_owned(),
    };
    for result in [
        ActionResult {
            output_symlinks: vec![link()],
            ..ActionResult::default()
        },
        ActionResult {
            output_file_symlinks: vec![link()],
            ..ActionResult::default()
        },
        ActionResult {
            output_directory_symlinks: vec![link()],
            ..ActionResult::default()
        },
    ] {
        assert_eq!(
            plan(&result, &declared(&["dist"]), &BTreeMap::new()),
            Err(OutputError::Symlink("dist/l".to_owned()))
        );
    }
    let root = Directory {
        symlinks: vec![SymlinkNode {
            name: "l".to_owned(),
            target: "../../x".to_owned(),
        }],
        ..Directory::default()
    };
    let (bytes, digest) = tree_of(&root, &[]);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest.clone()),
        }],
        ..ActionResult::default()
    };
    let trees = BTreeMap::from([(digest.hash, bytes)]);
    assert_eq!(
        plan(&result, &declared(&["dist"]), &trees),
        Err(OutputError::Symlink("dist/l".to_owned()))
    );
}

/// Catches a tree whose names climb out of the output directory or nest a path in one name, a
/// node that names a directory the tree lacks, a tree that does not decode, and an output with
/// no digest written from nothing.
#[test]
fn a_tree_that_is_not_whole_or_plain_is_refused() {
    for name in ["..", ".", "", "a/b"] {
        let root = Directory {
            files: vec![file_node(name, b"x", false)],
            ..Directory::default()
        };
        let (bytes, digest) = tree_of(&root, &[]);
        let result = ActionResult {
            output_directories: vec![OutputDirectory {
                path: "dist".to_owned(),
                tree_digest: Some(digest.clone()),
            }],
            ..ActionResult::default()
        };
        let trees = BTreeMap::from([(digest.hash, bytes)]);
        assert_eq!(
            plan(&result, &declared(&["dist"]), &trees),
            Err(OutputError::BadName {
                path: "dist".to_owned(),
                name: name.to_owned()
            }),
            "{name:?}"
        );
    }
    let lacking = Directory {
        directories: vec![dir_node("sub", &Directory::default())],
        ..Directory::default()
    };
    let (bytes, digest) = tree_of(&lacking, &[]);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest.clone()),
        }],
        ..ActionResult::default()
    };
    let trees = BTreeMap::from([(digest.hash.clone(), bytes)]);
    assert_eq!(
        plan(&result, &declared(&["dist"]), &trees),
        Err(OutputError::NoDirectory {
            path: "dist".to_owned(),
            hash: digest_of(&Directory::default().encode_to_vec()).hash
        })
    );
    assert_eq!(
        plan(&result, &declared(&["dist"]), &BTreeMap::new()),
        Err(OutputError::NoDirectory {
            path: "dist".to_owned(),
            hash: digest.hash
        })
    );
    let garbage = BTreeMap::from([(digest_of(b"\xff").hash, b"\xff".to_vec())]);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest_of(b"\xff")),
        }],
        ..ActionResult::default()
    };
    assert!(matches!(
        plan(&result, &declared(&["dist"]), &garbage),
        Err(OutputError::Tree { .. })
    ));
    let no_tree = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: None,
        }],
        ..ActionResult::default()
    };
    let no_digest = OutputError::NoDigest {
        path: "dist".to_owned(),
    };
    assert_eq!(tree_digests(&no_tree), Err(no_digest.clone()));
    assert_eq!(
        plan(&no_tree, &declared(&["dist"]), &BTreeMap::new()),
        Err(no_digest.clone())
    );
    let no_file_digest = ActionResult {
        output_files: vec![OutputFile {
            path: "dist".to_owned(),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    assert_eq!(
        plan(&no_file_digest, &declared(&["dist"]), &BTreeMap::new()),
        Err(no_digest)
    );
    let unhashed = Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: None,
            is_executable: false,
        }],
        ..Directory::default()
    };
    let (bytes, digest) = tree_of(&unhashed, &[]);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest.clone()),
        }],
        ..ActionResult::default()
    };
    assert_eq!(
        plan(
            &result,
            &declared(&["dist"]),
            &BTreeMap::from([(digest.hash, bytes)])
        ),
        Err(OutputError::NoDigest {
            path: "dist/f".to_owned()
        })
    );
}

/// Catches a child directory read by the digest of its re-encoded message, which differs from
/// the farm's bytes when they hold a field this client does not know: the digest is of the bytes
/// as sent. A child whose bytes do not decode is refused.
#[test]
fn a_child_is_found_by_the_digest_of_its_bytes_as_sent() {
    let mut child = Directory {
        files: vec![file_node("f", b"x", false)],
        ..Directory::default()
    }
    .encode_to_vec();
    // An unknown field (number 5, `node_properties` in the full API): kept in the bytes, dropped
    // by a decode and encode.
    child.extend([0x2a, 0x00]);
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(digest_of(&child)),
        }],
        ..Directory::default()
    };
    let raw = RawTree {
        root: root.encode_to_vec(),
        children: vec![child],
    }
    .encode_to_vec();
    let digest = digest_of(&raw);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest.clone()),
        }],
        ..ActionResult::default()
    };
    let items = plan(
        &result,
        &declared(&["dist"]),
        &BTreeMap::from([(digest.hash, raw)]),
    )
    .expect("a plan");
    let paths: Vec<&str> = items.iter().map(Item::path).collect();
    assert_eq!(paths, ["dist", "dist/sub", "dist/sub/f"]);

    let broken = vec![0xff];
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(digest_of(&broken)),
        }],
        ..Directory::default()
    };
    let raw = RawTree {
        root: root.encode_to_vec(),
        children: vec![broken],
    }
    .encode_to_vec();
    let digest = digest_of(&raw);
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dist".to_owned(),
            tree_digest: Some(digest.clone()),
        }],
        ..ActionResult::default()
    };
    assert!(matches!(
        plan(
            &result,
            &declared(&["dist"]),
            &BTreeMap::from([(digest.hash, raw)])
        ),
        Err(OutputError::Tree { .. })
    ));
}

/// Catches one path written twice: as an output file and inside an output directory.
#[test]
fn a_path_given_twice_is_refused() {
    let (mut result, trees) = dist();
    result.output_files.push(OutputFile {
        path: "dist/a.tar.gz".to_owned(),
        digest: Some(digest_of(b"tar")),
        ..OutputFile::default()
    });
    assert_eq!(
        plan(&result, &declared(&["dist"]), &trees),
        Err(OutputError::Twice("dist/a.tar.gz".to_owned()))
    );
}
