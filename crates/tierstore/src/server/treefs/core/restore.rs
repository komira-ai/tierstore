//! The attachments made again at a start (`fs.reattach_concurrency`): before a frontend serves,
//! every pair of tree digest and run the attachments file holds is attached in memory, which reads
//! the tree's manifests (through the fill socket where the cache does not hold them: a fill is a
//! request of its own, so reading them one tree after the other makes the start as long as the sum
//! of the fills).
//!
//! The trees are read by at most `fs.reattach_concurrency` threads at once, each tree once however
//! many runs attach it. The start still waits for all of them before it serves: a tree that is
//! served while others are read would answer `ENOENT` for a path the job was given, and a client
//! that retries a lookup on a hard mount waits for the mount either way. The results are applied
//! in the order of the file, one thread, so the outcome does not depend on which read finished
//! first.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::tree_manifest::TreeDigest;

use super::{Core, locked};

impl Core {
    /// Attaches again what the attachments file held before a restart: each pair of tree digest
    /// and run whose tree can be read is attached in memory, and the file is written once at the
    /// end. A pair that cannot be attached now is kept in the file, not served, until its run
    /// attaches again or is detached. At most `concurrency` trees are read at once (at least one).
    pub(in crate::server::treefs) fn restore(
        &self,
        saved: Vec<(TreeDigest, String)>,
        concurrency: usize,
    ) {
        let _lifecycle = locked(&self.lifecycle);
        let numbered: Vec<(TreeDigest, String, Result<u64, String>)> = saved
            .into_iter()
            .map(|(digest, run)| {
                let number = self
                    .table
                    .number_of(&digest)
                    .map_err(|error| error.to_string());
                (digest, run, number)
            })
            .collect();
        let mut seen = HashSet::new();
        let trees: Vec<(u64, TreeDigest)> = numbered
            .iter()
            .filter_map(|(digest, _, number)| number.as_ref().ok().map(|number| (*number, *digest)))
            .filter(|(number, _)| seen.insert(*number))
            .collect();
        let failed = self.load_all(&trees, concurrency);
        for (digest, run, number) in numbered {
            let outcome = number.and_then(|number| match failed.get(&number) {
                Some(why) => Err(why.clone()),
                None => Ok(number),
            });
            let mut state = locked(&self.state);
            match outcome {
                Ok(number) => {
                    let _ = state.attachments.add(&run, number, digest);
                }
                Err(why) => {
                    tracing::warn!("tierstore fs: not attaching {digest} for {run} again: {why}");
                    state.attachments.keep(&run, digest);
                }
            }
        }
        if let Err(why) = locked(&self.state).attachments.save() {
            tracing::warn!("tierstore fs: cannot write the attachments file: {why}");
        }
    }

    /// Reads `trees` into memory, `concurrency` at a time; the reason for each that could not be
    /// read, by number.
    fn load_all(&self, trees: &[(u64, TreeDigest)], concurrency: usize) -> HashMap<u64, String> {
        let next = AtomicUsize::new(0);
        let failed = std::sync::Mutex::new(HashMap::new());
        let worker = || {
            while let Some(&(number, digest)) = trees.get(next.fetch_add(1, Ordering::Relaxed)) {
                if let Err(error) = self.load_unless_loaded(number, digest) {
                    let _ = locked(&failed).insert(number, error.to_string());
                }
            }
        };
        std::thread::scope(|scope| {
            for _ in 1..concurrency.clamp(1, trees.len().max(1)) {
                let _ = scope.spawn(worker);
            }
            worker();
        });
        failed
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
