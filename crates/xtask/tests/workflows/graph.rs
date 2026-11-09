//! The `needs` graph of `release.yml` and the rule it is held to: GitHub skips a job when any job
//! in its `needs` chain, transitively, was skipped, unless the `if:` of the job holds a status
//! function (`always()`, `!cancelled()`, `failure()`, `success()`). A plain `if:` is given an
//! implicit `success()` over the whole chain, so a job behind a job that is skipped on purpose
//! (the farm when no hostkeeper is set, an attempt that was not needed) is skipped as well, and
//! the release stops at the promotion without a failure to show for it.
//!
//! - A job is *gated* when its `if:` says more than the run is a release on `main`: a test of an
//!   output (`needs.promote-check.outputs.promote == 'true'`) or of a result. The `plan` job is
//!   the root and is left out: when it is skipped, so is everything.
//! - A job with a gated job anywhere in its `needs` chain has a status function in its `if:`.
//! - A job that waits for an attempt gated on `outputs.conflict == 'true'` names that attempt in
//!   its `if:`, so that with a status function it still does not run when the attempt did not.

use std::collections::{BTreeMap, BTreeSet};

use super::lint::{entries, entry, names};
use super::yaml::Node;

/// The words that make an `if:` a status check.
const STATUS: [&str; 4] = ["always()", "cancelled()", "failure()", "success()"];

/// What every job after the plan says first: a release, on `main`.
const RELEASE_CONJUNCTS: [&str; 2] = [
    "needs.plan.outputs.release == 'true'",
    "github.ref == 'refs/heads/main'",
];

/// The part of a job the rules read.
#[derive(Clone, Debug)]
pub struct Info {
    pub needs: Vec<String>,
    /// The `if:` without `${{ }}`; empty for a job with none.
    pub condition: String,
}

impl Info {
    /// Does the `if:` hold a status function?
    pub fn has_status(&self) -> bool {
        STATUS.iter().any(|s| self.condition.contains(s))
    }

    /// What the `if:` says beyond a status function and the release test: empty for a job that
    /// is skipped only when a job it waits for is.
    pub fn gate(&self) -> String {
        let mut rest = self.condition.clone();
        for word in STATUS.iter().chain(&RELEASE_CONJUNCTS) {
            rest = rest.replace(word, "");
        }
        rest.retain(|c| !c.is_whitespace() && c != '!' && c != '&');
        rest
    }
}

/// The jobs of a workflow, by name.
pub fn jobs(doc: &Node) -> BTreeMap<String, Info> {
    let mut found = BTreeMap::new();
    for (name, job) in entry(doc, "jobs").map_or(&[][..], entries) {
        let condition = entry(job, "if")
            .and_then(Node::as_str)
            .unwrap_or_default()
            .trim()
            .trim_start_matches("${{")
            .trim_end_matches("}}")
            .trim()
            .to_owned();
        found.insert(
            name.as_str().unwrap_or_default().to_owned(),
            Info {
                needs: names(entry(job, "needs")),
                condition,
            },
        );
    }
    found
}

/// Every job in the `needs` chain of `job`, `plan` aside, whatever the depth.
pub fn ancestors(jobs: &BTreeMap<String, Info>, job: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&str> = jobs
        .get(job)
        .map_or_else(Vec::new, |j| j.needs.iter().map(String::as_str).collect());
    while let Some(next) = todo.pop() {
        if next != "plan" && seen.insert(next.to_owned()) {
            todo.extend(
                jobs.get(next)
                    .into_iter()
                    .flat_map(|j| j.needs.iter().map(String::as_str)),
            );
        }
    }
    seen
}

/// One line per job of `doc` that breaks the rules above.
pub fn graph_problems(doc: &Node) -> Vec<String> {
    let jobs = jobs(doc);
    let mut problems = Vec::new();
    for (name, info) in &jobs {
        let above = ancestors(&jobs, name);
        let gated: Vec<&String> = above
            .iter()
            .filter(|a| jobs.get(*a).is_some_and(|j| !j.gate().is_empty()))
            .collect();
        if !gated.is_empty() && !info.has_status() {
            problems.push(format!(
                "{name}: `if` has no status function (`!cancelled() && !failure() && ...`) and \
                 the jobs it waits for include {gated:?}, which may be skipped: GitHub skips \
                 {name} with them"
            ));
        }
        for need in &info.needs {
            let attempt = jobs
                .get(need)
                .is_some_and(|j| j.condition.contains(".outputs.conflict == 'true'"));
            if attempt && info.has_status() && !info.condition.contains(&format!("needs.{need}.")) {
                problems.push(format!(
                    "{name}: `if` does not test needs.{need}, an attempt that may not have run, \
                     and a status function lets {name} run without it"
                ));
            }
        }
    }
    problems
}
