//! Prefetch (`docs/design/virtual-build-fs.md` section 6.4): an attach that asks for
//! `prefetch = "all"` has every blob of its tree filled in the background, in manifest order, on
//! the fill socket's permits, and pinned. The attach is answered without waiting for it.
//!
//! One thread fills one tree, one blob at a time: the fill socket decides how many fills run, so
//! a prefetch never takes more than the permits a job's own open would wait for. It stops at the
//! next blob when the tree is no longer attached, and pins what it has filled in batches, so that
//! a tree of many files is not many requests.

use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::sync::Arc;

use crate::tree_manifest::{EntryKind, TreeDigest};

use super::{Core, Loaded, locked};

/// The most blobs one pin request names (a request line is limited, and so is the work of one
/// pin).
const BATCH: usize = 512;

impl Core {
    /// Starts the prefetch of the tree `number` on a thread of its own, unless one runs for it.
    pub(super) fn start_prefetch(self: &Arc<Self>, number: u64) {
        let token = Arc::new(());
        match locked(&self.prefetching).entry(number) {
            Entry::Occupied(_) => return,
            Entry::Vacant(vacant) => {
                let _ = vacant.insert(Arc::clone(&token));
            }
        }
        let core = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("fs-prefetch".to_owned())
            .spawn(move || {
                core.prefetch(number, &token);
                core.finish_prefetch(number, &token);
            });
    }

    /// Removes the marker of the tree `number` if it is the one of `token`: a thread that a detach
    /// and a new attach replaced leaves the marker of its successor.
    fn finish_prefetch(&self, number: u64, token: &Arc<()>) {
        let mut running: std::sync::MutexGuard<'_, HashMap<u64, Arc<()>>> =
            locked(&self.prefetching);
        if running
            .get(&number)
            .is_some_and(|mine| Arc::ptr_eq(mine, token))
        {
            let _ = running.remove(&number);
        }
    }

    /// Whether the thread of `token` is still the prefetch of the tree `number`, and the tree is
    /// attached.
    fn current(&self, number: u64, token: &Arc<()>) -> bool {
        let mine = locked(&self.prefetching)
            .get(&number)
            .is_some_and(|marked| Arc::ptr_eq(marked, token));
        mine && self.attached(number)
    }

    /// Fills and pins every blob of the tree `number`, in manifest order, until it is done, the
    /// tree is no longer attached, or a detach and a new attach have given the tree another
    /// prefetch (the marker of `token` is gone). A blob that cannot be had is recorded on the attachments, as a
    /// failed open records it, and the others go on.
    fn prefetch(&self, number: u64, token: &Arc<()>) {
        let Ok(loaded) = self.tree(number) else {
            return;
        };
        let mut seen = HashSet::new();
        let mut batch = Vec::new();
        for entry in loaded.tree.entries() {
            let EntryKind::File {
                digest,
                size,
                source,
                ..
            } = &entry.kind
            else {
                continue;
            };
            if *size == 0 || !seen.insert(*digest) || locked(&loaded.pinned).contains(digest) {
                continue;
            }
            if !self.current(number, token) {
                return;
            }
            match self.cache_file((digest, *size, source.as_ref())) {
                Ok(_) => batch.push(*digest),
                Err(reason) => self.failed((&loaded, number), entry.index, digest, reason),
            }
            if batch.len() >= BATCH {
                self.pin_batch((number, token), &loaded, &mut batch);
            }
        }
        self.pin_batch((number, token), &loaded, &mut batch);
    }

    /// Whether any run has the tree `number`.
    fn attached(&self, number: u64) -> bool {
        locked(&self.state).attachments.is_attached(number)
    }

    /// Pins `batch` for the tree, and empties it. Under the lifecycle lock and only while the tree
    /// is attached and this thread is its prefetch: a detach that has unpinned the tree is never
    /// followed by a pin.
    fn pin_batch(
        &self,
        (number, token): (u64, &Arc<()>),
        loaded: &Loaded,
        batch: &mut Vec<TreeDigest>,
    ) {
        let _lifecycle = locked(&self.lifecycle);
        if !batch.is_empty() && self.current(number, token) {
            let shas: Vec<String> = batch.iter().map(ToString::to_string).collect();
            let named: Vec<&str> = shas.iter().map(String::as_str).collect();
            self.fills.touch(&named);
            let pinned =
                self.fills
                    .pin(&self.settings.mount_id, &loaded.digest.to_string(), &named);
            if pinned.is_ok_and(|answer| answer.starts_with("ok")) {
                locked(&loaded.pinned).extend(batch.iter().copied());
            }
        }
        batch.clear();
    }

    /// Runs the prefetch of the tree `number` on this thread, as the one the tree has now.
    #[cfg(test)]
    pub(in crate::server::treefs) fn prefetch_here(&self, number: u64) {
        let token = Arc::new(());
        let _ = locked(&self.prefetching).insert(number, Arc::clone(&token));
        self.prefetch(number, &token);
        self.finish_prefetch(number, &token);
    }

    /// The trees a prefetch thread is working on.
    #[cfg(test)]
    pub(in crate::server::treefs) fn prefetching(&self) -> usize {
        locked(&self.prefetching).len()
    }
}

#[cfg(test)]
#[path = "prefetch_tests.rs"]
mod tests;
