//! The layout of an inode number: tree number above, index below.

use super::{MOST_TREES, ROOT, TREES, of, split};

/// Asserts that a number is the tree number shifted over the index, and splits back into both, at
/// the ends of each: the namespace, the first tree, the last index of a tree and the last tree.
#[test]
fn a_number_is_the_tree_number_over_the_index_and_splits_back() {
    assert_eq!(of(0, 1), ROOT);
    assert_eq!(of(0, 2), TREES);
    assert_eq!(of(1, 0), 1 << 24);
    assert_eq!(of(1, (1 << 24) - 1), (2 << 24) - 1);
    for (tree, index) in [
        (0, 1),
        (1, 0),
        (1, (1 << 24) - 1),
        (7, 12345),
        (MOST_TREES, 0),
    ] {
        assert_eq!(split(of(tree, index)), (tree, index));
    }
    assert!(of(MOST_TREES, (1 << 24) - 1) < 1 << 63);
}

/// Asserts that the number of a directory above the fs locations is in its range for any hash,
/// including the ends, and that no number of the namespace, the put directory or a tree is.
#[test]
fn only_numbers_made_by_placed_dir_are_in_the_placed_range() {
    for hash in [0, 1, 12345, (1 << 62) - 1, 1 << 62, u64::MAX] {
        let number = super::placed_dir(hash);
        assert!(super::is_placed_dir(number), "{hash}");
        assert_eq!(super::put_of(number), None, "{hash}");
    }
    assert_eq!(super::placed_dir(0), 0b10 << 62);
    for other in [
        0,
        ROOT,
        TREES,
        super::PUT,
        super::put_dir(0),
        super::put_dir(u64::MAX),
        super::put_file(0),
        super::put_file(u64::MAX),
        of(0, 3),
        of(MOST_TREES, (1 << 24) - 1),
    ] {
        assert!(!super::is_placed_dir(other), "{other:#x}");
    }
}
