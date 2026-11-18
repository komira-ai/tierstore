//! The tree of a list: its inodes and what its lookups, reads and listings answer. The file
//! system over a real mount is in `tests/nullfs_integration.rs`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use fuser::{Errno, FileType};

use super::{Node, Tree};
use crate::list::{Entry, FileList};

fn list(paths: &[(&str, u64)]) -> FileList {
    FileList {
        files: paths
            .iter()
            .map(|&(path, size)| Entry {
                path: path.to_owned(),
                size,
            })
            .collect(),
        sha256: String::new(),
    }
}

fn sample() -> FileList {
    list(&[("a/b/c.h", 3), ("a/d", 300_000), ("e", 0)])
}

/// The tree of a list has the root as inode 1, a directory for each directory its paths name
/// (made once, however many files are in it), and a file for each file; its lookups, reads and
/// directory listings answer as a kernel request would be answered, errors included.
#[test]
fn the_tree_of_a_list_answers_lookups_reads_and_listings() {
    let tree = Tree::of(&sample()).unwrap();
    let a = tree.lookup(1, OsStr::new("a")).unwrap();
    assert!(matches!(a.1, Node::Dir { parent: 1, .. }));
    let b = tree.lookup(a.0, OsStr::new("b")).unwrap();
    let c = tree.lookup(b.0, OsStr::new("c.h")).unwrap();
    assert_eq!(c.1, &Node::File { size: 3 });
    let d = tree.lookup(a.0, OsStr::new("d")).unwrap();
    assert_eq!(tree.nodes.len(), 6, "root, a, b, c.h, d, e");
    assert_eq!(tree.lookup(1, OsStr::new("zz")), Err(Errno::ENOENT));
    assert_eq!(
        tree.lookup(1, OsStr::from_bytes(b"\xff")),
        Err(Errno::ENOENT)
    );
    assert_eq!(tree.lookup(c.0, OsStr::new("x")), Err(Errno::ENOTDIR));
    assert_eq!(tree.lookup(0, OsStr::new("a")), Err(Errno::ENOENT));
    assert_eq!(tree.lookup(99, OsStr::new("a")), Err(Errno::ENOENT));
    assert_eq!(tree.read(d.0, 0, 4096), Ok(4096));
    assert_eq!(tree.read(d.0, 299_000, 4096), Ok(1000));
    assert_eq!(tree.read(d.0, 400_000, 4096), Ok(0));
    assert_eq!(tree.read(d.0, 0, u32::MAX), Ok(300_000));
    assert_eq!(tree.read(1, 0, 1), Err(Errno::EISDIR));
    assert_eq!(tree.read(99, 0, 1), Err(Errno::ENOENT));
    assert_eq!(
        tree.entries(a.0).unwrap(),
        [
            (a.0, ".", FileType::Directory),
            (1, "..", FileType::Directory),
            (b.0, "b", FileType::Directory),
            (d.0, "d", FileType::RegularFile)
        ]
    );
    assert_eq!(tree.entries(c.0), Err(Errno::ENOTDIR));
    assert_eq!(tree.entries(99), Err(Errno::ENOENT));
}

/// A list that names a file as a directory, or (out of order) a directory as a file, has no tree,
/// and the error names the path.
#[test]
fn a_list_that_names_a_file_as_a_directory_has_no_tree() {
    let error = Tree::of(&list(&[("a", 1), ("a/b", 1)])).unwrap_err();
    assert_eq!(
        error.to_string(),
        "a/b: a file of the list is named as a directory"
    );
    let error = Tree::of(&list(&[("a/b", 1), ("a", 1)])).unwrap_err();
    assert_eq!(
        error.to_string(),
        "a: a directory of the list is named as a file"
    );
}
