//! What the ingest knows of the files of the jobs' directories, with no I/O in it: the digest each
//! committed name was declared with (by a `.part` name the watcher or a scan saw), when a
//! committed name with no declaration was first found, and the files whose put is running.

use std::collections::{BTreeMap, BTreeSet};

use super::GRACE_MS;
use super::names::JobDir;
use crate::TreeDigest;

/// A file of a job's directory: the directory and the file's name.
pub(super) type FileId = (JobDir, String);

/// What is to be done with a committed file now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Put it, as bytes that must hash to this digest.
    Store(TreeDigest),
    /// Its put is running: nothing.
    Running,
    /// No digest was declared for it yet: look again later.
    Wait,
    /// No digest was declared for it within [`GRACE_MS`] of its first finding: refuse it.
    Refuse,
}

/// The state of the ingest.
#[derive(Debug, Default)]
pub(super) struct Ledger {
    /// The digest declared for each committed name.
    declared: BTreeMap<FileId, TreeDigest>,
    /// When (on the clock, in milliseconds) each committed file with no declaration was first found.
    undeclared: BTreeMap<FileId, u64>,
    /// The files whose put is running.
    running: BTreeSet<FileId>,
}

impl Ledger {
    /// Records that `name` of `dir` must hash to `sha256` (the last declaration counts), and
    /// whether a committed file of that name was found waiting for it.
    pub(super) fn declare(&mut self, dir: &JobDir, name: &str, sha256: TreeDigest) -> bool {
        let id = (dir.clone(), name.to_owned());
        let waiting = self.undeclared.remove(&id).is_some();
        let _ = self.declared.insert(id, sha256);
        waiting
    }

    /// What to do with the committed file `id`, found at `now`; a file to put is marked running
    /// until [`Ledger::done`].
    pub(super) fn verdict(&mut self, id: &FileId, now: u64) -> Verdict {
        if self.running.contains(id) {
            return Verdict::Running;
        }
        if let Some(sha256) = self.declared.get(id) {
            let _ = self.running.insert(id.clone());
            return Verdict::Store(*sha256);
        }
        let first = *self.undeclared.entry(id.clone()).or_insert(now);
        if now.saturating_sub(first) >= GRACE_MS {
            let _ = self.undeclared.remove(id);
            Verdict::Refuse
        } else {
            Verdict::Wait
        }
    }

    /// The put of `id` ended: with `forget`, the file is gone (stored or refused) and its
    /// declaration with it; without, it is kept to be put again.
    pub(super) fn done(&mut self, id: &FileId, forget: bool) {
        let _ = self.running.remove(id);
        if forget {
            let _ = self.declared.remove(id);
        }
    }

    /// Whether a put of a file of `dir` is running.
    pub(super) fn busy(&self, dir: &JobDir) -> bool {
        self.running.iter().any(|(of, _)| of == dir)
    }

    /// Keeps only what concerns the directories of `live` (the directories a whole scan found), and
    /// the files whose put is running.
    pub(super) fn keep_only(&mut self, live: &BTreeSet<JobDir>) {
        let running = &self.running;
        let kept = |id: &FileId| live.contains(&id.0) || running.contains(id);
        self.declared.retain(|id, _| kept(id));
        self.undeclared.retain(|id, _| kept(id));
    }

    /// The number of declarations held, and of undeclared files waiting.
    #[cfg(test)]
    pub(super) fn sizes(&self) -> (usize, usize) {
        (self.declared.len(), self.undeclared.len())
    }
}
