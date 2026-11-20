//! The flattening of a tree whose manifests are fetched a level at a time: every manifest of a
//! level that is not held yet is fetched before any of the level below, by up to `width` threads
//! at once. A tree of depth d then costs d rounds of fetches however wide its levels are, where
//! [`flatten`](super::flatten) fetches one manifest after another and costs a round for each.
//!
//! The entries are those [`flatten`](super::flatten) makes of the same manifests: the walk that
//! measures and numbers the tree is the same, over the manifests gathered here. Each distinct
//! manifest is fetched once. The limits are judged as each level comes in, so a tree over
//! [`MAX_ENTRIES`] or [`MAX_DEPTH`] is refused with no level below the one that passes the limit
//! fetched; the threads of that level stop taking manifests once one of them passes it, or once a
//! fetch fails. A tree with one fault is refused for it as `flatten` refuses it. A tree with
//! several may be refused for another of them than the one `flatten` meets first, since the
//! levels are fetched in another order than the walk's.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::{
    EntryKind, MAX_DEPTH, MAX_ENTRIES, TreeDigest, TreeEntry, TreeError, TreeManifest, entries_of,
    measure, parsed,
};

/// The name of the threads that help fetch a level.
const HELPER: &str = "fs-manifests";

/// Flattens the tree whose root manifest has digest `root`, as [`flatten`](super::flatten) does,
/// fetching the manifests of each level by up to `width` calls of `fetch` at once.
///
/// `fetch` returns the bytes of the manifest with the digest it is given, and is called from
/// several threads at once. A tree over a limit is refused before any entry is made.
pub(crate) fn flatten_in_waves<E: fmt::Display>(
    root: &TreeDigest,
    width: usize,
    fetch: impl Fn(&TreeDigest) -> Result<Vec<u8>, E> + Sync,
) -> Result<Vec<TreeEntry>, TreeError> {
    let held = gather(root, width, &fetch)?;
    let measured = measure(root, |digest| {
        let manifest = held.get(digest).cloned();
        manifest.ok_or_else(|| TreeError::Fetch {
            digest: *digest,
            reason: "not gathered".to_owned(),
        })
    })?;
    Ok(entries_of(root, &measured))
}

/// Every manifest of the tree `root`, by digest, fetched a level at a time. A level is refused
/// for the first manifest, in the order the level lists them, that cannot be fetched or read, or
/// that takes the children of the manifests fetched so far over [`MAX_ENTRIES`], or that has
/// children deeper than [`MAX_DEPTH`].
fn gather<E: fmt::Display>(
    root: &TreeDigest,
    width: usize,
    fetch: &(impl Fn(&TreeDigest) -> Result<Vec<u8>, E> + Sync),
) -> Result<HashMap<TreeDigest, Arc<TreeManifest>>, TreeError> {
    let mut held = HashMap::new();
    let mut seen = HashSet::from([*root]);
    let mut opened = 0u64;
    let mut digests = vec![*root];
    // The children of the manifests of `digests` are at `depth`.
    let mut depth = 1;
    while !digests.is_empty() {
        let level = Level {
            digests: &digests,
            depth,
            next: AtomicUsize::new(0),
            opened: AtomicU64::new(opened),
            stop: AtomicBool::new(false),
        };
        let got = level.fetch(width, fetch);
        let mut below = Vec::new();
        for (digest, manifest) in digests.iter().zip(got) {
            let manifest = manifest?;
            opened += manifest.children.len() as u64;
            if opened > MAX_ENTRIES {
                return Err(TreeError::TreeTooLarge);
            }
            if !manifest.children.is_empty() && depth > MAX_DEPTH {
                return Err(TreeError::TreeTooDeep);
            }
            for child in &manifest.children {
                let EntryKind::Dir { digest: dir, .. } = &child.kind else {
                    continue;
                };
                if seen.insert(*dir) {
                    below.push(*dir);
                }
            }
            held.insert(*digest, manifest);
        }
        digests = below;
        depth += 1;
    }
    Ok(held)
}

/// One level of the tree being fetched, as the threads that fetch it share it: the next manifest
/// to take, the children of every manifest opened so far, and whether to stop taking more.
struct Level<'a> {
    digests: &'a [TreeDigest],
    /// The depth of the children of the manifests of the level.
    depth: usize,
    next: AtomicUsize,
    opened: AtomicU64,
    stop: AtomicBool,
}

/// What a fetch of a manifest of a level gave, by its place in the level.
type Got = (usize, Result<Arc<TreeManifest>, TreeError>);

impl Level<'_> {
    /// The manifests of the level, fetched and read by this thread and up to `width - 1` more, in
    /// the order of the level: all of them, or those up to the one that stopped the level and
    /// any the other threads had taken then.
    fn fetch<E: fmt::Display>(
        &self,
        width: usize,
        fetch: &(impl Fn(&TreeDigest) -> Result<Vec<u8>, E> + Sync),
    ) -> Vec<Result<Arc<TreeManifest>, TreeError>> {
        let helpers = width.min(self.digests.len()).saturating_sub(1);
        let mut got = std::thread::scope(|scope| {
            let helping: Vec<_> = (0..helpers)
                .filter_map(|_| {
                    let helper = std::thread::Builder::new().name(HELPER.to_owned());
                    // A thread that cannot be started leaves its share to the others.
                    helper.spawn_scoped(scope, || self.take(fetch)).ok()
                })
                .collect();
            let mut got = self.take(fetch);
            for helper in helping {
                match helper.join() {
                    Ok(theirs) => got.extend(theirs),
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            got
        });
        got.sort_unstable_by_key(|(index, _)| *index);
        got.into_iter().map(|(_, manifest)| manifest).collect()
    }

    /// Takes the manifests of the level one at a time, fetches and reads each, until none is left
    /// or the level stops: a fetch or a read failed, or a manifest passed a limit. The places
    /// handed out are 0, 1, 2, ... in turn, so the manifests taken are always the first ones of
    /// the level.
    fn take<E: fmt::Display>(
        &self,
        fetch: &impl Fn(&TreeDigest) -> Result<Vec<u8>, E>,
    ) -> Vec<Got> {
        let mut got = Vec::new();
        while !self.stop.load(Ordering::Acquire) {
            let index = self.next.fetch_add(1, Ordering::AcqRel);
            let Some(digest) = self.digests.get(index) else {
                break;
            };
            let manifest = parsed(digest, fetch(digest));
            let stops = match &manifest {
                Ok(manifest) => self.passes_a_limit(manifest),
                Err(_) => true,
            };
            if stops {
                self.stop.store(true, Ordering::Release);
            }
            got.push((index, manifest));
        }
        got
    }

    /// Adds the children of `manifest` to those opened: whether the tree is now over
    /// [`MAX_ENTRIES`], or `manifest` has children deeper than [`MAX_DEPTH`].
    fn passes_a_limit(&self, manifest: &TreeManifest) -> bool {
        let children = manifest.children.len() as u64;
        let opened = self.opened.fetch_add(children, Ordering::AcqRel) + children;
        opened > MAX_ENTRIES || (children > 0 && self.depth > MAX_DEPTH)
    }
}

#[cfg(test)]
#[path = "waves_tests.rs"]
mod tests;
