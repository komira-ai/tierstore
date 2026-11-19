//! The tree of a list: its inodes, its data file, and what its lookups, reads and listings
//! answer.

use super::{DATA_ALIGN, Miss, Node, Span, Tree};
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
    let a = tree.lookup(1, b"a").unwrap();
    assert!(matches!(a.1, Node::Dir { parent: 1, .. }));
    let b = tree.lookup(a.0, b"b").unwrap();
    let c = tree.lookup(b.0, b"c.h").unwrap();
    assert_eq!(c.1, &Node::File { size: 3, at: 0 });
    let d = tree.lookup(a.0, b"d").unwrap();
    assert_eq!(tree.len(), 6, "root, a, b, c.h, d, e");
    assert!(!tree.is_empty());
    assert_eq!(tree.lookup(1, b"zz"), Err(Miss::NotFound));
    assert_eq!(tree.lookup(1, b"\xff"), Err(Miss::NotFound));
    assert_eq!(tree.lookup(c.0, b"x"), Err(Miss::NotDir));
    assert_eq!(tree.lookup(0, b"a"), Err(Miss::NotFound));
    assert_eq!(tree.lookup(99, b"a"), Err(Miss::NotFound));
    let root = tree.node(1).unwrap();
    assert_eq!(
        tree.entries(a.0).unwrap(),
        [
            (a.0, ".", a.1),
            (1, "..", root),
            (b.0, "b", b.1),
            (d.0, "d", d.1)
        ]
    );
    assert_eq!(tree.entries(c.0), Err(Miss::NotDir));
    assert_eq!(tree.entries(99), Err(Miss::NotFound));
}

/// Each file sits in the data file at the next 4 KiB boundary after the one before it, in list
/// order (an empty file takes no room), and a read answers the span of the file it covers: from
/// the offset to the end of the file at most, nothing at or past the end, never a byte of the
/// next file.
#[test]
fn a_read_answers_the_span_of_its_file_in_the_data_file() {
    let tree = Tree::of(&sample()).unwrap();
    let (c, d, e) = (4, 5, 6);
    assert_eq!(tree.node(c), Ok(&Node::File { size: 3, at: 0 }));
    assert_eq!(
        tree.node(d),
        Ok(&Node::File {
            size: 300_000,
            at: DATA_ALIGN
        })
    );
    let after_d = DATA_ALIGN + 300_000_u64.next_multiple_of(DATA_ALIGN);
    assert_eq!(after_d, 4096 + 303_104);
    assert_eq!(
        tree.node(e),
        Ok(&Node::File {
            size: 0,
            at: after_d
        })
    );
    assert_eq!(tree.data_len(), after_d, "the empty file adds nothing");

    let at = |at, len| Ok(Span { at, len });
    assert_eq!(tree.read(c, 0, 4096), at(0, 3));
    assert_eq!(tree.read(c, 2, 4096), at(2, 1));
    assert_eq!(tree.read(c, 3, 4096), at(3, 0));
    assert_eq!(tree.read(d, 0, 4096), at(4096, 4096));
    assert_eq!(tree.read(d, 299_000, 4096), at(4096 + 299_000, 1000));
    assert_eq!(tree.read(d, 400_000, 4096), at(4096 + 300_000, 0));
    assert_eq!(tree.read(d, 0, u32::MAX), at(4096, 300_000));
    assert_eq!(tree.read(e, 0, 1), at(after_d, 0));
    assert_eq!(tree.read(1, 0, 1), Err(Miss::IsDir));
    assert_eq!(tree.read(99, 0, 1), Err(Miss::NotFound));
}

/// A list of no file is the root alone, with an empty data file.
#[test]
fn an_empty_list_is_the_root_alone() {
    let tree = Tree::of(&list(&[])).unwrap();
    assert_eq!((tree.len(), tree.data_len()), (1, 0));
    let root = tree.node(1).unwrap();
    assert_eq!(tree.entries(1).unwrap(), [(1, ".", root), (1, "..", root)]);
}

/// Each miss is the errno a kernel request is answered with.
#[test]
fn a_miss_is_its_errno() {
    assert_eq!(Miss::NotFound.errno(), 2);
    assert_eq!(Miss::NotDir.errno(), 20);
    assert_eq!(Miss::IsDir.errno(), 21);
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
