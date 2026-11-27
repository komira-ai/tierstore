//! The attaches and detaches of runs, the requests of the control socket (`control.rs`), and how
//! long each took: what the mount tells the node for `tierstore_fs_run_attach_seconds`,
//! `tierstore_fs_run_attach_part_seconds` and `tierstore_fs_run_detach_seconds`
//! (`local_cache/runs.rs`).
//!
//! An attach is timed on the core's clock from its start to its answer, whatever the answer, and
//! split at two marks into three parts that add up to it: the end of the last manifest its read
//! fetched (`Fetched::last_fetch`), which ends the fetch, and the end of the read, which ends the
//! flatten; the append is the rest, to the answer. A read that fetched nothing of its own (the
//! tree is in memory, another request is reading it, or it is read again into memory under the
//! number it has) puts the whole read under the fetch, and an attach refused before its read has
//! no fetch and no flatten. A detach is timed from its start to its answer. Each is sent to the
//! node when it ends, as a lookup's attach is (`core/implicit.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use crate::server::local_cache::runs::{RunAttach, RunDetach, RunOutcome};
use crate::tree_manifest::TreeDigest;

use super::super::attach::{Failure, is_run};
use super::super::fills::Report;
use super::super::locked;
use super::fetched::Fetched;
use super::{AttachError, Core, placed};

/// The parts of an attach that started at `start` and ended at `end`, by the end of its read
/// (`read`, none for an attach refused before it) and the end of the last fetch of its read
/// (`fetched`, none for a read that fetched nothing of its own): fetch, flatten and append, in
/// milliseconds, which add up to `end - start`.
fn parts(start: u64, end: u64, read: Option<u64>, fetched: Option<u64>) -> [u64; 3] {
    let read = read.unwrap_or(start).clamp(start, end);
    let fetched = fetched.unwrap_or(read).clamp(start, read);
    [fetched - start, read - fetched, end - read]
}

impl Core {
    /// Makes `run` use the tree `digest` (sixty-four hex digits): reads and flattens it, records
    /// the attachment, and gives the path the job is to be given.
    ///
    /// # Errors
    /// [`AttachError`]: why nothing was attached.
    pub(in crate::server::treefs) fn attach(
        self: &Arc<Self>,
        digest: &str,
        run: &str,
    ) -> Result<PathBuf, AttachError> {
        self.attach_asking(digest, run, false)
    }

    /// [`Core::attach`], and, with `prefetch` (`prefetch = "all"`), every blob of the tree is
    /// filled in the background, in manifest order, without delaying the answer. The node is told
    /// how long it took, and its parts (module docs).
    pub(in crate::server::treefs) fn attach_asking(
        self: &Arc<Self>,
        digest: &str,
        run: &str,
        prefetch: bool,
    ) -> Result<PathBuf, AttachError> {
        let start = self.now();
        let fetched = Fetched::default();
        let mut read = None;
        let attached = self.attach_timed(digest, run, prefetch, &fetched, &mut read);
        let end = self.now().max(start);
        let outcome = match attached {
            Ok(_) => RunOutcome::Ok,
            Err(_) => RunOutcome::Failed,
        };
        self.fills.report(Report::RunAttached(RunAttach {
            outcome,
            ms: end - start,
            parts: parts(start, end, read, fetched.last_fetch()),
        }));
        attached
    }

    /// [`Core::attach_asking`] but for its report: the manifests its read fetches are counted in
    /// `fetched`, and the end of its read is set in `read`.
    fn attach_timed(
        self: &Arc<Self>,
        digest: &str,
        run: &str,
        prefetch: bool,
        fetched: &Fetched,
        read: &mut Option<u64>,
    ) -> Result<PathBuf, AttachError> {
        let digest = TreeDigest::from_hex(digest)
            .filter(|digest| placed::placement_of(digest).is_none())
            .ok_or(AttachError::Digest)?;
        if !is_run(run) {
            return Err(AttachError::Run);
        }
        if self.is_held() {
            return Err(AttachError::Replaced);
        }
        // Read before the lifecycle lock, as a lookup that attaches a tree reads it: a slow fill
        // holds up no attach, no detach and no lookup.
        let tree = self.read_ahead(&digest, fetched);
        *read = Some(self.now());
        let tree = tree.map_err(|why| AttachError::Tree(why.to_string()))?;
        let _lifecycle = locked(&self.lifecycle);
        if self.is_held() {
            return Err(AttachError::Replaced);
        }
        let number = self.table.number_of(&digest)?;
        if let Some(tree) = tree {
            let _ = self.keep(number, digest, tree);
            let _ = self.read_done(number);
        }
        let (added, snapshot) = {
            let mut state = self.state();
            let added = state.attachments.add(run, number, digest);
            (added, state.attachments.snapshot())
        };
        if let Err(error) = self.write_attachments(&snapshot) {
            if added {
                self.state().attachments.remove_one(run, number);
            }
            return Err(error.into());
        }
        if prefetch {
            self.start_prefetch(number);
        }
        Ok(self
            .settings
            .mountpoint
            .join("trees")
            .join(digest.to_string()))
    }

    /// Ends every attachment of `run`: the failures its attachments recorded, or none if it
    /// attached nothing. A tree no run has now and no lookup attached is unpinned, with no lock of
    /// the core's held but that of its pins (`Core::free_tree`), and leaves memory when the kernel
    /// forgets its inodes, or at the sweep once no request has named it for `fs.tree_idle_secs`.
    /// The node is told how long it took, and whether it ended any.
    pub(in crate::server::treefs) fn detach(&self, run: &str) -> Option<Vec<Failure>> {
        let start = self.now();
        let detached = self.detach_run(run);
        let outcome = match detached {
            Some(_) => RunOutcome::Ok,
            None => RunOutcome::Failed,
        };
        let ms = self.now().saturating_sub(start);
        self.fills
            .report(Report::RunDetached(RunDetach { outcome, ms }));
        detached
    }

    /// [`Core::detach`] but for its report.
    fn detach_run(&self, run: &str) -> Option<Vec<Failure>> {
        let (freed, failures, _freeing) = {
            let _lifecycle = locked(&self.lifecycle);
            if self.is_held() {
                return None;
            }
            let freeing = self.freeing();
            let (freed, failures, snapshot) = {
                let mut state = self.state();
                let trees = state.attachments.remove(run)?;
                let mut failures = Vec::new();
                let mut freed = Vec::new();
                for (number, digest, mut failed) in trees {
                    failures.append(&mut failed);
                    if !state.attachments.is_attached(number) {
                        freed.push((number, digest));
                    }
                }
                (freed, failures, state.attachments.snapshot())
            };
            let _ = self.write_attachments(&snapshot);
            for (number, _) in &freed {
                let _ = locked(&self.prefetching).remove(number);
            }
            (freed, failures, freeing)
        };
        for (number, digest) in freed {
            self.free_tree(number, &digest);
        }
        Some(failures)
    }
}

#[cfg(test)]
#[path = "runs_tests.rs"]
mod tests;
