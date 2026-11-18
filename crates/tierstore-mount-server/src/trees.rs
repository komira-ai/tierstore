//! Where the server finds a tree's list (section 3): `open-pack` names a tree and a pack index, and
//! the server needs the pack's record, its root and its cell digests.

use std::sync::Arc;

use tierstore_fetch_api::ids::TreeRef;
use tierstore_fetch_api::list::{ListL2, ListVersionUnsupported};

/// Why a tree's list is not there.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// No tree of that reference is known, or its list cannot be had now.
    #[error("tree {0:?} is not known")]
    Unknown(TreeRef),
    /// The tree's list is not L2 (3.5): the mount takes its miss path.
    #[error(transparent)]
    Unsupported(#[from] ListVersionUnsupported),
}

/// The tree lists of the node: the mapped L2 list of a tree (T-FMT's reader), found by the tree
/// reference a mount names in `open-pack`.
#[async_trait::async_trait]
pub trait Trees: Send + Sync {
    /// The list of `tree`.
    async fn list(&self, tree: &TreeRef) -> Result<Arc<dyn ListL2>, TreeError>;
}
