//! The tree of the mount, flattened or an image: children by name, in the order of the manifest
//! or of the image, and the link count.

use std::sync::Arc;

use super::{ARC_HEADER, Kids, Tree};
use crate::TreeImage;
use crate::server::treefs::testing::{Built, Node, dir, file, image_bytes};
use crate::tree_image_list::ListImage;
use crate::tree_manifest::{EntryKind, TreeDigest, flatten};

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
            .from(0)
            .map(|k| tree.entry(k).unwrap().name.to_string())
            .collect()
    };
    assert_eq!(names(0), ["a", "b", "c"]);
    assert_eq!(names(2), ["x", "y"]);
    assert_eq!(tree.children(1), Kids::Listed(&[]));
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
    assert_eq!(empty.children(0), Kids::Listed(&[]));
    assert_eq!(empty.child(0, "x"), None);
}

/// Asserts that a tree of manifests has no modes and no location, and that a placed tree reports
/// the mode of each entry by index, none past the last, and its location.
#[test]
fn a_placed_tree_has_modes_and_a_location_and_a_tree_of_manifests_has_neither() {
    let plain = tree_of(vec![("a", file(b"1"))]);
    assert_eq!(plain.mode(0), None);
    assert_eq!(plain.mode(1), None);
    assert_eq!(plain.location(), None);
    let placed = Tree::placed(
        (0..2).map(|index| plain.entry(index).unwrap()).collect(),
        vec![0o711, 0o4755],
        "jobs/1".to_owned(),
    );
    assert_eq!(placed.mode(0), Some(0o711));
    assert_eq!(placed.mode(1), Some(0o4755));
    assert_eq!(placed.mode(2), None);
    assert_eq!(placed.location(), Some("jobs/1"));
    assert_eq!(placed.child(0, "a"), Some(1));
}

/// The image tree of [`image_bytes`] placed at `jobs/1`.
fn image_tree() -> Tree {
    let image = TreeImage::checked(image_bytes().into()).unwrap();
    Tree::image(Arc::new(ListImage::heap(&image)), "jobs/1".to_owned())
}

/// Asserts that a tree over an image counts its entries, reports its location, and makes each
/// entry from its record: a directory with the zero digest, a file with its digest, size, exec bit
/// from its mode and its source (key and offset), an empty file with no source, and a link with
/// its target; an index past the end has no entry.
#[test]
fn an_image_tree_makes_its_entries_from_the_records() {
    let tree = image_tree();
    assert_eq!(tree.len(), 7);
    assert_eq!(tree.location(), Some("jobs/1"));
    let root = tree.entry(0).unwrap();
    assert_eq!((root.parent, &*root.name), (None, ""));
    assert_eq!(
        root.kind,
        EntryKind::Dir {
            digest: TreeDigest::from_bytes([0; 32]),
            size: 0
        }
    );
    let tool = tree.entry(6).unwrap();
    assert_eq!((tool.index, tool.parent, &*tool.name), (6, Some(1), "tool"));
    match tool.kind {
        EntryKind::File {
            digest,
            size,
            exec,
            source: Some(source),
        } => {
            assert_eq!(digest, TreeDigest::of(b"hello"));
            assert_eq!((size, exec), (5, true));
            assert_eq!((&*source.key, source.offset), ("p0", 0));
        }
        other => panic!("{other:?}"),
    }
    match tree.entry(5).unwrap().kind {
        EntryKind::File {
            size,
            exec,
            source: Some(source),
            ..
        } => {
            assert_eq!((size, exec), (3, false));
            assert_eq!((&*source.key, source.offset), ("p0", 5));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        tree.entry(3).unwrap().kind,
        EntryKind::File {
            size: 0,
            exec: false,
            source: None,
            ..
        }
    ));
    assert!(
        matches!(tree.entry(4).unwrap().kind, EntryKind::Symlink { target } if &*target == "bin/tool")
    );
    assert!(tree.entry(7).is_none());
}

/// Asserts that the mode of an image tree's entries is the one stored for a directory and a file,
/// 0777 for a link, and none past the end.
#[test]
fn an_image_tree_reports_stored_modes_and_0777_for_links() {
    let tree = image_tree();
    let modes: Vec<Option<u16>> = (0..8).map(|index| tree.mode(index)).collect();
    assert_eq!(
        modes,
        [
            Some(0o755),
            Some(0o755),
            Some(0o700),
            Some(0o644),
            Some(0o777),
            Some(0o644),
            Some(0o755),
            None
        ]
    );
}

/// Asserts that an image tree's children are a range of indices in name order, found by name with
/// a binary search, that a name that is not there, a name in a file and a past-the-end directory
/// find nothing, and that a directory's link count is 2 and its subdirectories.
#[test]
fn an_image_tree_has_ranges_of_children_found_by_name_with_links_counted() {
    let tree = image_tree();
    assert_eq!(tree.children(0), Kids::Range(1..5));
    assert_eq!(tree.children(1), Kids::Range(5..7));
    assert_eq!(tree.children(2).from(0).count(), 0);
    assert_eq!(tree.children(3).from(0).count(), 0);
    assert_eq!(tree.child(0, "bin"), Some(1));
    assert_eq!(tree.child(0, "link"), Some(4));
    assert_eq!(tree.child(1, "tool"), Some(6));
    assert_eq!(tree.child(1, "other"), Some(5));
    assert_eq!(tree.child(0, "zz"), None);
    assert_eq!(tree.child(3, "x"), None);
    assert_eq!(tree.links(0), 4);
    assert_eq!(tree.links(1), 2);
    assert_eq!(tree.links(2), 2);
}

/// Asserts that `from` of a listed set starts at the `at`-th child and is empty past the end, and
/// that of a range does the same, including an `at` that does not fit 32 bits and one that
/// overflows the range's start.
#[test]
fn kids_from_starts_at_a_position_in_a_list_and_in_a_range() {
    let listed = Kids::Listed(&[4, 7, 9]);
    assert_eq!(listed.from(0).collect::<Vec<_>>(), [4, 7, 9]);
    assert_eq!(listed.from(2).collect::<Vec<_>>(), [9]);
    assert_eq!(listed.from(3).count(), 0);
    assert_eq!(listed.from(usize::MAX).count(), 0);
    let range = Kids::Range(10..13);
    assert_eq!(range.from(0).collect::<Vec<_>>(), [10, 11, 12]);
    assert_eq!(range.from(1).collect::<Vec<_>>(), [11, 12]);
    assert_eq!(range.from(3).count(), 0);
    assert_eq!(range.from(99).count(), 0);
    assert_eq!(range.from(usize::MAX).count(), 0);
    assert_eq!(range.from(u32::MAX as usize).count(), 0);
    let high = Kids::Range(u32::MAX - 2..u32::MAX);
    assert_eq!(high.from(1).collect::<Vec<_>>(), [u32::MAX - 1]);
    assert_eq!(high.from(5).count(), 0);
}

/// Asserts that `partition_point` gives the number of children that come before the others, in a
/// list and in a range, for none, some and all of them, and for an empty set.
#[test]
fn kids_partition_point_counts_the_children_before_a_boundary() {
    let point = |kids: &Kids<'_>, bound: u32| kids.partition_point(|kid| kid < bound);
    let listed = Kids::Listed(&[4, 7, 9]);
    let range = Kids::Range(10..14);
    let in_list: Vec<usize> = [0, 5, 8, 20].iter().map(|b| point(&listed, *b)).collect();
    let in_range: Vec<usize> = [0, 10, 12, 20].iter().map(|b| point(&range, *b)).collect();
    assert_eq!(in_list, [0, 1, 2, 3]);
    assert_eq!(in_range, [0, 0, 2, 4]);
    assert_eq!(point(&Kids::Listed(&[]), 5), 0);
    assert_eq!(point(&Kids::Range(5..5), 9), 0);
}

/// Asserts the heap estimate of a flattened tree: its entries, their names, the tables of
/// children (one start more than the entries, one child fewer) and, for a placed tree, its modes
/// and its location.
#[test]
fn a_flattened_tree_estimates_its_entries_names_and_tables() {
    let plain = tree_of(vec![
        ("ab", file(b"1")),
        ("c", dir(vec![("def", file(b"2"))])),
    ]);
    let mut entries: Vec<_> = (0..4).map(|index| plain.entry(index).unwrap()).collect();
    entries.shrink_to_fit();
    let names = ("ab".len() + "c".len() + "def".len()) + 4 * ARC_HEADER;
    let tables = size_of::<u32>() * (5 + 3);
    let listed = size_of::<crate::tree_manifest::TreeEntry>() * 4 + names + tables;
    assert_eq!(Tree::new(entries.clone()).heap_bytes(), listed as u64);
    let placed = Tree::placed(entries, vec![0o755; 4], "jobs/1".to_owned());
    let modes = size_of::<u16>() * 4;
    assert_eq!(
        placed.heap_bytes(),
        (listed + modes + "jobs/1".len()) as u64
    );
}

/// Asserts the heap estimate of a tree over an image: each key of its packs and its location, and
/// not the bytes of the list, which are budgeted apart (`core/lists.rs`); and that the tree gives
/// the list it is read from, which a flattened tree has none of.
#[test]
fn an_image_tree_estimates_its_keys_and_location_and_not_its_list() {
    let image = Arc::new(ListImage::heap(
        &TreeImage::checked(image_bytes().into()).unwrap(),
    ));
    let keys: usize = image
        .keys()
        .iter()
        .map(|key| size_of::<Arc<str>>() + key.len() + ARC_HEADER)
        .sum();
    assert_eq!(image.keys().len(), 1);
    let tree = Tree::image(Arc::clone(&image), "jobs/1".to_owned());
    assert_eq!(tree.heap_bytes(), (keys + "jobs/1".len()) as u64);
    assert!(Arc::ptr_eq(tree.list().unwrap(), &image));
    assert!(Tree::new(Vec::new()).list().is_none());
}
