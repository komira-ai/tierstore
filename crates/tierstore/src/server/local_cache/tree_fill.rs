//! The fill of a tree's new manifests by the cache itself: `fill_tree`, which the file system's
//! core calls in this process (`treefs/in_process.rs`).
//!
//! A tree the mount reads whose root manifest the cache directory does not hold is a new tree, and
//! its new manifests are a chain down from the root: a directory that changed changes the manifest
//! of every directory above it. The mount learns the digests of a level only once it has read the
//! level above, so a fill of each new manifest by the core ([`LocalCache::fill_manifest`]) costs a
//! call for each level of the chain, each waiting on the one before. Here the cache goes down the
//! chain itself and answers once: the root, filled if it is not held; then, a level at a time, every
//! directory of the manifests it read that the cache does not hold, filled up to
//! [`FILL_SOCKET_CONCURRENCY`] at once, each level before the one below. A directory the cache
//! holds is not gone into: the tree below it was filled with it, or is filled by the mount's own
//! read if it was not (an eviction took part of it). So the core calls once, and the levels of the
//! chain cost a round of the upstream's answers each, with no call between.
//!
//! Each manifest is filled as `fill <sha256>` fills it (one fill for a digest, whichever path
//! asks; a permit of the pool of `fill_socket_concurrency`), then read from its file and checked
//! by its digest. The limits of a tree are judged as each manifest comes in, as the mount's read
//! judges them (`tree_manifest/waves.rs`): the children of the manifests filled over
//! [`MAX_ENTRIES`], or a manifest with children deeper than [`MAX_DEPTH`], end the fill with no
//! level below fetched. The children of the manifests the cache held are not counted, since they
//! are not read, so the mount's read may refuse a tree that this passes; it never passes one that
//! this refuses. The fill ends by the time the request was given ([`TreeFillError::Spent`]); the
//! fills it started go on, as every fill does when its caller stops waiting.
//!
//! A root that is a version 2 tree list (its file begins with [`TREE_LIST_V2_MAGIC`]) has no
//! manifests below it: the list is the whole tree, in the one object. The fill stops once the root
//! is in the cache and answers its one level, so the mount reads the list from the cache directory
//! with no other request (`treefs/core/placed.rs`). A list below the root is not looked for: it is
//! read as a manifest, and refused as one.
//!
//! The answer is a [`TreeFilled`]: the milliseconds the fill took on the cache's clock, and the
//! manifests it filled at each level it went down, the root's first; or a [`TreeFillError`].
//! Either way the mount then reads the tree from the cache directory as before, filling what it
//! does not find there one manifest at a time, so a failure here is the failure of that read, in
//! its words.

use std::collections::HashSet;
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;

use crate::TREE_LIST_V2_MAGIC;
use crate::tree_manifest::{
    EntryKind, MAX_DEPTH, MAX_ENTRIES, MAX_MANIFEST_BYTES, TreeDigest, TreeError, TreeManifest,
    parse_manifest,
};

use super::super::FILL_SOCKET_CONCURRENCY;
use super::super::attaches::Via;
use super::super::layout::{data_path, name_of};
use super::{FillError, LocalCache, object_key};

/// What a fill of a tree's new manifests did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct TreeFilled {
    /// The milliseconds it took, on the clock of the cache.
    pub ms: u64,
    /// The manifests it filled at each level it went down, the root's first.
    pub levels: Vec<u64>,
}

/// Why a fill of a tree's new manifests did not fill them all. The text is the words of the
/// failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::server) enum TreeFillError {
    /// The fill of a manifest failed, in the words of `fill <sha256>`.
    #[error("{error} at {digest}")]
    Fill {
        /// The manifest.
        digest: TreeDigest,
        /// Why it was not filled.
        error: FillError,
    },
    /// A manifest that was filled could not be read from its file, or is not a manifest of its
    /// digest.
    #[error("cannot read {digest}: {why}")]
    Read {
        /// The manifest.
        digest: TreeDigest,
        /// Why, in words.
        why: String,
    },
    /// The tree is over a limit: [`TreeError::TreeTooLarge`] or [`TreeError::TreeTooDeep`].
    #[error("{0}")]
    Limit(TreeError),
    /// The time the request was given passed first.
    #[error("spent")]
    Spent,
}

/// The limits a fill of a tree judges: the children of the manifests it fills, and the depth of
/// the deepest child.
#[derive(Clone, Copy, Debug)]
struct Limits {
    entries: u64,
    depth: usize,
}

/// The limits of every tree of the mount.
const LIMITS: Limits = Limits {
    entries: MAX_ENTRIES,
    depth: MAX_DEPTH,
};

/// The manifest in the file at `path`, of `digest`: at most [`MAX_MANIFEST_BYTES`] and one more
/// byte read, so that a larger file is refused by its size and not read whole.
fn read_manifest(path: &std::path::Path, digest: &TreeDigest) -> Result<TreeManifest, String> {
    let bytes = bounded(path).map_err(|why| why.to_string())?;
    parse_manifest(&bytes, digest).map_err(|why| why.to_string())
}

/// What a fill of a tree met at one digest: a manifest, read from its file, or, at the root, a
/// version 2 tree list, which has nothing below it.
enum Met {
    /// The manifest.
    Manifest(Arc<TreeManifest>),
    /// A tree list at the root.
    List,
}

/// What [`Met`] the file at `path` of the object `digest` is: a tree list if it is the `root` and
/// begins with [`TREE_LIST_V2_MAGIC`], else the manifest it is read as (a file that cannot be read
/// fails as that read).
fn met(path: &std::path::Path, digest: &TreeDigest, root: bool) -> Result<Met, String> {
    if root && is_list(path).unwrap_or(false) {
        return Ok(Met::List);
    }
    read_manifest(path, digest).map(|manifest| Met::Manifest(Arc::new(manifest)))
}

/// Whether the file at `path` begins with [`TREE_LIST_V2_MAGIC`]: its first bytes only are read.
pub(in crate::server) fn is_list(path: &std::path::Path) -> std::io::Result<bool> {
    let mut first = Vec::with_capacity(TREE_LIST_V2_MAGIC.len());
    let _ = std::fs::File::open(path)?
        .take(TREE_LIST_V2_MAGIC.len() as u64)
        .read_to_end(&mut first)?;
    Ok(first == TREE_LIST_V2_MAGIC)
}

/// The bytes of the file at `path`, at most [`MAX_MANIFEST_BYTES`] and one more.
fn bounded(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let _ = std::fs::File::open(path)?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

impl LocalCache {
    /// Fills the new manifests of the tree `root`, as the module says, within `within`.
    ///
    /// # Errors
    /// Why not all were filled: see [`TreeFillError`].
    pub(in crate::server) async fn fill_tree(
        &self,
        root: &TreeDigest,
        within: Duration,
    ) -> Result<TreeFilled, TreeFillError> {
        self.fill_tree_under(root, within, LIMITS).await
    }

    /// [`LocalCache::fill_tree`] under `limits`.
    async fn fill_tree_under(
        &self,
        root: &TreeDigest,
        within: Duration,
        limits: Limits,
    ) -> Result<TreeFilled, TreeFillError> {
        let clock = &self.inner.clock;
        let start = clock.now();
        let descent = self.descend(root, limits);
        let levels = tokio::time::timeout(within, descent)
            .await
            .map_err(|_| TreeFillError::Spent)??;
        Ok(TreeFilled {
            ms: clock.now().saturating_sub(start),
            levels,
        })
    }

    /// The manifests filled at each level of the tree `root`, going down from it as the module
    /// says.
    async fn descend(&self, root: &TreeDigest, limits: Limits) -> Result<Vec<u64>, TreeFillError> {
        let mut seen = HashSet::from([*root]);
        let mut level = vec![*root];
        let mut opened = 0u64;
        // The children of the manifests of `level` are at `depth`.
        let mut depth = 1;
        let mut levels = Vec::new();
        while !level.is_empty() {
            let root_level = levels.is_empty();
            // The digests are moved into the fetches: a borrow of the level would not be one the
            // fetches outlive for every lifetime, as a future sent to another task must.
            let fetches = std::mem::take(&mut level)
                .into_iter()
                .map(|digest| self.new_manifest(digest, root_level));
            let mut fetches = futures::stream::iter(fetches).buffered(FILL_SOCKET_CONCURRENCY);
            let (mut below, mut filled) = (Vec::new(), 0u64);
            while let Some(got) = fetches.next().await {
                let Some((met, fetched)) = got? else {
                    continue;
                };
                filled += u64::from(fetched);
                let manifest = match met {
                    Met::Manifest(manifest) => manifest,
                    // Only the root is read as a list: the tree is that one object.
                    Met::List => return Ok(vec![filled]),
                };
                let children = manifest.children.len() as u64;
                opened = opened.saturating_add(children);
                if opened > limits.entries {
                    return Err(TreeFillError::Limit(TreeError::TreeTooLarge));
                }
                if children > 0 && depth > limits.depth {
                    return Err(TreeFillError::Limit(TreeError::TreeTooDeep));
                }
                for child in &manifest.children {
                    if let EntryKind::Dir { digest, .. } = &child.kind
                        && seen.insert(*digest)
                    {
                        below.push(*digest);
                    }
                }
            }
            drop(fetches);
            levels.push(filled);
            level = below;
            depth += 1;
        }
        Ok(levels)
    }

    /// The manifest `digest`, filled if the cache does not hold it and read from its file, and
    /// whether it was filled; none for one the cache holds, unless it is the root, which is read
    /// either way, and is a tree list if its file begins as one ([`Met`]).
    async fn new_manifest(
        &self,
        digest: TreeDigest,
        root: bool,
    ) -> Result<Option<(Met, bool)>, TreeFillError> {
        let hex = digest.to_string();
        let held = self.inner.present(&name_of(&object_key(&hex))).await;
        if held && !root {
            return Ok(None);
        }
        if !held {
            self.fill_manifest_via(&hex, Via::Tree)
                .await
                .map_err(|error| TreeFillError::Fill { digest, error })?;
        }
        let path = data_path(&self.inner.config.path, &hex);
        let read = tokio::task::spawn_blocking(move || met(&path, &digest, root)).await;
        let read = read.unwrap_or_else(|why| Err(why.to_string()));
        read.map(|met| Some((met, !held)))
            .map_err(|why| TreeFillError::Read { digest, why })
    }
}

#[cfg(test)]
#[path = "tree_fill_tests.rs"]
mod tests;
