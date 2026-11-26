//! The merge queue of `main` (docs/ci-and-release.md, "Merging into main"): the checks it requires
//! are the jobs [`REQUIRED`] names, and [`merge_queue_problems`] holds the workflows to what the
//! queue needs of them. The queue builds each pull request merged with the ones ahead of it on a
//! branch of its own (`gh-readonly-queue/main/...`), starts every workflow with a `merge_group`
//! trigger on that commit, and lands it only once every required check has passed there. So:
//!
//! - every workflow of [`REQUIRED`] is there and has the `merge_group` trigger, with no filter (or
//!   the one type GitHub has, `checks_requested`): without it the queue waits for a check that never
//!   starts until it gives up; and its `pull_request` trigger has no filter either, since a pull
//!   request that a filter skips never reports the required checks and can never be queued;
//! - no other workflow has the trigger: a queue run starts only the checks the queue waits for;
//! - the jobs of such a workflow are exactly the required ones: a job added to it is either
//!   required (and named in [`REQUIRED`] and the ruleset) or belongs in another workflow;
//! - each of those jobs reports a check under its own id on the merge group's commit: no `name:`
//!   other than the id and no matrix (both rename the check), no `if:` (a skipped required check
//!   counts as passed), no call of a reusable workflow; it runs on a named GitHub-hosted Linux
//!   x86_64 runner (AGENTS.md B9 of this repository: what gates a pull request runs there alone),
//!   and holds no write scope (nothing is published, deployed or versioned from a queue run);
//! - the workflow's concurrency never cancels a queue run and never makes one wait on another: its
//!   group ends in [`GROUP_TAIL`] (a run that is not a pull request's has a group of its own, its
//!   run id) and `cancel-in-progress` is one of [`CANCEL`], or the workflow has no `concurrency`.
//!
//! What a queue run must not start elsewhere is held by the rules of those files: release.yml is
//! started by the completion of `ci` on `main` alone and acts on a run for a push alone
//! ([`super::lint::release_problems`]), and a queue run's branch is never `main`.

use super::lint::{entries, entry, names, scalars};
use super::pr_runners::linux_x86_64;
use super::yaml::{self, Node, Value};

/// The required checks of the merge queue of `main`, by workflow file: the ids of its jobs. The
/// ruleset that turns the queue on names exactly these jobs as its required status checks.
pub const REQUIRED: [(&str, &[&str]); 1] = [("ci.yml", &["lint", "test"])];

/// How a queued workflow's concurrency group ends: a pull request's runs share the group of its
/// ref, and any other run (a push, a merge group) has one of its own.
pub const GROUP_TAIL: &str =
    "${{ github.event_name == 'pull_request' && github.ref || github.run_id }}";

/// The values of `cancel-in-progress` that never cancel a queue run.
pub const CANCEL: [&str; 2] = ["false", "${{ github.event_name == 'pull_request' }}"];

/// One line per way the workflows `files` (`(file name, text)`) fail the merge queue.
pub fn merge_queue_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, jobs) in REQUIRED {
        if !files.iter().any(|(name, _)| name == file) {
            problems.push(format!(
                "{file}: missing, and its jobs {jobs:?} are the required checks of the merge queue"
            ));
        }
    }
    for (file, text) in files {
        let doc = match yaml::load(text) {
            Ok(doc) => doc,
            Err(e) => {
                problems.push(format!("{file}: {e}"));
                continue;
            }
        };
        let on = entry(&doc, "on");
        let queued = names(on).iter().any(|t| t == "merge_group");
        match REQUIRED.iter().find(|(f, _)| *f == file.as_str()) {
            Some((_, jobs)) => {
                problems.extend(trigger_problems(file, on));
                problems.extend(job_problems(file, &doc, jobs));
                problems.extend(concurrency_problems(file, &doc));
            }
            None if queued => problems.push(format!(
                "{file}: runs on merge_group, but none of its jobs is a required check of the merge queue (merge_queue::REQUIRED)"
            )),
            None => {}
        }
    }
    problems
}

/// Whether the trigger `key` of `on` is there with no filter: a bare key (`key:`, `key: {}`), or a
/// name in the list or scalar form of `on`, which hold no filter.
fn unfiltered(on: &Node, key: &str) -> bool {
    match &on.value {
        Value::Map(_) => entry(on, key).is_some_and(is_bare),
        _ => names(Some(on)).iter().any(|t| t == key),
    }
}

/// Whether `node` is a null scalar or an empty mapping.
fn is_bare(node: &Node) -> bool {
    node.is_empty_map() || (matches!(node.value, Value::Scalar { .. }) && node.as_str().is_none())
}

/// Whether `node` is exactly `types: [checks_requested]`, the one type of `merge_group`.
fn checks_requested_only(node: &Node) -> bool {
    match entries(node) {
        [(key, types)] => {
            key.as_str() == Some("types") && names(Some(types)) == ["checks_requested"]
        }
        _ => false,
    }
}

/// The problems of the triggers `on` of the required workflow `file`.
fn trigger_problems(file: &str, on: Option<&Node>) -> Vec<String> {
    let Some(on) = on else {
        return vec![format!("{file}: no `on`")];
    };
    let mut problems = Vec::new();
    let merge_group = match &on.value {
        Value::Map(_) => entry(on, "merge_group").map(|v| is_bare(v) || checks_requested_only(v)),
        _ => unfiltered(on, "merge_group").then_some(true),
    };
    match merge_group {
        None => problems.push(format!(
            "{file}: no merge_group trigger, so the merge queue waits for its required checks until it gives up"
        )),
        Some(false) => problems.push(format!(
            "{file}: merge_group must have no filter (or `types: [checks_requested]`)"
        )),
        Some(true) => {}
    }
    if !unfiltered(on, "pull_request") {
        problems.push(format!(
            "{file}: pull_request must be a trigger with no filter: a pull request it skips never reports the required checks and cannot be queued"
        ));
    }
    problems
}

/// The problems of the jobs of the required workflow `file` (`doc`), whose required checks are
/// `required`.
fn job_problems(file: &str, doc: &Node, required: &[&str]) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut listed: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    listed.sort_unstable();
    let mut wanted = required.to_vec();
    wanted.sort_unstable();
    let mut problems = Vec::new();
    if listed != wanted {
        problems.push(format!(
            "{file}: jobs are {listed:?}, not the required checks of the merge queue {wanted:?}"
        ));
    }
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        let mut refuse = |what: &str| problems.push(format!("{file}: job {name}: {what}"));
        if entry(job, "if").is_some() {
            refuse("has an `if:`, and a required check that is skipped counts as passed");
        }
        if entry(job, "strategy").is_some() {
            refuse("has a `strategy`, whose matrix renames the check the queue waits for");
        }
        if entry(job, "uses").is_some() {
            refuse("calls a reusable workflow, whose checks are named after its jobs");
        }
        if entry(job, "name").is_some_and(|n| n.as_str() != Some(name)) {
            refuse("has a `name:` other than its id, which renames the check the queue waits for");
        }
        if !entry(job, "runs-on")
            .and_then(Node::as_str)
            .is_some_and(|l| !l.contains("${{") && linux_x86_64(l))
        {
            refuse("must run on one named GitHub-hosted Linux x86_64 runner (ubuntu-*, not -arm)");
        }
        let mut scopes = Vec::new();
        if let Some(permissions) = entry(job, "permissions") {
            scalars(permissions, &mut scopes);
        }
        if scopes.iter().any(|s| s.contains("write")) {
            refuse("holds a write scope, and nothing is published from a merge queue's run");
        }
    }
    problems
}

/// The problems of the top-level concurrency of the required workflow `file` (`doc`).
fn concurrency_problems(file: &str, doc: &Node) -> Vec<String> {
    let Some(concurrency) = entry(doc, "concurrency") else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    let group = entry(concurrency, "group").and_then(Node::as_str);
    if !group.is_some_and(|g| g.ends_with(GROUP_TAIL)) {
        problems.push(format!(
            "{file}: concurrency group {group:?} must end in {GROUP_TAIL}, a group of its own for each merge queue run"
        ));
    }
    let cancel = match entry(concurrency, "cancel-in-progress") {
        None => Some("false"),
        Some(c) => c.as_str(),
    };
    if !cancel.is_some_and(|c| CANCEL.contains(&c)) {
        problems.push(format!(
            "{file}: concurrency cancel-in-progress {cancel:?} must be one of {CANCEL:?}, which never cancel a merge queue run"
        ));
    }
    problems
}
