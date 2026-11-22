//! A whole look at the put root: every build's directory `cas/b/<build>` and every job's directory
//! `cas/b/<build>/<nonce>`, with the time each was last changed, and the committed files of a job's
//! directory and the digests its `.part` names declare. Only directories are walked, never a link
//! to one, and only the files directly in a job's directory are looked at. It reads the disk: it
//! runs on a blocking thread.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::names::{JobDir, Name, PREFIX, is_component, name_of};
use crate::TreeDigest;

/// What a whole scan found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Found {
    /// Every job's directory, and what it holds.
    pub(super) jobs: BTreeMap<JobDir, Job>,
    /// Every build's directory, relative to the root, and when it was last changed.
    pub(super) builds: Vec<(PathBuf, u64)>,
}

/// What a job's directory holds.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Job {
    /// When the directory was last changed, in milliseconds since the epoch.
    pub(super) changed: u64,
    /// Its committed files, by name.
    pub(super) committed: Vec<String>,
    /// The digests its `.part` names declare: the committed name each is for, and the digest.
    pub(super) declared: Vec<(String, TreeDigest)>,
}

/// The names of the directories in `dir` that may be components, sorted; none if it cannot be
/// read.
fn dirs_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_component(name))
        .collect();
    names.sort();
    names
}

/// The milliseconds since the epoch of `time`, zero before it.
fn millis(time: std::time::SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// When the directory at `dir` was last changed, in milliseconds since the epoch.
fn changed(dir: &Path) -> Option<u64> {
    let meta = fs::symlink_metadata(dir).ok()?;
    Some(millis(meta.modified().ok()?))
}

/// What the job's directory at `dir` holds; none if it cannot be read.
fn job_at(dir: &Path) -> Option<Job> {
    let mut job = Job {
        changed: changed(dir)?,
        ..Job::default()
    };
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !is_component(&name) || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        match name_of(&name) {
            Name::Part => {}
            Name::Declares { name, sha256 } => job.declared.push((name, sha256)),
            Name::Committed => job.committed.push(name),
        }
    }
    job.committed.sort();
    job.declared.sort_by(|a, b| a.0.cmp(&b.0));
    Some(job)
}

/// Every build's and job's directory under `root`, and what each job's holds.
pub(super) fn scan(root: &Path) -> Found {
    let [cas, b] = PREFIX;
    let rel = Path::new(cas).join(b);
    let mut found = Found::default();
    for build in dirs_in(&root.join(&rel)) {
        let build_rel = rel.join(&build);
        let job_names = dirs_in(&root.join(&build_rel));
        for dir in job_names
            .iter()
            .filter_map(|job_name| JobDir::new(&build, job_name))
        {
            if let Some(job) = job_at(&root.join(dir.rel())) {
                let _ = found.jobs.insert(dir, job);
            }
        }
        if let Some(when) = changed(&root.join(&build_rel)) {
            found.builds.push((build_rel, when));
        }
    }
    found
}
