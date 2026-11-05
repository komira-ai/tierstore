//! `hostkeeper-desired.yml`: the run by hand that sets the desired version of tierstore on the
//! hostkeeper coordinator for a published release, after `cargo xtask release desired` has
//! refused a release whose index lacks a platform.
//!
//! [`desired_problems`] refuses the file when:
//! - anything but `workflow_dispatch` starts it (so release.yml, or a run of it, cannot), or its
//!   inputs are not the one required string `version`;
//! - its jobs are not `check` and `set-desired`, or either job's `if:` is not [`DESIRED_IF`]: the
//!   repository variable is the gate, at the job level, and only `main` runs it;
//! - `check` needs a job, holds more than `contents: read`, does not run [`CHECK`] as its step
//!   `desired` with `VERSION` read from the input, or does not hand on that step's two outputs;
//! - `set-desired` needs anything but `check` (so it waits for nothing of release.yml), is not
//!   hostkeeper's reusable workflow ([`SET_DESIRED`]) pinned to a commit, holds other than
//!   `contents: read` and `id-token: write`, or one of its inputs is not what [`with_wanted`]
//!   gives: the service, the version and digest `check` handed on, the hostkeeper release the pin
//!   names (its commit, and the version of its `# v<version>` comment), the coordinator's two
//!   repository variables, the audience, and `wait: true`, so the job waits for the fleet.
//!
//! [`coordinator_problems`] refuses a workflow that reads a coordinator setting other than
//! hostkeeper's two variables ([`COORDINATOR_VARS`]), or that names `deploy-farm`: the version is
//! set through hostkeeper's coordinator and nothing else, and the job that set it through the farm
//! coordinator before hostkeeper does not come back.

use super::hostkeeper::{at, steps};
use super::lint::{entries, entry, names};
use super::yaml::{self, Node};

/// The workflow's file.
pub const FILE: &str = "hostkeeper-desired.yml";

/// The `if:` of both jobs: a run of `main`, once the coordinator's address is set.
pub const DESIRED_IF: &str =
    "${{ github.ref == 'refs/heads/main' && vars.HOSTKEEPER_COORDINATOR_URL != '' }}";

/// The command of the step `desired` of `check`.
pub const CHECK: &str = "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\"";

/// hostkeeper's reusable workflow that sets the desired version and waits for the fleet.
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The audience of the job's OIDC token, which the coordinator's allow-list entry names.
pub const AUDIENCE: &str = "tierstore-hostkeeper-desired";

/// The repository variables of the coordinator: its address and its CA.
pub const COORDINATOR_VARS: [&str; 2] = ["HOSTKEEPER_COORDINATOR_URL", "HOSTKEEPER_COORDINATOR_CA"];

/// The word of a workflow that names the farm coordinator's verb.
pub const DEPLOY_FARM: &str = "deploy-farm";

/// The job `name` of `jobs`.
fn job<'a>(jobs: &'a [(Node, Node)], name: &str) -> Option<&'a Node> {
    jobs.iter()
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

/// One line per way `hostkeeper-desired.yml` (`text`) is not the run described above.
pub fn desired_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = Vec::new();
    let on = entry(&doc, "on");
    let triggers = names(on);
    if triggers != ["workflow_dispatch"] {
        problems.push(format!(
            "triggers are {triggers:?}, not workflow_dispatch alone"
        ));
    }
    let inputs = on
        .and_then(|o| entry(o, "workflow_dispatch"))
        .and_then(|w| entry(w, "inputs"));
    let listed = names(inputs);
    if listed != ["version"] {
        problems.push(format!("inputs are {listed:?}, not version alone"));
    }
    let version = inputs.and_then(|i| entry(i, "version"));
    for (key, want) in [("type", "string"), ("required", "true")] {
        let got = version.and_then(|v| at(v, &[key]));
        if got != Some(want) {
            problems.push(format!("input version: {key} is {got:?}, not {want}"));
        }
    }
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let mut found: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    found.sort_unstable();
    if found != ["check", "set-desired"] {
        problems.push(format!("jobs are {found:?}, not check and set-desired"));
    }
    for name in ["check", "set-desired"] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(DESIRED_IF) {
            problems.push(format!("{name}: `if` is {condition:?}, not {DESIRED_IF}"));
        }
    }
    problems.extend(check_problems(job(jobs, "check")));
    problems.extend(set_desired_problems(text, job(jobs, "set-desired")));
    problems
}

/// One line per way the job `check` is not the read-only check that hands on the version and
/// the digest.
fn check_problems(check: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(check.and_then(|j| entry(j, "needs")));
    if !needs.is_empty() {
        problems.push(format!("check: needs {needs:?}; it needs no job"));
    }
    let held = held(check);
    if held != ["contents: read"] {
        problems.push(format!(
            "check: permissions are {held:?}, not contents: read alone"
        ));
    }
    let step = steps(check)
        .iter()
        .find(|s| at(s, &["id"]) == Some("desired"));
    let run = step.and_then(|s| at(s, &["run"])).map(str::trim_end);
    if run != Some(CHECK) {
        problems.push(format!("check: the step `desired` does not run `{CHECK}`"));
    }
    if step.and_then(|s| at(s, &["env", "VERSION"])) != Some("${{ inputs.version }}") {
        problems.push(
            "check: the step `desired` does not read VERSION from ${{ inputs.version }}".to_owned(),
        );
    }
    for output in ["package-version", "package-digest"] {
        let got = check.and_then(|j| at(j, &["outputs", output]));
        let want = format!("${{{{ steps.desired.outputs.{output} }}}}");
        if got != Some(want.as_str()) {
            problems.push(format!("check: output {output} is {got:?}, not {want}"));
        }
    }
    problems
}

/// What each input of hostkeeper's workflow must be, for the pin `commit` whose version comment
/// says `release`.
pub fn with_wanted<'a>(commit: &'a str, release: &'a str) -> [(&'static str, &'a str); 9] {
    [
        ("service", "tierstore"),
        ("version", "${{ needs.check.outputs.package-version }}"),
        (
            "package_digest",
            "${{ needs.check.outputs.package-digest }}",
        ),
        ("hostkeeper_version", release),
        ("hostkeeper_commit", commit),
        ("coordinator_url", "${{ vars.HOSTKEEPER_COORDINATOR_URL }}"),
        ("coordinator_ca", "${{ vars.HOSTKEEPER_COORDINATOR_CA }}"),
        ("audience", AUDIENCE),
        ("wait", "true"),
    ]
}

/// One line per way the job `set-desired` is not hostkeeper's workflow, called with the release
/// `check` handed on.
fn set_desired_problems(text: &str, job: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(job.and_then(|j| entry(j, "needs")));
    if needs != ["check"] {
        problems.push(format!("set-desired: needs are {needs:?}, not [\"check\"]"));
    }
    let held = held(job);
    if held != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "set-desired: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let uses = job.and_then(|j| entry(j, "uses"));
    let commit = uses
        .and_then(Node::as_str)
        .and_then(|u| u.strip_prefix(SET_DESIRED))
        .and_then(|rest| rest.strip_prefix('@'));
    let Some(commit) = commit else {
        problems.push(format!("set-desired: `uses` is not {SET_DESIRED}@<commit>"));
        return problems;
    };
    // The version comment of the pin, on the line of `uses:`; the general lint holds its form.
    let release = uses
        .and_then(|u| text.lines().nth(u.line() - 1))
        .and_then(|line| line.split_once(" # v"))
        .map_or("", |(_, version)| version.trim_end());
    for (key, want) in with_wanted(commit, release) {
        let got = job.and_then(|j| at(j, &["with", key]));
        if got != Some(want) {
            problems.push(format!("set-desired: with.{key} is {got:?}, not {want:?}"));
        }
    }
    problems
}

/// One line per place in the workflow `file` (`text`) that reads a coordinator variable other
/// than [`COORDINATOR_VARS`] or names [`DEPLOY_FARM`].
pub fn coordinator_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for read in line.split("vars.").skip(1) {
            let name: String = read
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.contains("COORDINATOR") && !COORDINATOR_VARS.contains(&name.as_str()) {
                problems.push(format!(
                    "{file}: line {}: reads vars.{name}, not one of hostkeeper's {COORDINATOR_VARS:?}",
                    n + 1
                ));
            }
        }
        if line.contains(DEPLOY_FARM) {
            problems.push(format!("{file}: line {}: names {DEPLOY_FARM}", n + 1));
        }
    }
    problems
}

/// One line per workflow of `files` (name, text), other than [`FILE`], that names [`FILE`]: none
/// starts or calls it.
pub fn caller_problems(files: &[(String, String)]) -> Vec<String> {
    files
        .iter()
        .filter(|(name, text)| name != FILE && text.contains(FILE))
        .map(|(name, _)| format!("{name}: names {FILE}, which only a person starts"))
        .collect()
}
