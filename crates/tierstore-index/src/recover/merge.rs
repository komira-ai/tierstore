//! The k-way merge of sorted runs of records into one stream in key order.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::iter::Peekable;
use std::vec::IntoIter;

use tierstore_format::stamp::Stamp;
use tierstore_store_api::index::{Fact, IndexEntry};

use super::facts::Run;

/// A run being merged: its records, the next one peekable.
type Source = Peekable<IntoIter<(Vec<u8>, IndexEntry)>>;

/// An iterator of [`Fact::Record`] over the records of several runs in key order; the records of
/// one key come in stamp order. The heap holds a copy of each run's next key, the runs hold the
/// records.
pub(super) struct Merge {
    runs: Vec<Source>,
    heap: BinaryHeap<Reverse<(Vec<u8>, Stamp, usize)>>,
}

impl Merge {
    /// The merge of `runs`, each sorted by key and stamp.
    pub(super) fn new(runs: Vec<Run>) -> Self {
        let mut merge = Self {
            runs: runs.into_iter().map(|r| r.into_iter().peekable()).collect(),
            heap: BinaryHeap::new(),
        };
        for run in 0..merge.runs.len() {
            merge.push_head(run);
        }
        merge
    }

    /// Puts the next record of `run`, if it has one, on the heap.
    fn push_head(&mut self, run: usize) {
        if let Some((key, entry)) = self.runs[run].peek() {
            self.heap.push(Reverse((key.clone(), entry.stamp, run)));
        }
    }
}

impl Iterator for Merge {
    type Item = Fact;

    fn next(&mut self) -> Option<Fact> {
        let Reverse((_, _, run)) = self.heap.pop()?;
        let item = self.runs[run].next();
        self.push_head(run);
        item.map(|(key, entry)| Fact::Record { key, entry })
    }
}
