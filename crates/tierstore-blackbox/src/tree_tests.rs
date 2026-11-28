//! A tree as the rig writes it, and the check of a mount of it.

use std::path::Path;

use tierstore::{TreeListV2, tree_list_digest};
use tierstore_testkit::dir::TestDir;

use super::*;

/// Writes `tree` at `root` as a mount would show it.
fn lay_out(tree: &Tree, root: &Path) {
    let pack = &tree.objects()[0].1;
    for file in &tree.files {
        let path = root.join(&file.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let at = usize::try_from(file.offset).unwrap();
        std::fs::write(&path, &pack[at..at + tree.file_bytes]).unwrap();
    }
    std::os::unix::fs::symlink(&tree.files[0].path, root.join(LINK)).unwrap();
    std::fs::write(root.join(EMPTY), b"").unwrap();
}

/// Asserts that a tree is held by its pack under [`PACK_PREFIX`], its version 2 list under the
/// key the list's bytes name, and the L2 sidecar of that id, in that order; that the list reads
/// as a version 2 list; that every file has bytes of its own; and that two trees of other names
/// have other packs and keys.
#[test]
fn a_tree_is_its_pack_its_list_under_its_key_and_its_sidecar() {
    let tree = Tree::new("one", 300, 16).unwrap();
    let objects = tree.objects();
    assert_eq!(objects.len(), 3);
    let (pack_key, pack) = &objects[0];
    assert_eq!(pack_key, &format!("{PACK_PREFIX}{}", TreeDigest::of(pack)));
    assert_eq!(pack.len(), 300 * 16);
    let (list_key, list) = &objects[1];
    assert_eq!(list_key, tree.key());
    assert_eq!(tree_list_digest(tree.key()), Some(TreeDigest::of(list)));
    assert!(TreeListV2::open(list).is_ok());
    let (side_key, side) = &objects[2];
    assert_eq!(side_key, &sidecar_key(TreeDigest::of(list).as_bytes()));
    assert!(!side.is_empty());
    assert_eq!(tree.files[0].path, "d0000/f0000000");
    assert_eq!(tree.files[299].path, "d0001/f0000299");
    let mut digests: Vec<_> = tree.files.iter().map(|f| f.sha256.to_string()).collect();
    digests.sort();
    digests.dedup();
    assert_eq!(digests.len(), 300);

    let other = Tree::new("two", 300, 16).unwrap();
    assert_ne!(other.key(), tree.key());
    assert_ne!(other.objects()[0].0, tree.objects()[0].0);
}

/// Asserts that a shape out of bounds is refused before anything is written: no file, more
/// than [`MOST_FILES`], and files too short to hold their number.
#[test]
fn a_shape_out_of_bounds_is_refused() {
    for (files, file_bytes) in [(0, 64), (MOST_FILES + 1, 64), (10, LEAST_FILE_BYTES - 1)] {
        let refused = Tree::new("t", files, file_bytes).unwrap_err();
        assert_eq!(
            refused.to_string(),
            format!(
                "a tree of {files} files of {file_bytes} bytes: 1 to {MOST_FILES} files of at least {LEAST_FILE_BYTES} bytes"
            )
        );
    }
    assert!(Tree::new("t", 1, LEAST_FILE_BYTES).is_ok());
}

/// Asserts that a mount that shows the tree as written passes the check.
#[test]
fn a_mount_of_the_tree_passes_the_check() {
    let dir = TestDir::under(std::env::temp_dir(), "blackbox-tree-good");
    let tree = Tree::new("good", 5, 32).unwrap();
    lay_out(&tree, dir.path());
    assert_eq!(tree.check(dir.path()), Ok(()));
}

/// Asserts that the check names what is wrong: a missing file, a file with other bytes of the
/// same length, a file cut short, a link to another target, a missing link, an empty file with
/// bytes, an empty file that is a directory, and a missing empty file.
#[test]
fn the_check_names_what_is_not_the_trees() {
    let dir = TestDir::under(std::env::temp_dir(), "blackbox-tree-bad");
    let tree = Tree::new("bad", 3, 16).unwrap();
    let fresh = |name: &str| {
        let root = dir.path().join(name);
        lay_out(&tree, &root);
        root
    };
    let first = "d0000/f0000000";

    let missing = fresh("missing");
    std::fs::remove_file(missing.join(first)).unwrap();
    let said = tree.check(&missing).unwrap_err();
    assert!(
        matches!(&said, TreeFault::Unreadable { path, .. } if path == first),
        "{said}"
    );

    let other = fresh("other");
    std::fs::write(other.join(first), [7u8; 16]).unwrap();
    let said = tree.check(&other).unwrap_err();
    assert!(matches!(said, TreeFault::Differs { .. }), "{said}");
    assert_eq!(
        said.to_string(),
        format!("{first}: 16 bytes that are not the tree's 16")
    );

    let short = fresh("short");
    std::fs::write(short.join("d0000/f0000002"), [0u8; 3]).unwrap();
    assert_eq!(
        tree.check(&short).unwrap_err().to_string(),
        "d0000/f0000002: 3 bytes that are not the tree's 16"
    );

    let linked = fresh("linked");
    std::fs::remove_file(linked.join(LINK)).unwrap();
    std::os::unix::fs::symlink("elsewhere", linked.join(LINK)).unwrap();
    assert_eq!(
        tree.check(&linked),
        Err(TreeFault::Differs {
            path: LINK.to_owned(),
            what: "a link to elsewhere".to_owned(),
        })
    );

    let unlinked = fresh("unlinked");
    std::fs::remove_file(unlinked.join(LINK)).unwrap();
    let said = tree.check(&unlinked).unwrap_err();
    assert!(
        matches!(&said, TreeFault::Unreadable { path, .. } if path == LINK),
        "{said}"
    );

    let full = fresh("full");
    std::fs::write(full.join(EMPTY), b"xy").unwrap();
    assert_eq!(
        tree.check(&full).unwrap_err().to_string(),
        format!("{EMPTY}: 2 bytes, not an empty file")
    );

    let dir_empty = fresh("dir-empty");
    std::fs::remove_file(dir_empty.join(EMPTY)).unwrap();
    std::fs::create_dir(dir_empty.join(EMPTY)).unwrap();
    let said = tree.check(&dir_empty).unwrap_err();
    assert!(matches!(said, TreeFault::Differs { .. }), "{said}");

    let no_empty = fresh("no-empty");
    std::fs::remove_file(no_empty.join(EMPTY)).unwrap();
    let said = tree.check(&no_empty).unwrap_err();
    assert!(
        matches!(&said, TreeFault::Unreadable { path, .. } if path == EMPTY),
        "{said}"
    );
}
