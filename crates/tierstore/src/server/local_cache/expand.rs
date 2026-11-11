//! The expansion of a tree into queued blobs: a tree digest to its manifests, and the manifests to
//! the blobs of the files below it.
//!
//! The same tree digest reaches the cache twice, as the list a client hands the job scheduler and
//! as the request that comes first, and the second is the one that gets here: this module reads
//! the tree as an attach does, the root manifest (the object `cas/l/<tree>`) and the manifest of
//! each directory below it, and hands every file with bytes to the prefetch queue
//! ([`LocalCache::prefetch`](super::LocalCache::prefetch)'s `queue_blob`), so that the blobs are
//! filled before the job opens them, and a job that opens one while it fills joins that fill.
//!
//! A manifest is an ordinary cache entry, filled by the fill the fill socket asks for, so an attach
//! of the same tree that comes while it is fetched joins the fetch and the reverse, and once read it
//! is present for the attach that follows. (A manifest therefore draws on the permits of
//! `fill_socket_concurrency`, one at a time per tree being expanded, as an attach's does; the blobs
//! draw on the workers.)
//!
//! The walk is depth first in manifest order, the order the entries of a flattened tree are
//! numbered in, and a blob is handed over as the walk meets it, not after it. It keeps the stack of
//! the manifests being listed and the digests of the directories it has opened (a directory that
//! occurs at several places is read and walked once, as its blobs are the same), never the
//! flattened tree, so its memory is that of the distinct manifests of one tree. It refuses a tree
//! past [`MAX_ENTRIES`] entries of distinct manifests or [`MAX_DEPTH`] levels, as the library's
//! flatten does, and a manifest [`parse_manifest`] refuses.
//!
//! The bounds of a call (`hydrate_max_trees`, `hydrate_max_blobs` and `hydrate_max_bytes`) are
//! kept in [`Call`]: a call with more trees than `hydrate_max_trees` is refused whole; the walk
//! stops at the first blob that would take the call past the others, and the trees after it are
//! cut without a request. A blob the cache holds, or that is queued or running already, costs the
//! call nothing: only the blobs and bytes newly queued count.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the hydrate call that expands the trees of a request is a later change"
    )
)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use super::metrics::{Turned, Walked};
use super::prefetch::Prefetched;
use super::source::{Blob, FillError, manifest_path};
use super::{Inner, LocalCache};
use crate::journal::is_reserved;
use crate::tree_manifest::{
    EntryKind, MAX_DEPTH, MAX_ENTRIES, TreeDigest, TreeError, TreeManifest, parse_manifest,
};

/// Why a tree was not expanded. The text is what the call records for the tree.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::server) enum Refusal {
    /// A manifest could not be filled into the cache: the upstream has none, the cache is
    /// read-only, and so on.
    #[error("cannot fetch manifest {digest}: {why}")]
    Fetch {
        /// The manifest.
        digest: TreeDigest,
        /// The end of its fill.
        why: FillError,
    },
    /// The file of a manifest could not be read after its fill.
    #[error("cannot read manifest {digest}: {kind}")]
    Read {
        /// The manifest.
        digest: TreeDigest,
        /// The kind of the error of the disk.
        kind: std::io::ErrorKind,
    },
    /// The library refused the manifest, or the tree it makes.
    #[error("{0}")]
    Manifest(#[from] TreeError),
}

/// A bound of a call that the walk reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Bound {
    /// `hydrate_max_blobs` blobs were queued.
    Blobs,
    /// The next blob would pass `hydrate_max_bytes`.
    Bytes,
}

/// How the expansion of one tree ended.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::server) enum Tree {
    /// Every file of the tree with bytes was handed to the queue (whatever became of each).
    Expanded,
    /// The walk stopped at a bound of the call, or never began for it. The blobs met before it stay
    /// queued, and the rest is left to the reads.
    Cut(Bound),
    /// The tree was not expanded.
    Refused(Refusal),
}

/// A call with more trees than `hydrate_max_trees`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{given} trees in one call, at most {max}")]
pub(in crate::server) struct TooManyTrees {
    /// The trees the call lists.
    pub given: usize,
    /// `hydrate_max_trees`.
    pub max: usize,
}

/// The limits of the library on a tree: the entries of its distinct manifests, and its levels.
#[derive(Clone, Copy, Debug)]
struct Limits {
    entries: u64,
    depth: usize,
}

impl Limits {
    /// The limits [`flatten`](crate::flatten) refuses a tree at.
    const LIBRARY: Self = Self {
        entries: MAX_ENTRIES,
        depth: MAX_DEPTH,
    };

    /// Whether a manifest of `children` entries, opened at `level` (the root's is 1), with
    /// `entries` in the manifests opened before it, keeps the tree within the limits. A manifest
    /// with no children has no entry to be too deep.
    fn admit(&self, entries: u64, children: usize, level: usize) -> Result<(), TreeError> {
        if entries > self.entries {
            return Err(TreeError::TreeTooLarge);
        }
        if children > 0 && level > self.depth {
            return Err(TreeError::TreeTooDeep);
        }
        Ok(())
    }
}

/// What one call has queued so far, which the bounds of the call are judged against.
struct Call {
    limits: Limits,
    blobs: usize,
    bytes: u64,
    /// The bound the call reached, once it has.
    reached: Option<Bound>,
}

impl Call {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            blobs: 0,
            bytes: 0,
            reached: None,
        }
    }
}

/// The bytes of the file of the manifest `digest`.
async fn read_file(path: &Path, digest: &TreeDigest) -> Result<Vec<u8>, Refusal> {
    tokio::fs::read(path).await.map_err(|error| Refusal::Read {
        digest: *digest,
        kind: error.kind(),
    })
}

/// A manifest being listed.
struct Frame {
    manifest: TreeManifest,
    /// The next child to look at.
    next: usize,
}

impl Inner {
    /// Hands the file `digest` of `size` bytes, found in `source`, to the queue, unless the call is
    /// at a bound or the file is read from a key that is not the caller's to read. The bound the
    /// walk has reached, if it has.
    fn hand_over(
        self: &Arc<Self>,
        call: &mut Call,
        (digest, size): (&TreeDigest, u64),
        source: Option<(&str, u64)>,
    ) -> Option<Bound> {
        let sha256 = digest.to_string();
        let key = format!("cas/l/{sha256}");
        let (from, offset) = source.unwrap_or((&key, 0));
        if is_reserved(from) {
            self.counters.turned(Turned::ReservedKey);
            return None;
        }
        if call.blobs >= self.config.prefetch.max_blobs {
            self.counters.turned(Turned::TooManyBlobs);
            return Some(Bound::Blobs);
        }
        if size > self.config.prefetch.max_bytes.saturating_sub(call.bytes) {
            self.counters.turned(Turned::TooManyBytes);
            return Some(Bound::Bytes);
        }
        let blob = Blob {
            sha256: &sha256,
            size,
            from,
            offset,
        };
        if self.queue_blob(&blob) == Prefetched::Queued {
            call.blobs += 1;
            call.bytes += size;
        }
        None
    }
}

impl LocalCache {
    /// Reads the manifest `digest` through the cache: filled if it is not there, then read from
    /// its file.
    async fn manifest(&self, digest: &TreeDigest) -> Result<Frame, Refusal> {
        let hex = digest.to_string();
        self.fill_manifest(&hex)
            .await
            .map_err(|why| Refusal::Fetch {
                digest: *digest,
                why,
            })?;
        let path = manifest_path(&self.inner.config.path, &hex);
        let bytes = read_file(&path, digest).await?;
        let manifest = parse_manifest(&bytes, digest)?;
        Ok(Frame { manifest, next: 0 })
    }

    /// Walks the tree `root`, handing its blobs to the queue as it meets them.
    async fn walk(&self, root: &TreeDigest, call: &mut Call) -> Result<(), Refusal> {
        let mut opened = HashSet::from([*root]);
        let first = self.manifest(root).await?;
        let mut entries = first.manifest.children.len() as u64;
        call.limits.admit(entries, entries as usize, 1)?;
        let mut stack = vec![first];
        while let Some(frame) = stack.last_mut() {
            let Some(child) = frame.manifest.children.get(frame.next) else {
                let _ = stack.pop();
                continue;
            };
            frame.next += 1;
            match &child.kind {
                EntryKind::Dir { digest, .. } => {
                    if !opened.insert(*digest) {
                        continue;
                    }
                    let digest = *digest;
                    let below = self.manifest(&digest).await?;
                    let children = below.manifest.children.len();
                    entries += children as u64;
                    call.limits
                        .admit(entries, children, stack.len() + 1)
                        .map_err(Refusal::from)?;
                    stack.push(below);
                }
                EntryKind::File {
                    digest,
                    size,
                    source,
                    ..
                } if *size > 0 => {
                    let source = source.as_ref().map(|s| (&*s.key, s.offset));
                    if let Some(bound) = self.inner.hand_over(call, (digest, *size), source) {
                        call.reached = Some(bound);
                        return Ok(());
                    }
                }
                EntryKind::File { .. } | EntryKind::Symlink { .. } => {}
            }
        }
        Ok(())
    }

    /// Expands `trees` in the order given, one at a time, for one call.
    async fn expand(&self, limits: Limits, trees: &[TreeDigest]) -> Vec<Tree> {
        let mut call = Call::new(limits);
        let mut listed = HashSet::new();
        let mut ended = Vec::with_capacity(trees.len());
        for tree in trees {
            if !listed.insert(*tree) {
                continue;
            }
            let end = match call.reached {
                Some(bound) => Tree::Cut(bound),
                None => match self.walk(tree, &mut call).await {
                    Err(refusal) => Tree::Refused(refusal),
                    Ok(()) => call.reached.map_or(Tree::Expanded, Tree::Cut),
                },
            };
            self.inner.counters.walked(match end {
                Tree::Expanded => Walked::Expanded,
                Tree::Cut(_) => Walked::Cut,
                Tree::Refused(_) => Walked::Refused,
            });
            ended.push(end);
        }
        ended
    }

    /// Expands the trees of one call, in the order given, into blobs queued to be filled: see the
    /// module. A tree listed twice is expanded once. It answers once every tree has been walked,
    /// which fetches the manifests but waits for no blob: the blobs are filled by the workers after.
    ///
    /// # Errors
    /// [`TooManyTrees`] if the call lists more than `hydrate_max_trees` trees: nothing is read.
    pub(in crate::server) async fn expand_trees(
        &self,
        trees: &[TreeDigest],
    ) -> Result<Vec<Tree>, TooManyTrees> {
        let max = self.inner.config.prefetch.max_trees;
        if trees.len() > max {
            return Err(TooManyTrees {
                given: trees.len(),
                max,
            });
        }
        Ok(self.expand(Limits::LIBRARY, trees).await)
    }
}

#[cfg(test)]
#[path = "expand_tests.rs"]
mod tests;
