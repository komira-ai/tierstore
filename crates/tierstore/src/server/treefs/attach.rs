//! The attachments: which run uses which tree, what each run's opens have failed on, and the file
//! in the runtime directory that keeps the attachments across a restart of the process.
//!
//! An attachment is a run's statement that it will use a tree. Two attaches of one tree for one
//! run are one attachment. A lookup of `trees/<digest>` that no run has attached attaches the tree
//! with no run (an implicit attachment), which ends when the tree has been idle for
//! `fs.tree_idle_secs`. A tree is attached while any run has it or it is attached implicitly. The
//! file holds one line, `<digest> <run>`, for each attachment of a run; it is rewritten by a rename
//! after every change. The implicit attachments are not in it: a start has none, and a lookup
//! attaches again.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::tree_manifest::TreeDigest;

/// The name of the file in the runtime directory.
pub(super) const FILE: &str = "attachments";

/// The longest name of a run, in bytes.
pub(super) const MAX_RUN: usize = 128;

/// An open that failed, as the run's `detach` answers it (`docs/design/virtual-build-fs.md`
/// section 7.4).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct Failure {
    /// The path of the file in the tree.
    pub path: String,
    /// The SHA-256 of the file's contents.
    pub digest: String,
    /// The words of the fill socket's answer, or what else failed.
    pub reason: String,
}

/// One run's attachment of one tree.
#[derive(Debug)]
struct Attached {
    digest: TreeDigest,
    /// The failures since the attach, by path, so that a file opened again is one failure.
    failures: BTreeMap<String, Failure>,
}

impl Attached {
    fn new(digest: TreeDigest) -> Self {
        Self {
            digest,
            failures: BTreeMap::new(),
        }
    }
}

/// Whether `run` is a name the control socket takes: one word of visible ASCII, not empty.
pub(super) fn is_run(run: &str) -> bool {
    !run.is_empty() && run.len() <= MAX_RUN && run.bytes().all(|byte| byte.is_ascii_graphic())
}

/// The attachments of every run, and the file they are kept in.
#[derive(Debug)]
pub(super) struct Attachments {
    file: PathBuf,
    /// The tree number of each tree of each run.
    runs: BTreeMap<String, BTreeMap<u64, Attached>>,
    /// The pairs of tree digest and run a restart could not attach again: not served, but kept in
    /// the file so that a transient failure does not delete the record.
    kept: BTreeSet<(String, String)>,
    /// The trees a lookup attached with no run, by tree number.
    implicit: BTreeMap<u64, TreeDigest>,
}

impl Attachments {
    /// No attachments, kept in `<dir>/attachments`.
    pub(super) fn new(dir: &Path) -> Self {
        Self {
            file: dir.join(FILE),
            runs: BTreeMap::new(),
            kept: BTreeSet::new(),
            implicit: BTreeMap::new(),
        }
    }

    /// The pairs of tree digest and run the file of `dir` holds: none if there is no file. A line
    /// that is not a digest and a run is passed over.
    ///
    /// # Errors
    /// What the system said when the file exists and cannot be read.
    pub(super) fn saved(dir: &Path) -> io::Result<Vec<(TreeDigest, String)>> {
        let text = match std::fs::read_to_string(dir.join(FILE)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(text
            .lines()
            .filter_map(|line| {
                let (digest, run) = line.split_once(' ')?;
                Some((TreeDigest::from_hex(digest)?, run.to_owned())).filter(|(_, run)| is_run(run))
            })
            .collect())
    }

    /// Records that `run` uses the tree `number`, `digest`; whether it was new.
    pub(super) fn add(&mut self, run: &str, number: u64, digest: TreeDigest) -> bool {
        let _ = self.kept.remove(&(digest.to_string(), run.to_owned()));
        self.runs
            .entry(run.to_owned())
            .or_default()
            .insert(number, Attached::new(digest))
            .is_none()
    }

    /// Takes away the one attachment of `run` to the tree `number`, and nothing else.
    pub(super) fn remove_one(&mut self, run: &str, number: u64) {
        let trees = self.runs.entry(run.to_owned()).or_default();
        let _ = trees.remove(&number);
        if trees.is_empty() {
            let _ = self.runs.remove(run);
        }
    }

    /// Keeps the record of `run` using the tree `digest`, which a restart could not attach again.
    pub(super) fn keep(&mut self, run: &str, digest: TreeDigest) {
        let _ = self.kept.insert((digest.to_string(), run.to_owned()));
    }

    /// Takes away every attachment of `run`, and the records kept for it; the trees it had, with
    /// their failures, or none if it attached nothing and nothing is kept for it.
    pub(super) fn remove(&mut self, run: &str) -> Option<Vec<(u64, TreeDigest, Vec<Failure>)>> {
        let before = self.kept.len();
        self.kept.retain(|(_, kept)| kept != run);
        let dropped = self.kept.len() != before;
        let trees = self.runs.remove(run).unwrap_or_default();
        (dropped || !trees.is_empty()).then(|| {
            trees
                .into_iter()
                .map(|(number, got)| (number, got.digest, got.failures.into_values().collect()))
                .collect()
        })
    }

    /// The runs there are.
    pub(super) fn runs(&self) -> Vec<String> {
        let mut runs: Vec<String> = self.runs.keys().cloned().collect();
        runs.extend(self.kept.iter().map(|(_, run)| run.clone()));
        runs.sort();
        runs.dedup();
        runs
    }

    /// Whether any run has the tree `number`.
    pub(super) fn has_run(&self, number: u64) -> bool {
        self.runs.values().any(|trees| trees.contains_key(&number))
    }

    /// Whether any run has the tree `number`, or a lookup attached it.
    pub(super) fn is_attached(&self, number: u64) -> bool {
        self.implicit.contains_key(&number) || self.has_run(number)
    }

    /// Records that a lookup attached the tree `number`, `digest`, with no run.
    pub(super) fn add_implicit(&mut self, number: u64, digest: TreeDigest) {
        let _ = self.implicit.insert(number, digest);
    }

    /// Ends the implicit attachment of the tree `number`.
    pub(super) fn remove_implicit(&mut self, number: u64) {
        let _ = self.implicit.remove(&number);
    }

    /// The trees a lookup attached, with their digests, by number.
    pub(super) fn implicit(&self) -> Vec<(u64, TreeDigest)> {
        self.implicit
            .iter()
            .map(|(number, digest)| (*number, *digest))
            .collect()
    }

    /// The trees attached now, by a run or by a lookup, by digest.
    pub(super) fn trees(&self) -> Vec<TreeDigest> {
        let mut trees: Vec<TreeDigest> = self
            .runs
            .values()
            .flat_map(|trees| trees.values().map(|got| got.digest))
            .chain(self.implicit.values().copied())
            .collect();
        trees.sort_by_key(ToString::to_string);
        trees.dedup();
        trees
    }

    /// Each run's attachment, as a digest and a run, by run.
    pub(super) fn listing(&self) -> Vec<(TreeDigest, String)> {
        self.runs
            .iter()
            .flat_map(|(run, trees)| trees.values().map(move |got| (got.digest, run.clone())))
            .collect()
    }

    /// Records `failure` in the attachment of every run that has the tree `number`.
    pub(super) fn fail(&mut self, number: u64, failure: &Failure) {
        for got in self
            .runs
            .values_mut()
            .filter_map(|trees| trees.get_mut(&number))
        {
            let _ = got.failures.insert(failure.path.clone(), failure.clone());
        }
    }

    /// Writes the file: the attachments, one line each, by a rename of a file written beside it.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn save(&self) -> io::Result<()> {
        let mut text = String::new();
        let attached = self
            .listing()
            .into_iter()
            .map(|(digest, run)| (digest.to_string(), run));
        for (digest, run) in attached.chain(self.kept.iter().cloned()) {
            text.push_str(&format!("{digest} {run}\n"));
        }
        let mut new = self.file.clone().into_os_string();
        new.push(".new");
        std::fs::write(&new, text)?;
        std::fs::rename(&new, &self.file)
    }
}

#[cfg(test)]
#[path = "attach_tests.rs"]
mod tests;
