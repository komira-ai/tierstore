//! The version 2 tree lists the process holds in memory, by the digest of the list: one
//! [`ListImage`] per list, shared by every holder (the tree store's tree, and each tree of the
//! mount that shows it), so a list is mapped once and counted once however many hold it.
//!
//! The registry holds each list weakly: a list leaves it when its last holder drops it, and its
//! mapping is released then. It never holds a list alive itself, so what keeps a list in memory is
//! what holds it: the tree store while a placement reaches its tree, and the mount while a lookup
//! or a run attaches it and the cap of `fs.tree_list_ram_bytes` leaves it there
//! (`server/treefs/core/lists.rs`). The tree store and the mount of one process (`tierstore serve`
//! with an `fs` interface) share one registry; a mount of its own process (`tierstore fs`) has its
//! own, and the two map the same files.
//!
//! The lock of the registry is a leaf: nothing else is taken under it, and no file is read or
//! mapped while it is held, so a slow disk holds up no other list.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use super::super::locked;
use crate::TreeDigest;
use crate::tree_image_list::{ImageFileError, ListImage, in_pages};

/// The lists in memory, by digest (module docs).
#[derive(Debug, Default)]
pub(in crate::server) struct MappedLists {
    lists: Mutex<HashMap<TreeDigest, Weak<ListImage>>>,
}

/// The lists mapped now, and the bytes their mappings take in whole pages: each list once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Mapped {
    /// The lists mapped.
    pub lists: u64,
    /// The bytes of their mappings, in whole pages.
    pub bytes: u64,
}

impl MappedLists {
    /// The list `tree`, if something holds it.
    pub(in crate::server) fn get(&self, tree: &TreeDigest) -> Option<Arc<ListImage>> {
        locked(&self.lists).get(tree).and_then(Weak::upgrade)
    }

    /// Keeps `image` as the list `tree`, unless a holder has the list already: the list that is
    /// shared from now on. An `image` that is not kept is dropped, with its mapping, with no lock
    /// held.
    pub(in crate::server) fn keep(&self, tree: TreeDigest, image: ListImage) -> Arc<ListImage> {
        let fresh = Arc::new(image);
        let kept = {
            let mut lists = locked(&self.lists);
            match lists.get(&tree).and_then(Weak::upgrade) {
                Some(there) => there,
                None => {
                    let _ = lists.insert(tree, Arc::downgrade(&fresh));
                    Arc::clone(&fresh)
                }
            }
        };
        drop(fresh);
        kept
    }

    /// The list `tree` from its image file at `path`: the one in memory, else read, walked once
    /// for its shape, mapped and kept ([`ListImage::open_file`]).
    ///
    /// # Errors
    /// [`ImageFileError`]: the file cannot be read or mapped, or holds no list.
    pub(in crate::server) fn open(
        &self,
        tree: &TreeDigest,
        path: &Path,
    ) -> Result<Arc<ListImage>, ImageFileError> {
        if let Some(there) = self.get(tree) {
            return Ok(there);
        }
        let image = ListImage::open_file(path)?;
        Ok(self.keep(*tree, image))
    }

    /// The bytes a mapping of the list `tree` would add, in whole pages: none if it is in memory,
    /// else the length of its image file at `path` (none for a file that cannot be measured,
    /// which is not mapped either).
    pub(in crate::server) fn need(&self, tree: &TreeDigest, path: &Path) -> u64 {
        if self.get(tree).is_some() {
            return 0;
        }
        std::fs::metadata(path).map_or(0, |meta| in_pages(meta.len()))
    }

    /// The lists mapped now and their bytes, each once; the lists no one holds any longer are
    /// forgotten.
    pub(in crate::server) fn mapped(&self) -> Mapped {
        let mut lists = locked(&self.lists);
        lists.retain(|_, list| list.strong_count() > 0);
        lists
            .values()
            .filter_map(Weak::upgrade)
            .filter(|list| list.is_mapped())
            .fold(Mapped::default(), |sum, list| Mapped {
                lists: sum.lists + 1,
                bytes: sum.bytes + list.mapped_bytes(),
            })
    }
}

#[cfg(test)]
#[path = "mapped_tests.rs"]
mod tests;
