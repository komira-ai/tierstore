//! The fill of a tree's new manifests by the cache itself: `fill-tree <sha256> <ms>` of the fill
//! socket, and the same call made in process (`treefs/in_process.rs`).
//!
//! A tree the mount reads whose root manifest the cache directory does not hold is a new tree, and
//! its new manifests are a chain down from the root: a directory that changed changes the manifest
//! of every directory above it. The mount learns the digests of a level only once it has read the
//! level above, so a fill of each new manifest over the socket ([`LocalCache::fill_manifest`])
//! costs a round trip of the socket for each level of the chain. Here the cache goes down the
//! chain itself and answers once: the root, filled if it is not held; then, a level at a time, every
//! directory of the manifests it read that the cache does not hold, filled up to
//! [`FILL_SOCKET_CONCURRENCY`] at once, each level before the one below. A directory the cache
//! holds is not gone into: the tree below it was filled with it, or is filled by the mount's own
//! read if it was not (an eviction took part of it). So the socket is crossed once, and the levels
//! of the chain cost a round of the upstream's answers each, with no socket between.
//!
//! Each manifest is filled as `fill <sha256>` fills it (one fill for a digest, whichever path
//! asks; a permit of the fill socket's pool), then read from its file and checked by its digest.
//! The limits of a tree are judged as each manifest comes in, as the mount's read judges them
//! (`tree_manifest/waves.rs`): the children of the manifests filled over [`MAX_ENTRIES`], or a
//! manifest with children deeper than [`MAX_DEPTH`], end the fill with no level below fetched.
//! The children of the manifests the cache held are not counted, since they are not read, so the
//! mount's read may refuse a tree that this passes; it never passes one that this refuses. The
//! fill ends by the time the request was given ([`TreeFillError::Spent`]); the fills it started
//! go on, as every fill does when its caller stops waiting.
//!
//! The answer to the socket is `ok <ms> <filled> [<filled> ...]`: the milliseconds the fill took
//! on the cache's clock, and the manifests it filled at each level it went down, the root's first
//! ([`TreeFilled`]); or the words of a [`TreeFillError`]. Either way the mount then reads the
//! tree from the cache directory as before, filling what it does not find there one manifest at a
//! time, so a failure here is the failure of that read, in its words.

use std::collections::HashSet;
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;

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

impl TreeFilled {
    /// The answer of the socket: `ok`, the milliseconds and the manifests of each level.
    pub(in crate::server) fn answer(&self) -> String {
        let mut answer = format!("ok {}", self.ms);
        for filled in &self.levels {
            answer.push_str(&format!(" {filled}"));
        }
        answer
    }

    /// What the answer `answer` of the socket says, if it is one of [`TreeFilled::answer`].
    pub(in crate::server) fn parse(answer: &str) -> Option<Self> {
        let mut words = answer.strip_prefix("ok ")?.split(' ');
        let ms = number(words.next()?)?;
        let levels = words.map(number).collect::<Option<Vec<u64>>>()?;
        Some(Self { ms, levels })
    }
}

/// The number `word` writes in decimal digits, with no sign; none for anything else.
fn number(word: &str) -> Option<u64> {
    if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    word.parse().ok()
}

/// Why a fill of a tree's new manifests did not fill them all. The text is the answer of the
/// socket.
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
                let Some((manifest, fetched)) = got? else {
                    continue;
                };
                filled += u64::from(fetched);
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
    /// either way.
    async fn new_manifest(
        &self,
        digest: TreeDigest,
        root: bool,
    ) -> Result<Option<(Arc<TreeManifest>, bool)>, TreeFillError> {
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
        let read = tokio::task::spawn_blocking(move || read_manifest(&path, &digest)).await;
        let read = read.unwrap_or_else(|why| Err(why.to_string()));
        read.map(|manifest| Some((Arc::new(manifest), !held)))
            .map_err(|why| TreeFillError::Read { digest, why })
    }
}

#[cfg(test)]
#[path = "tree_fill_tests.rs"]
mod tests;
