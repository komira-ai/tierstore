//! No runner but Linux x86_64 in a pull request's checks (AGENTS.md B9, this repository): the
//! rule [`pull_request_runner_problems`] holds every workflow a pull request starts to.

use super::lint::{entries, entry, names, scalars};
use super::macos_farm;
use super::platforms::{NOT_FOR_A_PULL_REQUEST, at};
use super::yaml::{self, Node};

/// Whether `label` is a GitHub-hosted Linux x86_64 runner: `ubuntu-<version>` or `ubuntu-latest`,
/// not an `-arm` image, not a larger-runner or self-hosted label.
pub fn linux_x86_64(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label.strip_prefix("ubuntu-").is_some_and(|v| {
        v == "latest" || (!v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    })
}

/// The labels of the `runs-on` of `job` that are not [`linux_x86_64`].
fn other_platform_labels(job: &Node) -> Vec<String> {
    let mut labels = Vec::new();
    if let Some(runs_on) = entry(job, "runs-on") {
        scalars(runs_on, &mut labels);
    }
    labels
        .into_iter()
        .filter(|l| !l.contains("${{") && !linux_x86_64(l))
        .map(str::to_owned)
        .collect()
}

/// One line per job of a workflow a pull request starts whose runner the lint cannot see: a
/// `runs-on` that is an expression (a matrix value, an input), or a call of a reusable workflow
/// (a job-level `uses:`), either of which could put the job on another platform unseen. The one
/// call allowed is artifacts.yml's farm job ([`macos_farm::is_the_farm_call`]): its condition is
/// never true for a pull request, and the lint holds the one job it calls to `ubuntu-24.04`.
fn unseen_runner_problems(file: &str, name: &str, job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    let mut labels = Vec::new();
    if let Some(runs_on) = entry(job, "runs-on") {
        scalars(runs_on, &mut labels);
    }
    if labels.iter().any(|l| l.contains("${{")) {
        problems.push(format!(
            "{file}: the job {name} of a pull request runs on an expression; name its runner, so the lint can tell its platform"
        ));
    }
    if entry(job, "uses").is_some() && !macos_farm::is_the_farm_call(name, job) {
        problems.push(format!(
            "{file}: the job {name} of a pull request calls a reusable workflow, whose runners the lint cannot see"
        ));
    }
    problems
}

/// Whether the job condition `condition` is one of the conditions of this repository's jobs that is
/// never true for a pull request: the beta stage's, and the three of the macOS build, hosted
/// ([`macos_farm::HOSTED_IF`]) or after it (`sign-pkg`'s, [`macos_farm::SIGN_PKG_IF`], and
/// `check-pkg`'s, [`macos_farm::CHECK_PKG_IF`]).
fn never_a_pull_request(condition: &str) -> bool {
    [
        NOT_FOR_A_PULL_REQUEST,
        macos_farm::HOSTED_IF,
        macos_farm::SIGN_PKG_IF,
        macos_farm::CHECK_PKG_IF,
    ]
    .contains(&condition)
}

/// One line per job of the workflow `file` (`text`) that a pull request runs on a platform other
/// than Linux x86_64. The workflow has a `pull_request` trigger and a job names a runner that is not
/// a GitHub-hosted `ubuntu-*` x86_64 one ([`linux_x86_64`]: macOS, Windows and `-arm` images are
/// not) under another `if:` than one never true for a pull request ([`never_a_pull_request`]).
/// The other platforms build and test as the beta stage of the release, after a merge. A job of
/// such a workflow whose runner is an expression, or that calls a reusable workflow, is refused
/// ([`unseen_runner_problems`]).
pub fn pull_request_runner_problems(file: &str, text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![format!("{file}: {e}")],
    };
    if !names(entry(&doc, "on")).iter().any(|t| t == "pull_request") {
        return Vec::new();
    }
    let mut problems = Vec::new();
    for (name, job) in entry(&doc, "jobs").map_or(&[][..], entries) {
        let name = name.as_str().unwrap_or_default();
        problems.extend(unseen_runner_problems(file, name, job));
        // A push to main is never a pull request: the jobs that sign and check the installer run
        // under it alone.
        if at(job, &["if"]).is_some_and(never_a_pull_request) {
            continue;
        }
        let others = other_platform_labels(job);
        if !others.is_empty() {
            problems.push(format!(
                "{file}: the job {name} runs on {others:?} for a pull request; a pull request runs on Linux x86_64 (ubuntu-*, not -arm) alone, and other platforms are the beta stage of the release, under `if: {NOT_FOR_A_PULL_REQUEST}`"
            ));
        }
    }
    problems
}
