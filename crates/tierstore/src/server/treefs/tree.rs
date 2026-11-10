//! A tree flattened for lookups: its entries in the order of their indices, and for each
//! directory the indices of its children.

use crate::tree_manifest::{EntryKind, TreeEntry};

/// The entries of one tree.
#[derive(Debug)]
pub(super) struct Tree {
    entries: Vec<TreeEntry>,
    /// `starts[i]..starts[i + 1]` is where the children of entry `i` are in `kids`.
    starts: Vec<u32>,
    /// The indices of the children of every entry, grouped by parent, each group in the order of
    /// the manifest (sorted by name).
    kids: Vec<u32>,
}

impl Tree {
    /// The tree of `entries`, which `flatten` made: the root first, a parent before its children.
    pub(super) fn new(entries: Vec<TreeEntry>) -> Self {
        let mut starts = vec![0u32; entries.len() + 1];
        for parent in entries.iter().filter_map(|entry| entry.parent) {
            starts[parent as usize + 1] += 1;
        }
        for i in 1..starts.len() {
            starts[i] += starts[i - 1];
        }
        let mut next = starts.clone();
        let mut kids = vec![0u32; entries.len().saturating_sub(1)];
        for entry in &entries {
            if let Some(parent) = entry.parent {
                kids[next[parent as usize] as usize] = entry.index;
                next[parent as usize] += 1;
            }
        }
        Self {
            entries,
            starts,
            kids,
        }
    }

    /// Every entry, in the order of their indices: the manifest's, depth first.
    pub(super) fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// The entry with index `index`.
    pub(super) fn entry(&self, index: u32) -> Option<&TreeEntry> {
        self.entries.get(index as usize)
    }

    /// The entry with index `index`, which the caller got from this tree.
    pub(super) fn at(&self, index: u32) -> &TreeEntry {
        &self.entries[index as usize]
    }

    /// The indices of the children of the directory `dir`, by name; none for anything else.
    pub(super) fn children(&self, dir: u32) -> &[u32] {
        let (from, to) = (self.starts[dir as usize], self.starts[dir as usize + 1]);
        &self.kids[from as usize..to as usize]
    }

    /// The index of the child of `dir` called `name`.
    pub(super) fn child(&self, dir: u32, name: &str) -> Option<u32> {
        let kids = self.children(dir);
        let at = kids
            .binary_search_by(|&kid| self.entries[kid as usize].name.as_ref().cmp(name))
            .ok()?;
        Some(kids[at])
    }

    /// The link count of the directory `dir`: 2 and its subdirectories.
    pub(super) fn links(&self, dir: u32) -> u32 {
        let dirs = self
            .children(dir)
            .iter()
            .filter(|&&kid| matches!(self.entries[kid as usize].kind, EntryKind::Dir { .. }))
            .count();
        2 + u32::try_from(dirs).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
