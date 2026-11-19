//! What the null file system answers for a miss of the tree. The file system over a real mount
//! is in `tests/nullfs_integration.rs`; the tree itself in `tree_tests.rs`.

use fuser::Errno;

use super::errno;
use crate::tree::Miss;

/// Each miss of the tree is answered with its errno.
#[test]
fn a_miss_is_answered_with_its_errno() {
    assert_eq!(errno(Miss::NotFound), Errno::ENOENT);
    assert_eq!(errno(Miss::NotDir), Errno::ENOTDIR);
    assert_eq!(errno(Miss::IsDir), Errno::EISDIR);
}
