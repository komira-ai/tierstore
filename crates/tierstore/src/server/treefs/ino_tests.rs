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
