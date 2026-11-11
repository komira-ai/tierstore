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
//! The bounds (`hydrate_max_trees`, `hydrate_max_blobs` and `hydrate_max_bytes`) are
//! kept in [`Call`], which a hold's task carries from one call's trees to the next: a call with more
//! trees than `hydrate_max_trees` is refused whole. The walk
//! stops at the first blob to queue once `hydrate_max_blobs` are queued, and the trees after it
//! are cut without a request. A file larger than the bytes left under `hydrate_max_bytes` is left
//! out (counted, its tree ends `Cut`) and the walk goes on to the files that fit, and to the later
//! trees. A blob the cache holds, or that is queued or running already, costs the call nothing,
//! and is judged against no bound: only the blobs and bytes newly queued count.
//!
//! A call of the hydrate endpoint ([`LocalCache::hydrate`]) opens a hold for its trees, answers, and
//! has the one task of the hold expand them (`drain`): each blob is handed to the queue for the hold
//! ([`LocalCache::hold_blob`]), which pins it when it is in the cache. The trees of later calls for
//! the hold wait in its list, and the bounds above are the hold's, over all its calls. A key that
//! is scoped may expand only what it may read: each manifest and each `source.key` is judged
//! against its read prefixes before anything is fetched, and a manifest outside ends the tree
//! (`access_denied`), a blob outside is left out (`access_denied`). When the hold has ended
//! (released, or its time passed) the walk finds it so before the next entry, whether that is a
//! manifest to fetch or a blob to hand over, and stops there, the later trees unread; the fetch
//! that is running when the hold ends is the only one it makes after.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use super::holds::{HoldError, Spent};
use super::metrics::{Turned, Walked};
use super::prefetch::Prefetched;
use super::source::{Blob, FillError, manifest_path};
use super::{Inner, LocalCache};
use crate::journal::is_reserved;
use crate::server::scoped::Scopes;
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
    /// The key that made the call may not read the object that holds the manifest.
    #[error("the key of the call may not read {key}")]
    Denied {
        /// The object.
        key: String,
    },
}

/// A bound of a call that the walk reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Bound {
    /// `hydrate_max_blobs` blobs were queued: the walk stopped.
    Blobs,
    /// A file would have passed `hydrate_max_bytes` and was left out: the walk went on.
    Bytes,
    /// The hold of the call ended (it was released, or its time passed): the walk stopped.
    Released,
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

/// Why a hydrate call was not accepted.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::server) enum HydrateError {
    /// `prefetch_concurrency` is 0: there is no queue to fill.
    #[error("the prefetch queue is off: prefetch_concurrency is 0")]
    Off,
    /// The call lists more trees than `hydrate_max_trees`.
    #[error("{0}")]
    TooManyTrees(#[from] TooManyTrees),
    /// The hold could not be opened.
    #[error("{0}")]
    Hold(#[from] HoldError),
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
    /// The bound the call reached, once it has: the walk stops there.
    reached: Option<Bound>,
    /// Whether the tree being walked had a file left out for `hydrate_max_bytes`: the walk goes
    /// on past it, to the files that fit.
    skipped: bool,
    /// The hold, by id and generation, that the blobs are handed to the queue for; none for a walk
    /// that only queues.
    hold: Option<(String, u64)>,
    /// The tree being walked, as the hold names it.
    tree: String,
    /// What the key of the call may read; none for a key that may read everything.
    reads: Option<Arc<Scopes>>,
}

impl Call {
    fn new(limits: Limits, hold: Option<(&str, u64)>, reads: Option<Arc<Scopes>>) -> Self {
        Self {
            limits,
            blobs: 0,
            bytes: 0,
            reached: None,
            skipped: false,
            hold: hold.map(|(id, generation)| (id.to_owned(), generation)),
            tree: String::new(),
            reads,
        }
    }

    /// What the call has queued so far.
    fn spent(&self) -> Spent {
        Spent {
            blobs: self.blobs,
            bytes: self.bytes,
            reached: self.reached,
        }
    }

    /// Takes up what the walks of the hold queued before this one.
    fn resume(&mut self, spent: Spent) {
        self.blobs = spent.blobs;
        self.bytes = spent.bytes;
        self.reached = spent.reached;
    }

    /// Whether the hold of the call, if it has one, has ended.
    fn ended(&self, inner: &Inner) -> bool {
        self.hold
            .as_ref()
            .is_some_and(|(id, generation)| !inner.hold_live((id, *generation)))
    }

    /// Whether the key of the call may read the object `key`.
    fn may_read(&self, key: &str) -> bool {
        self.reads
            .as_ref()
            .is_none_or(|scopes| scopes.may_read(key))
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
        if !call.may_read(from) {
            self.counters.turned(Turned::AccessDenied);
            return None;
        }
        // A blob the cache holds, or that is queued or running, costs the call nothing, so the
        // bounds are judged only for a blob that would be queued.
        if !self.holds(&sha256) {
            if call.blobs >= self.config.prefetch.max_blobs {
                self.counters.turned(Turned::TooManyBlobs);
                return Some(Bound::Blobs);
            }
            if size > self.config.prefetch.max_bytes.saturating_sub(call.bytes) {
                self.counters.turned(Turned::TooManyBytes);
                call.skipped = true;
                return None;
            }
        }
        let blob = Blob {
            sha256: &sha256,
            size,
            from,
            offset,
        };
        let handed = match &call.hold {
            Some((id, generation)) => match self.hold_blob((id, *generation), &call.tree, &blob) {
                Ok(handed) => handed,
                Err(_) => return Some(Bound::Released),
            },
            None => self.queue_blob(&blob),
        };
        if handed == Prefetched::Queued {
            call.blobs += 1;
            call.bytes += size;
        }
        None
    }
}

impl LocalCache {
    /// Reads the manifest `digest` through the cache: filled if it is not there, then read from
    /// its file; refused, before anything is fetched, if the key of the call may not read it.
    async fn manifest(&self, digest: &TreeDigest, call: &Call) -> Result<Frame, Refusal> {
        let hex = digest.to_string();
        let key = format!("cas/l/{hex}");
        if !call.may_read(&key) {
            return Err(Refusal::Denied { key });
        }
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
        call.skipped = false;
        call.tree = root.to_string();
        let mut opened = HashSet::from([*root]);
        if call.ended(&self.inner) {
            call.reached = Some(Bound::Released);
            return Ok(());
        }
        let first = self.manifest(root, call).await?;
        let mut entries = first.manifest.children.len() as u64;
        call.limits.admit(entries, entries as usize, 1)?;
        let mut stack = vec![first];
        while let Some(frame) = stack.last_mut() {
            // Before every entry, so before every manifest fetched and every blob handed over.
            if call.ended(&self.inner) {
                call.reached = Some(Bound::Released);
                return Ok(());
            }
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
                    let below = self.manifest(&digest, call).await?;
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

    /// Walks the tree `tree` for `call` and says how it ended, counting it.
    async fn expand_one(&self, tree: &TreeDigest, call: &mut Call) -> (Tree, Walked) {
        let end = match call.reached {
            Some(bound) => Tree::Cut(bound),
            None => match self.walk(tree, call).await {
                Err(refusal) => Tree::Refused(refusal),
                Ok(()) => match (call.reached, call.skipped) {
                    (Some(bound), _) => Tree::Cut(bound),
                    (None, true) => Tree::Cut(Bound::Bytes),
                    (None, false) => Tree::Expanded,
                },
            },
        };
        let how = match end {
            Tree::Expanded => Walked::Expanded,
            Tree::Cut(_) => Walked::Cut,
            Tree::Refused(Refusal::Denied { .. }) => Walked::AccessDenied,
            Tree::Refused(_) => Walked::Refused,
        };
        self.inner.counters.walked(how);
        (end, how)
    }

    /// Expands `trees` in the order given, one at a time, for one call that has no hold.
    #[cfg(test)]
    async fn expand(&self, limits: Limits, trees: &[TreeDigest]) -> Vec<Tree> {
        let mut call = Call::new(limits, None, None);
        let mut listed = HashSet::new();
        let mut ended = Vec::with_capacity(trees.len());
        for tree in trees {
            if listed.insert(*tree) {
                ended.push(self.expand_one(tree, &mut call).await.0);
            }
        }
        ended
    }

    /// The task of the hold `id` of `generation`: expands the trees its calls added, one at a
    /// time, as far as `reads` reaches, until none is left or the hold has ended. The only task
    /// of its hold, so the bounds are the hold's.
    async fn drain(&self, at: (&str, u64), reads: Option<Arc<Scopes>>) {
        let mut call = Call::new(Limits::LIBRARY, Some(at), reads);
        while let Some((tree, spent)) = self.inner.next_tree(at) {
            call.resume(spent);
            let (_, how) = self.expand_one(&tree, &mut call).await;
            // A walk the hold's end cut leaves `Released`, which the next tree finds out above.
            self.inner.hold_walked(at, how, call.spent());
        }
    }

    /// Whether `given` trees are no more than `hydrate_max_trees`.
    fn check_trees(&self, given: usize) -> Result<(), TooManyTrees> {
        let max = self.inner.config.prefetch.max_trees;
        if given > max {
            return Err(TooManyTrees { given, max });
        }
        Ok(())
    }

    /// Whether this cache can take a hydrate call or a release now.
    ///
    /// # Errors
    /// [`HydrateError::Off`] if `prefetch_concurrency` is 0; [`HoldError::ReadOnly`] if this cache
    /// does not hold the lock of its directory.
    pub(in crate::server) fn hydrate_ready(&self) -> Result<(), HydrateError> {
        if self.inner.config.prefetch.concurrency == 0 {
            return Err(HydrateError::Off);
        }
        if !self.inner.read_write() {
            return Err(HoldError::ReadOnly.into());
        }
        Ok(())
    }

    /// Takes a hydrate call of the key `owner`: opens (or adds to) the hold `hold` for `trees`,
    /// and has the task of the hold expand them, in the order given, as far as `reads` reaches. It
    /// answers at once, before any manifest is read: the number of distinct trees of the call. A tree the hold is for already is not expanded again, and a tree listed twice is
    /// expanded once.
    ///
    /// # Errors
    /// [`HydrateError::Off`] and the read-only [`HoldError`] as [`hydrate_ready`](Self::hydrate_ready);
    /// [`TooManyTrees`] for more trees than `hydrate_max_trees` in the call, and
    /// [`HoldError::TooManyTrees`] for more in the hold, with nothing opened or read;
    /// [`HoldError::TooMany`] if `hold_max_count` other holds are open;
    /// [`HoldError::NoSuchHold`] if another key opened a hold of this name.
    pub(in crate::server) fn hydrate(
        &self,
        (hold, owner): (&str, &str),
        expires_ms: Option<u64>,
        trees: &[TreeDigest],
        reads: Option<Arc<Scopes>>,
    ) -> Result<usize, HydrateError> {
        self.hydrate_ready()?;
        self.check_trees(trees.len())?;
        let mut listed = HashSet::new();
        let distinct: Vec<TreeDigest> = trees
            .iter()
            .filter(|tree| listed.insert(**tree))
            .copied()
            .collect();
        let names: Vec<String> = distinct.iter().map(ToString::to_string).collect();
        let opened = self.inner.open_hold(hold, owner, expires_ms, &names)?;
        self.inner.counters.hydrated();
        let at = (hold.to_owned(), opened.generation);
        let new: HashSet<&String> = opened.added.iter().collect();
        let added: Vec<TreeDigest> = distinct
            .into_iter()
            .filter(|tree| new.contains(&tree.to_string()))
            .collect();
        if self.inner.queue_walk((&at.0, at.1), &added) {
            let cache = self.clone();
            drop(tokio::spawn(async move {
                cache.drain((&at.0, at.1), reads).await;
            }));
        }
        Ok(names.len())
    }

    /// Expands the trees of one call, in the order given, into blobs queued to be filled, for no
    /// hold: see the module. A tree listed twice is expanded once. It answers once every tree has
    /// been walked, which fetches the manifests but waits for no blob.
    ///
    /// # Errors
    /// [`TooManyTrees`] if the call lists more than `hydrate_max_trees` trees: nothing is read.
    #[cfg(test)]
    pub(in crate::server) async fn expand_trees(
        &self,
        trees: &[TreeDigest],
    ) -> Result<Vec<Tree>, TooManyTrees> {
        self.check_trees(trees.len())?;
        Ok(self.expand(Limits::LIBRARY, trees).await)
    }
}

#[cfg(test)]
#[path = "expand_tests.rs"]
mod tests;
