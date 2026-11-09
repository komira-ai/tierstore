//! `hostkeeper-desired.yml`: the run that sets the desired version of tierstore on the hostkeeper
//! coordinator for a published release, by its targets, after each release run that published one
//! and by hand, once `cargo xtask release desired` has refused a release whose index lacks a
//! platform and the release channel serves its index.
//!
//! [`desired_problems`] refuses the file when:
//! - anything but `workflow_run` and `workflow_dispatch` starts it; the `workflow_run` is not the
//!   completion of `release` on `main`; or the dispatch's inputs are not the one required string
//!   `version`;
//! - its jobs are not `check` and `set-desired`; `check`'s `if:` is not [`DESIRED_IF`] (the
//!   repository variable is the gate, at the job level, only `main` runs it, and a release run is
//!   acted on only when it succeeded, on `main` of this repository) or `set-desired`'s is not
//!   [`SET_IF`] (the same gate, and `check` said `set=true`);
//! - `check` needs a job, holds other than `actions: read` and `contents: read`, does not run
//!   [`CHECK`] as its step `desired` with `VERSION` and `RUN_ID` read from the input and from the
//!   run that started it, does not hand on that step's five outputs, or does not wait for the
//!   release channel ([`CHANNEL_WAIT`], then [`CHANNEL_SAME`], each under [`SET_STEP_IF`]);
//! - `set-desired` needs anything but `check`, is not hostkeeper's reusable workflow
//!   ([`SET_DESIRED`]) pinned to [`CLIENT_COMMIT`] with the comment `# v`[`CLIENT_RELEASE`],
//!   holds other than `contents: read` and `id-token: write`, or its inputs are not exactly those
//!   [`with_wanted`] gives, no more: the service, what `check` handed on, the hostkeeper release the pin
//!   names, the coordinator's two repository variables, the audience, `wait: false`, and the
//!   tailnet's inputs from their repository variables.
//!
//! [`coordinator_problems`] refuses a workflow that reads a coordinator setting other than
//! hostkeeper's two variables ([`COORDINATOR_VARS`]) and the tailnet's name, or that names
//! `deploy-farm`: the version is set through hostkeeper's coordinator and nothing else, and the
//! job that set it through the farm coordinator before hostkeeper does not come back.

use super::hostkeeper::{at, steps};
use super::lint::{entries, entry, names};
use super::yaml::{self, Node};

/// The workflow's file.
pub const FILE: &str = "hostkeeper-desired.yml";

/// The `if:` of `check`: a run of `main` while the repository variable is `true`, by hand, or
/// after a run of `release.yml` that succeeded on `main` of this repository.
pub const DESIRED_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'workflow_run' && github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository)) }}";

/// The `if:` of `set-desired`: the gate again, and `check` chose a release.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// The `if:` of the steps of `check` after `desired`.
pub const SET_STEP_IF: &str = "${{ steps.desired.outputs.set == 'true' }}";

/// The command of the step `desired` of `check`.
pub const CHECK: &str = "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --after-run \"$RUN_ID\" --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\"";

/// The wait of `check` for the release channel to serve the release's index.
pub const CHANNEL_WAIT: &str = "curl --proto =https --proto-redir =https --tlsv1.2 --fail --silent --show-error --max-time 30 --retry 45 --retry-delay 40 --retry-max-time 1800 --retry-all-errors --output \"$RUNNER_TEMP/served-index.json\" \"$RELEASE_CHANNEL/tierstore/$PACKAGE_VERSION/index.json\"";

/// The comparison of the served index with the checked one.
pub const CHANNEL_SAME: &str =
    "cmp \"$RUNNER_TEMP/desired/index.json\" \"$RUNNER_TEMP/served-index.json\"";

/// The outputs `check` hands on, each its step `desired`'s.
pub const OUTPUTS: [&str; 5] = [
    "set",
    "package-version",
    "release-index",
    "release-index-digest",
    "targets",
];

/// hostkeeper's reusable workflow that sets the desired version (the CI client).
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The revision of the CI client the workflow pins: the one hostkeeper's own `release.yml` and
/// `hello-desired.yml` pin, which takes `targets` and joins the tailnet. Moving it moves the
/// `job_workflow_ref` of the coordinator's allow-list entry, so it is a reviewed change here.
pub const CLIENT_COMMIT: &str = "2c767d1842a88edb7120073d2dff3b4bfccc834c";

/// The hostkeeper release of [`CLIENT_COMMIT`], whose hkctl makes the call.
pub const CLIENT_RELEASE: &str = "0.1.184";

/// The audience of the job's OIDC token, which the coordinator's allow-list entry names.
pub const AUDIENCE: &str = "tierstore-hostkeeper-desired";

/// The repository variables of the coordinator: its address and its CA.
pub const COORDINATOR_VARS: [&str; 2] = ["HOSTKEEPER_COORDINATOR_URL", "HOSTKEEPER_COORDINATOR_CA"];

/// The repository variable of the coordinator's name on the tailnet, which the join pings.
pub const TAILNET_COORDINATOR: &str = "HOSTKEEPER_TAILNET_COORDINATOR";

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

/// One line per way the triggers of the workflow (its `on`) are not the completion of `release`
/// on `main` and a dispatch with the one input `version`.
fn trigger_problems(on: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let triggers = names(on);
    if triggers != ["workflow_dispatch", "workflow_run"] {
        problems.push(format!(
            "triggers are {triggers:?}, not workflow_dispatch and workflow_run"
        ));
    }
    let run = on.and_then(|o| entry(o, "workflow_run"));
    for (key, want) in [
        ("workflows", "release"),
        ("types", "completed"),
        ("branches", "main"),
    ] {
        let got = names(run.and_then(|r| entry(r, key)));
        if got != [want] {
            problems.push(format!("workflow_run.{key} is {got:?}, not [{want:?}]"));
        }
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
    problems
}

/// One line per way `hostkeeper-desired.yml` (`text`) is not the run described above.
pub fn desired_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = trigger_problems(entry(&doc, "on"));
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let mut found: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    found.sort_unstable();
    if found != ["check", "set-desired"] {
        problems.push(format!("jobs are {found:?}, not check and set-desired"));
    }
    for (name, want) in [("check", DESIRED_IF), ("set-desired", SET_IF)] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is {condition:?}, not {want}"));
        }
    }
    problems.extend(check_problems(job(jobs, "check")));
    problems.extend(set_desired_problems(text, job(jobs, "set-desired")));
    problems
}

/// The step of `job` whose `run` is `command`.
fn step_running<'a>(job: Option<&'a Node>, command: &str) -> Option<&'a Node> {
    steps(job)
        .iter()
        .find(|s| at(s, &["run"]).map(str::trim_end) == Some(command))
}

/// One line per way the job `check` is not the read-only check that hands on the release and
/// waits for the release channel.
fn check_problems(check: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(check.and_then(|j| entry(j, "needs")));
    if !needs.is_empty() {
        problems.push(format!("check: needs {needs:?}; it needs no job"));
    }
    let held = held(check);
    if held != ["actions: read", "contents: read"] {
        problems.push(format!(
            "check: permissions are {held:?}, not actions: read and contents: read"
        ));
    }
    let all = steps(check);
    let step = all.iter().position(|s| at(s, &["id"]) == Some("desired"));
    let desired = step.map(|i| &all[i]);
    if desired.and_then(|s| at(s, &["run"])).map(str::trim_end) != Some(CHECK) {
        problems.push(format!("check: the step `desired` does not run `{CHECK}`"));
    }
    for (var, want) in [
        ("VERSION", "${{ inputs.version }}"),
        ("RUN_ID", "${{ github.event.workflow_run.id }}"),
    ] {
        if desired.and_then(|s| at(s, &["env", var])) != Some(want) {
            problems.push(format!(
                "check: the step `desired` does not read {var} from {want}"
            ));
        }
    }
    for output in OUTPUTS {
        let got = check.and_then(|j| at(j, &["outputs", output]));
        let want = format!("${{{{ steps.desired.outputs.{output} }}}}");
        if got != Some(want.as_str()) {
            problems.push(format!("check: output {output} is {got:?}, not {want}"));
        }
    }
    let mut after = step.unwrap_or(all.len());
    for command in [CHANNEL_WAIT, CHANNEL_SAME] {
        let found = all
            .iter()
            .position(|s| at(s, &["run"]).map(str::trim_end) == Some(command));
        match found {
            Some(i) if i > after => after = i,
            _ => problems.push(format!(
                "check: no step after the ones before it runs `{command}`"
            )),
        }
        let condition = step_running(check, command).and_then(|s| at(s, &["if"]));
        if condition != Some(SET_STEP_IF) {
            problems.push(format!(
                "check: the step that runs `{command}` has `if` {condition:?}, not {SET_STEP_IF}"
            ));
        }
    }
    let wait = step_running(check, CHANNEL_WAIT);
    for (var, want) in [
        ("RELEASE_CHANNEL", "${{ vars.HOSTKEEPER_RELEASE_CHANNEL }}"),
        (
            "PACKAGE_VERSION",
            "${{ steps.desired.outputs.package-version }}",
        ),
    ] {
        if wait.and_then(|s| at(s, &["env", var])) != Some(want) {
            problems.push(format!(
                "check: the wait for the channel does not read {var} from {want}"
            ));
        }
    }
    problems
}

/// What each input of hostkeeper's workflow must be, and no other input is given.
pub fn with_wanted() -> [(&'static str, &'static str); 16] {
    [
        ("service", "tierstore"),
        ("version", "${{ needs.check.outputs.package-version }}"),
        ("release_index", "${{ needs.check.outputs.release-index }}"),
        (
            "release_index_digest",
            "${{ needs.check.outputs.release-index-digest }}",
        ),
        ("targets", "${{ needs.check.outputs.targets }}"),
        ("hostkeeper_version", CLIENT_RELEASE),
        ("hostkeeper_commit", CLIENT_COMMIT),
        ("coordinator_url", "${{ vars.HOSTKEEPER_COORDINATOR_URL }}"),
        ("coordinator_ca", "${{ vars.HOSTKEEPER_COORDINATOR_CA }}"),
        ("audience", AUDIENCE),
        ("wait", "false"),
        (
            "join_tailnet",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}",
        ),
        (
            "tailnet_client_id",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        ),
        (
            "tailnet_audience",
            "${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        ),
        (
            "tailnet_coordinator",
            "${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}",
        ),
        ("tailnet_tag", "${{ vars.HOSTKEEPER_TAILNET_TAG }}"),
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
    let want = format!("{SET_DESIRED}@{CLIENT_COMMIT}");
    if uses.and_then(Node::as_str) != Some(want.as_str()) {
        problems.push(format!("set-desired: `uses` is not {want}"));
    }
    // The version comment of the pin, on the line of `uses:`; the general lint holds its form.
    let release = uses
        .and_then(|u| text.lines().nth(u.line() - 1))
        .and_then(|line| line.split_once(" # v"))
        .map(|(_, version)| version.trim_end());
    if release != Some(CLIENT_RELEASE) {
        problems.push(format!(
            "set-desired: the pin's comment names {release:?}, not v{CLIENT_RELEASE}"
        ));
    }
    let wanted = with_wanted();
    for (key, want) in wanted {
        let got = job.and_then(|j| at(j, &["with", key]));
        if got != Some(want) {
            problems.push(format!("set-desired: with.{key} is {got:?}, not {want:?}"));
        }
    }
    // No other input: not `package_digest`, which the client refuses beside the index or the
    // targets, and not `max_polls`, which only a wait reads.
    let given = job.and_then(|j| entry(j, "with")).map_or(&[][..], entries);
    for (key, _) in given {
        let key = key.as_str().unwrap_or_default();
        if !wanted.iter().any(|(k, _)| *k == key) {
            problems.push(format!("set-desired: with.{key} is not an input it gives"));
        }
    }
    problems
}

/// One line per place in the workflow `file` (`text`) that reads a coordinator variable other
/// than [`COORDINATOR_VARS`] and [`TAILNET_COORDINATOR`] or names [`DEPLOY_FARM`].
pub fn coordinator_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for read in line.split("vars.").skip(1) {
            let name: String = read
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.contains("COORDINATOR")
                && !COORDINATOR_VARS.contains(&name.as_str())
                && name != TAILNET_COORDINATOR
            {
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
/// calls it; it starts on the completion of `release.yml` by its own trigger.
pub fn caller_problems(files: &[(String, String)]) -> Vec<String> {
    files
        .iter()
        .filter(|(name, text)| name != FILE && text.contains(FILE))
        .map(|(name, _)| format!("{name}: names {FILE}, which no other workflow calls"))
        .collect()
}
