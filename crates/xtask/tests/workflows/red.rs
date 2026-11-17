//! The `red` job of `release.yml` and of `beta.yml`, and `beta.yml` whole: what runs after a
//! release is deployed or beside it, and reports a failure as the issue `RELEASE-RED: tierstore
//! <stage>` (`cargo xtask release red`) instead of holding the deploy.
//!
//! [`red_job_problems`] refuses a workflow whose job `red`:
//! - is missing, or has another `if:` than the one given (it must run whatever the jobs before it
//!   did: a status function, and `main` alone);
//! - holds other than `actions: read`, `contents: read` and `issues: write`;
//! - does not run the verb with the arguments given as one of its steps, with the environment
//!   given on that step;
//! - does not wait for every other job of the workflow (its `needs`, transitively), or is waited
//!   for by one: it reads the conclusions of all the other jobs of its run, so it must be last.
//!
//! [`beta_problems`] refuses `beta.yml` when anything but a push to `main` and a dispatch starts
//! it; its concurrency is not one group per commit ([`BETA_GROUP`]) that never cancels; its jobs
//! are not `linux-arm64` (on `ubuntu-24.04-arm`), `macos-arm64` (on `macos-15`) and `red`; a test
//! job needs a job, has an `if:`, lets a step fail (`continue-on-error`) or does not run every
//! test of the workspace ([`BETA_TESTS`]); or its `red` job breaks the rules above or is in a
//! concurrency group (a group keeps one waiting job and drops the one before it, so the report of
//! a failed commit would be lost).

use super::graph::{ancestors, jobs};
use super::hostkeeper::{at, steps};
use super::lint::{entries, entry, names};
use super::platforms::BETA_TESTS;
use super::yaml::{self, Node};

/// The `if:` of release.yml's `red`: whatever the jobs before it did, on `main`, when the plan
/// failed or a release was made (a run that planned nothing has nothing to report).
pub const RELEASE_RED_IF: &str = "${{ !cancelled() && github.ref == 'refs/heads/main' && (needs.plan.result == 'failure' || needs.plan.outputs.release == 'true') }}";

/// The `if:` of beta.yml's `red`: whatever the test jobs did, on `main` (a run by hand of a
/// branch never opens or closes the issue).
pub const BETA_RED_IF: &str = "${{ !cancelled() && github.ref == 'refs/heads/main' }}";

/// The command of release.yml's `red`.
pub const RELEASE_RED_COMMAND: &str = "cargo xtask release red --repo \"$GITHUB_REPOSITORY\" --stage release --run \"$GITHUB_RUN_ID\" --job red --commit \"$COMMIT\" --complete \"$COMPLETE\"";

/// The environment of the step of release.yml's `red`: the commit released (the tip, when the
/// plan failed before naming one), and whether the run released, so that only a run that did
/// closes the issue.
pub const RELEASE_RED_ENV: [(&str, &str); 2] = [
    ("COMMIT", "${{ needs.plan.outputs.commit || github.sha }}"),
    ("COMPLETE", "${{ needs.plan.outputs.release == 'true' }}"),
];

/// The command of beta.yml's `red`: every run of main is complete.
pub const BETA_RED_COMMAND: &str = "cargo xtask release red --repo \"$GITHUB_REPOSITORY\" --stage beta --run \"$GITHUB_RUN_ID\" --job red --commit \"$GITHUB_SHA\" --complete true";

/// The concurrency group of beta.yml: one per commit.
pub const BETA_GROUP: &str = "beta-${{ github.sha }}";

/// The test jobs of beta.yml and their runners.
pub const BETA_JOBS: [(&str, &str); 2] = [
    ("linux-arm64", "ubuntu-24.04-arm"),
    ("macos-arm64", "macos-15"),
];

/// The job `name` of `doc`.
fn job<'a>(doc: &'a Node, name: &str) -> Option<&'a Node> {
    entry(doc, "jobs")
        .map_or(&[][..], entries)
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// The permissions of `job`, as `scope: level`, sorted.
fn held(job: Option<&Node>) -> Vec<String> {
    let mut held: Vec<String> = job
        .and_then(|j| entry(j, "permissions"))
        .map_or(&[][..], entries)
        .iter()
        .map(|(k, v)| {
            format!(
                "{}: {}",
                k.as_str().unwrap_or_default(),
                v.as_str().unwrap_or_default()
            )
        })
        .collect();
    held.sort();
    held
}

/// One line per way the job `red` of `file` (`doc`) is not the last job that reports the run,
/// under `condition`, running `command` with `env` on its step.
pub fn red_job_problems(
    file: &str,
    doc: &Node,
    condition: &str,
    command: &str,
    env: &[(&str, &str)],
) -> Vec<String> {
    let Some(red) = job(doc, "red") else {
        return vec![format!("{file}: there is no job red")];
    };
    let mut problems = Vec::new();
    let got = at(red, &["if"]);
    if got != Some(condition) {
        problems.push(format!("{file}: red: `if` is {got:?}, not {condition}"));
    }
    let held = held(Some(red));
    if held != ["actions: read", "contents: read", "issues: write"] {
        problems.push(format!(
            "{file}: red: permissions are {held:?}, not actions: read, contents: read and issues: write"
        ));
    }
    let all = steps(Some(red));
    let step = all
        .iter()
        .find(|s| at(s, &["run"]).map(str::trim_end) == Some(command));
    match step {
        None => problems.push(format!("{file}: red: no step runs `{command}`")),
        Some(step) => {
            for (var, want) in env {
                if at(step, &["env", var]) != Some(*want) {
                    problems.push(format!(
                        "{file}: red: the step that reports does not read {var} from {want}"
                    ));
                }
            }
        }
    }
    let graph = jobs(doc);
    let mut before = ancestors(&graph, "red");
    before.extend(
        graph
            .get("red")
            .map_or(&[][..], |j| &j.needs[..])
            .iter()
            .cloned(),
    );
    for (name, info) in &graph {
        if name == "red" {
            continue;
        }
        if !before.contains(name) {
            problems.push(format!(
                "{file}: red does not wait for {name}: it reads the conclusion of every job of the run"
            ));
        }
        if info.needs.iter().any(|n| n == "red") {
            problems.push(format!("{file}: {name} needs red, which must be last"));
        }
    }
    problems
}

/// One line per way the job `red` of release.yml (`doc`) is not the last job reporting the
/// release.
pub fn release_red_problems(doc: &Node) -> Vec<String> {
    red_job_problems(
        "release.yml",
        doc,
        RELEASE_RED_IF,
        RELEASE_RED_COMMAND,
        &RELEASE_RED_ENV,
    )
}

/// One line per way `beta.yml` (`text`) is not the beta stage described above.
pub fn beta_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = Vec::new();
    let on = entry(&doc, "on");
    let triggers = names(on);
    if triggers != ["push", "workflow_dispatch"] {
        problems.push(format!(
            "beta.yml: triggers are {triggers:?}, not push and workflow_dispatch"
        ));
    }
    let branches = names(
        on.and_then(|o| entry(o, "push"))
            .and_then(|p| entry(p, "branches")),
    );
    if branches != ["main"] {
        problems.push(format!(
            "beta.yml: push.branches is {branches:?}, not [\"main\"]"
        ));
    }
    for (key, want) in [("group", BETA_GROUP), ("cancel-in-progress", "false")] {
        let got = entry(&doc, "concurrency").and_then(|c| at(c, &[key]));
        if got != Some(want) {
            problems.push(format!(
                "beta.yml: concurrency: {key} is {got:?}, not {want}"
            ));
        }
    }
    let mut listed: Vec<&str> = entry(&doc, "jobs")
        .map_or(&[][..], entries)
        .iter()
        .filter_map(|(k, _)| k.as_str())
        .collect();
    listed.sort_unstable();
    if listed != ["linux-arm64", "macos-arm64", "red"] {
        problems.push(format!(
            "beta.yml: jobs are {listed:?}, not linux-arm64, macos-arm64 and red"
        ));
    }
    for (name, runner) in BETA_JOBS {
        let Some(test) = job(&doc, name) else {
            continue;
        };
        let got = at(test, &["runs-on"]);
        if got != Some(runner) {
            problems.push(format!(
                "beta.yml: {name}: runs-on is {got:?}, not {runner}"
            ));
        }
        let needs = names(entry(test, "needs"));
        if !needs.is_empty() {
            problems.push(format!(
                "beta.yml: {name}: needs {needs:?}; it needs no job"
            ));
        }
        if entry(test, "if").is_some() {
            problems.push(format!(
                "beta.yml: {name}: has an `if`; the tests run on every push to main"
            ));
        }
        let all = steps(Some(test));
        if !all
            .iter()
            .any(|s| at(s, &["run"]).map(str::trim_end) == Some(BETA_TESTS))
        {
            problems.push(format!("beta.yml: {name}: no step runs `{BETA_TESTS}`"));
        }
        if entry(test, "continue-on-error").is_some()
            || all.iter().any(|s| entry(s, "continue-on-error").is_some())
        {
            problems.push(format!(
                "beta.yml: {name}: `continue-on-error` would hide a failed test from red"
            ));
        }
    }
    problems.extend(red_job_problems(
        "beta.yml",
        &doc,
        BETA_RED_IF,
        BETA_RED_COMMAND,
        &[],
    ));
    if job(&doc, "red")
        .and_then(|r| entry(r, "concurrency"))
        .is_some()
    {
        problems.push(
            "beta.yml: red: has a concurrency group, which drops a waiting report".to_owned(),
        );
    }
    problems
}
