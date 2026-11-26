//! The budget of the version 2 tree lists the mount holds mapped: `fs.tree_list_ram_bytes`.
//!
//! A placement's tree that is a tree list is read in place from a read-only mapping of its image
//! file ([`ListImage`](crate::tree_image_list::ListImage)), one mapping per list however many
//! trees show it, shared with the node's tree store where it runs in this process
//! (`server/local_cache/trees/mapped.rs`). The budget counts the bytes of those mappings, in whole
//! pages, each list once, whoever holds it; a tree of manifests is counted on the heap, as before
//! (`Tree::heap_bytes`). The pages are budgeted, not locked: the kernel may drop one under memory
//! pressure and read it again from the file at the next touch.
//!
//! Before a list that is not mapped yet is mapped, the mount releases lists until the new one
//! fits under the cap ([`Core::make_room`]). A list is held while a run attaches its tree, or a
//! request is in flight on it (anything but the map of the trees and the look itself holds the
//! tree in memory); every other list tree in memory may be released, the one no request has
//! named for longest first, by the `used` stamp the sweep for idle trees reads. A release takes
//! the steps of that sweep, under its locks: it ends the tree's implicit attachment, gives back
//! the node's hold on its placement (`Core::free_tree`), and takes the tree out of memory, which
//! drops its share of the mapping. It is the sweep with a shorter idle bound, the shortest that
//! frees enough. A mapping the tree store also holds (its placement still has a lease or another
//! holder) stays mapped until the store lets it go too, so a release can free less than the list.
//!
//! A release is not an end: every inode of a tree can be derived from its number (`ino.rs`), so a
//! request that names a released tree reads it again, as after the sweep, while its placement
//! lives, and is answered `ENOENT` once it has ended.
//!
//! A mount never fails for the cap. If every list is held and the new one still does not fit,
//! it is mapped past the cap, counted, and said at warn once in `SAY_EVERY_MS` (`core/said.rs`);
//! the next release a detach allows brings the total under the cap again.
//!
//! What the mount holds mapped, the cap, and the lists released and mapped past the cap are told
//! to the node at each look (`treelists`, `local_cache/tree_lists.rs`), and said in the line of
//! the trees in memory (`core/memory.rs`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::TreeDigest;
use crate::server::local_cache::tree_lists::{Totals, TreeLists};
use crate::server::local_cache::trees::{Mapped, MappedLists};

use super::super::fills::Report;
use super::implicit::ended_with;
use super::said::Failing;
use super::{Core, Errno, Loaded, locked};

/// The lists the mount holds mapped, and what it released and mapped past the cap.
#[derive(Debug, Default)]
pub(super) struct Lists {
    /// The lists in memory, by digest: the tree store's where it is in this process.
    pub(super) mapped: Arc<MappedLists>,
    /// The lists released to make room under the cap, since the process started.
    cap: AtomicU64,
    /// The lists the sweep for idle trees released, since the process started.
    idle: AtomicU64,
    /// The lists mapped past the cap, since the process started.
    over_cap: AtomicU64,
    /// The releases for the cap and when idle at the last look: what the line of the trees in
    /// memory says since then.
    looked: Mutex<(u64, u64)>,
}

/// What the line of the trees in memory says of the lists (`core/memory.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Said {
    /// The lists mapped, and their bytes.
    pub mapped: Mapped,
    /// `fs.tree_list_ram_bytes`.
    pub cap: u64,
    /// The lists released for the cap, and those released idle, since the look before.
    pub released: (u64, u64),
}

impl Lists {
    /// The budget of a mount that shares `mapped` with the tree store of its process.
    pub(super) fn new(mapped: Arc<MappedLists>) -> Self {
        Self {
            mapped,
            ..Self::default()
        }
    }

    /// Counts `trees` that the sweep for idle trees took out of memory, those of them that are
    /// lists.
    pub(super) fn idle<'a>(&self, trees: impl Iterator<Item = &'a Arc<Loaded>>) {
        let lists = trees.filter(|loaded| loaded.tree.list().is_some()).count();
        let _ = self.idle.fetch_add(lists as u64, Ordering::AcqRel);
    }

    fn totals(&self) -> Totals {
        Totals {
            cap: self.cap.load(Ordering::Acquire),
            idle: self.idle.load(Ordering::Acquire),
            over_cap: self.over_cap.load(Ordering::Acquire),
        }
    }
}

impl Core {
    /// Releases list trees until a list of `need` more bytes fits under `fs.tree_list_ram_bytes`
    /// (module docs); whether it fits. Called with no lock of the core held, before the list is
    /// mapped.
    pub(super) fn make_room(&self, need: u64) -> bool {
        let cap = self.settings.tree_list_ram_bytes;
        // Each release takes a tree out of memory, so there are never more than there are trees;
        // a tree a request attached again in the meantime is not released twice for one list.
        // (Counted first: the trees of a look in the head of the loop would be held through it.)
        let most = self.trees.all().len();
        for _ in 0..=most {
            if self.lists.mapped.mapped().bytes.saturating_add(need) <= cap {
                return true;
            }
            if !self.release_one() {
                break;
            }
        }
        false
    }

    /// Releases the list tree that no run attaches, no request is in flight on, and no request
    /// has named for longest: the steps of the sweep for idle trees for that one tree
    /// (`Core::detach_idle`, `Core::drop_idle`); whether one was taken out of memory. None is
    /// released while a successor takes over.
    fn release_one(&self) -> bool {
        let all = self.trees.all();
        let (number, loaded, freeing) = {
            let _lifecycle = locked(&self.lifecycle);
            if self.is_held() {
                return false;
            }
            let freeing = self.freeing();
            let mut state = self.state();
            // The map of the trees and `all` hold each tree: one more holder is a request.
            let victim = all
                .iter()
                .filter(|(number, loaded)| {
                    loaded.tree.list().is_some()
                        && !state.attachments.has_run(*number)
                        && Arc::strong_count(loaded) <= 2
                })
                .min_by_key(|(number, loaded)| (loaded.used.load(Ordering::Acquire), *number));
            let Some((number, loaded)) = victim else {
                return false;
            };
            let failures = state.attachments.remove_implicit(*number);
            if !failures.is_empty() {
                tracing::warn!("{}", ended_with((&loaded.digest, *number), &failures));
            }
            (*number, Arc::clone(loaded), freeing)
        };
        drop(all);
        self.free_tree(number, &loaded.digest);
        // The guard goes before the lifecycle lock is taken again, as in `Core::detach_idle`.
        drop(freeing);
        let gone = {
            let _lifecycle = locked(&self.lifecycle);
            (!self.attached(number) && loaded.lookups.remove())
                .then(|| self.unmap(number, Arc::clone(&loaded)))
                .flatten()
        };
        // `free_tree` takes out a tree the kernel holds no inode of, with no lock held.
        let released = gone.is_some()
            || self
                .trees
                .get(number)
                .is_none_or(|there| !Arc::ptr_eq(&there, &loaded));
        drop(loaded);
        if let Some(gone) = gone {
            self.free(gone);
        }
        if released {
            let _ = self.lists.cap.fetch_add(1, Ordering::AcqRel);
        }
        released
    }

    /// Whether a tree of the mount in memory shows the list `tree` now.
    pub(super) fn shown(&self, tree: &TreeDigest) -> bool {
        let Some(list) = self.lists.mapped.get(tree) else {
            return false;
        };
        let trees = self.trees.all();
        trees.iter().any(|(_, loaded)| {
            loaded
                .tree
                .list()
                .is_some_and(|ours| Arc::ptr_eq(ours, &list))
        })
    }

    /// Counts the list `tree` mapped past the cap, and says so at warn, once in `SAY_EVERY_MS`.
    pub(super) fn over_cap(&self, tree: &TreeDigest) {
        let _ = self.lists.over_cap.fetch_add(1, Ordering::AcqRel);
        let mapped = self.lists.mapped.mapped().bytes;
        let cap = self.settings.tree_list_ram_bytes;
        self.say_failed((0, Failing::Lists), Some(tree), &|| {
            format!(
                "tierstore fs: tree list {tree} mapped past fs.tree_list_ram_bytes: {mapped} of \
                 {cap} mapped, every mapped list held"
            )
        });
    }

    /// What the line of a lookup that `attached` a tree says of it if it is a tree list: the bytes
    /// of the list, so that the time of an attach can be read by the size of its list.
    pub(super) fn list_said(&self, attached: &Result<u64, Errno>) -> String {
        let bytes = attached
            .as_ref()
            .ok()
            .and_then(|number| self.trees.get(*number))
            .and_then(|loaded| loaded.tree.list().map(|list| list.list_bytes()));
        bytes.map_or_else(String::new, |bytes| {
            format!("; a tree list of {bytes} bytes")
        })
    }

    /// Tells the node the lists mapped, the cap, and the running totals of the releases and of
    /// the lists mapped past the cap (`treelists`).
    pub(in crate::server::treefs) fn report_tree_lists(&self) {
        let mapped = self.lists.mapped.mapped();
        self.fills.report(Report::TreeLists(TreeLists {
            mount: self.settings.mount_id.clone(),
            process: u64::from(std::process::id()),
            mapped: mapped.lists,
            bytes: mapped.bytes,
            cap: self.settings.tree_list_ram_bytes,
            totals: self.lists.totals(),
        }));
    }

    /// What the line of the trees in memory says of the lists, with the releases since the look
    /// before; the look is the one that says it.
    pub(super) fn lists_said(&self) -> Said {
        let totals = self.lists.totals();
        let mut looked = locked(&self.lists.looked);
        let (cap, idle) = *looked;
        *looked = (totals.cap, totals.idle);
        Said {
            mapped: self.lists.mapped.mapped(),
            cap: self.settings.tree_list_ram_bytes,
            released: (totals.cap - cap, totals.idle - idle),
        }
    }
}

#[cfg(test)]
#[path = "lists_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "lists_live_tests.rs"]
mod live_tests;
