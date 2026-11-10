//! Prefetch (`docs/design/virtual-build-fs.md` section 6.4): an attach that asks for
//! `prefetch = "all"` has every blob of its tree filled in the background, in manifest order, on
//! the fill socket's permits, and pinned. The attach is answered without waiting for it.
//!
//! One thread fills one tree, one blob at a time: the fill socket decides how many fills run, so
//! a prefetch never takes more than the permits a job's own open would wait for. It stops at the
//! next blob when the tree is no longer attached, and pins what it has filled in batches, so that
//! a tree of many files is not many requests.

use std::collections::HashSet;
use std::sync::Arc;

use crate::tree_manifest::{EntryKind, TreeDigest};

use super::{Core, Loaded, locked};

/// The most blobs one pin request names (a request line is limited, and so is the work of one
/// pin).
const BATCH: usize = 512;

impl Core {
    /// Starts the prefetch of the tree `number` on a thread of its own, unless one runs for it.
    pub(super) fn start_prefetch(self: &Arc<Self>, number: u64) {
        if !locked(&self.prefetching).insert(number) {
            return;
        }
        let core = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("fs-prefetch".to_owned())
            .spawn(move || {
                core.prefetch(number);
                let _ = locked(&core.prefetching).remove(&number);
            });
    }

    /// Fills and pins every blob of the tree `number`, in manifest order, until it is done or the
    /// tree is no longer attached. A blob that cannot be had is recorded on the attachments, as a
    /// failed open records it, and the others go on.
    pub(in crate::server::treefs) fn prefetch(&self, number: u64) {
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
            if !self.attached(number) {
                return;
            }
            match self.cache_file((digest, *size, source.as_ref())) {
                Ok(_) => batch.push(*digest),
                Err(reason) => self.failed((&loaded, number), entry.index, digest, reason),
            }
            if batch.len() >= BATCH {
                self.pin_batch(number, &loaded, &mut batch);
            }
        }
        self.pin_batch(number, &loaded, &mut batch);
    }

    /// Whether any run has the tree `number`.
    fn attached(&self, number: u64) -> bool {
        locked(&self.state).attachments.is_attached(number)
    }

    /// Pins `batch` for the tree, and empties it. Under the lifecycle lock and only while the tree
    /// is attached: a detach that has unpinned the tree is never followed by a pin.
    fn pin_batch(&self, number: u64, loaded: &Loaded, batch: &mut Vec<TreeDigest>) {
        let _lifecycle = locked(&self.lifecycle);
        if !batch.is_empty() && self.attached(number) {
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

    /// The trees a prefetch thread is working on.
    #[cfg(test)]
    pub(in crate::server::treefs) fn prefetching(&self) -> usize {
        locked(&self.prefetching).len()
    }
}

#[cfg(test)]
#[path = "prefetch_tests.rs"]
mod tests;
