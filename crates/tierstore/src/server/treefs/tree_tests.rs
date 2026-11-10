//! The flattened tree: children by name, in the order of the manifest, and the link count.

use super::Tree;
use crate::server::treefs::testing::{Built, Node, dir, file};
use crate::tree_manifest::{TreeDigest, flatten};

/// The tree whose root holds `children`, flattened.
fn tree_of(children: Vec<(&str, Node)>) -> Tree {
    let built = Built::of(children);
    let digest = TreeDigest::from_hex(&built.root).unwrap();
    let entries = flatten(&digest, |wanted| {
        built
            .manifests
            .get(&wanted.to_string())
            .cloned()
            .ok_or("none")
    })
    .unwrap();
    Tree::new(entries)
}

/// Asserts that the children of each directory are its entries in manifest order, found by name,
/// and that a name that is not there, a name in a file and the root's own name are not found.
#[test]
fn children_are_found_by_name_and_listed_in_manifest_order() {
    let tree = tree_of(vec![
        ("b", dir(vec![("y", file(b"1")), ("x", file(b"2"))])),
        ("a", file(b"3")),
        ("c", Node::Link("a".to_owned())),
    ]);
    // Depth first in manifest order: root 0, a 1, b 2 (x 3, y 4), c 5.
    let names = |dir: u32| -> Vec<String> {
        tree.children(dir)
            .iter()
            .map(|&k| tree.at(k).name.to_string())
            .collect()
    };
    assert_eq!(names(0), ["a", "b", "c"]);
    assert_eq!(names(2), ["x", "y"]);
    assert_eq!(tree.children(1), &[] as &[u32]);
    assert_eq!(tree.child(0, "b"), Some(2));
    assert_eq!(tree.child(2, "y"), Some(4));
    assert_eq!(tree.child(0, "zz"), None);
    assert_eq!(tree.child(0, ""), None);
    assert_eq!(tree.child(1, "a"), None);
    assert_eq!(tree.entry(5).map(|e| e.index), Some(5));
    assert!(tree.entry(6).is_none());
}

/// Asserts that a directory has 2 links and one for each subdirectory, and that a tree of the root
/// alone is a root with no children.
#[test]
fn a_directory_has_two_links_and_one_for_each_subdirectory() {
    let tree = tree_of(vec![
        ("d1", dir(vec![("f", file(b"x"))])),
        ("d2", dir(vec![("e", dir(vec![("g", file(b"x"))]))])),
        ("f", file(b"x")),
    ]);
    assert_eq!(tree.links(0), 4);
    assert_eq!(tree.links(1), 2);
    let empty = tree_of(Vec::new());
    assert_eq!(empty.links(0), 2);
    assert_eq!(empty.children(0), &[] as &[u32]);
    assert_eq!(empty.child(0, "x"), None);
}
