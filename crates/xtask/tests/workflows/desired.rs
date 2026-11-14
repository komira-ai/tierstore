//! `hostkeeper-desired.yml`: the run that sets the desired version of tierstore on the hostkeeper
//! coordinator for a published release, by its targets, after each release run that published one
//! and by hand, once `cargo xtask release desired` has found the release's `.deb`s in the testing
//! suite with the digests of its `SHA256SUMS`, and that waits for the fleet to converge on it.
//!
//! [`desired_problems`] refuses the file when:
//! - anything but `workflow_run` and `workflow_dispatch` starts it; the `workflow_run` is not the
//!   completion of `release` on `main`; or the dispatch's inputs are not the one required string
//!   `version`;
//! - its concurrency is not [`GROUP`] with `cancel-in-progress: false` (a run by hand is never
//!   replaced by the completion of a release run while it waits, nor any run cancelled);
//! - its jobs are not `check` and `set-desired`; `check`'s `if:` is not [`DESIRED_IF`] (the
//!   repository variable is the gate, at the job level, only `main` runs it, and a release run is
//!   acted on only when it succeeded, on `main` of this repository) or the `if:` of `set-desired`
//!   is not [`SET_IF`] (the same gate, and `check` said `set=true`);
//! - `check` needs a job, holds other than `actions: read` and `contents: read`, does not run
//!   [`check_command`] as its step `desired` with `VERSION` and `RUN_ID` read from the input and
//!   from the run that started it, or does not hand on that step's three outputs ([`OUTPUTS`]).
//!   The command gives `--min-hostkeeper` [`APT_FLOOR`], the floor the workflow lint holds the lab
//!   stage of `release.yml` to: the same release set by both gets the same targets, and the
//!   coordinator refuses a version set again with other targets (`release_exists`);
//! - `set-desired` needs anything but `check`, is not hostkeeper's reusable workflow
//!   ([`SET_DESIRED`]) pinned to [`CLIENT_COMMIT`] with the comment `# v`[`CLIENT_RELEASE`],
//!   holds other than `contents: read` and `id-token: write`, or its inputs are not exactly those
//!   [`with_wanted`] gives, no more: the service, the version and targets `check` handed on (no
//!   release index, no package digest), the hostkeeper release the pin names, the coordinator's
//!   two repository variables, the audience, `wait: true` with `max_polls` [`MAX_POLLS`], and the
//!   tailnet's inputs from their repository variables. The wait is what makes a set that orders
//!   no host red: such a version never converges, and the client ends the job `not_converged`;
//!   without it the job ends green, `not_waited`, whatever the hosts do.
//!
//! [`coordinator_problems`] refuses a workflow that reads a coordinator setting other than
//! hostkeeper's two variables ([`COORDINATOR_VARS`]) and the tailnet's name, or that names
//! `deploy-farm`: the version is set through hostkeeper's coordinator and nothing else, and the
//! job that set it through the farm coordinator before hostkeeper does not come back.

use super::farm::APT_FLOOR;
use super::hostkeeper::{at, steps};
use super::lint::{entries, entry, names};
use super::yaml::{self, Node};

/// The workflow's file.
pub const FILE: &str = "hostkeeper-desired.yml";

/// The `if:` of `check`: a run of `main` while the repository variable is `true`, by hand, or
/// after a run of `release.yml` that succeeded on `main` of this repository.
pub const DESIRED_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'workflow_run' && github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository)) }}";

/// The concurrency group: one for the runs by hand, one for the runs after a release.
pub const GROUP: &str =
    "hostkeeper-desired-${{ github.event_name == 'workflow_dispatch' && 'dispatch' || 'release' }}";

/// The `if:` of `set-desired`: the gate again, and `check` chose a release.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// The command of the step `desired` of `check`, with the lab's hostkeeper floor.
pub fn check_command() -> String {
    format!(
        "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --after-run \"$RUN_ID\" --min-hostkeeper {APT_FLOOR} --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\""
    )
}

/// The outputs `check` hands on, each its step `desired`'s.
pub const OUTPUTS: [&str; 3] = ["set", "package-version", "targets"];

/// hostkeeper's reusable workflow that sets the desired version (the CI client).
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The revision of the CI client the workflow pins, which takes `targets`, joins the tailnet and
/// waits for the fleet; its hkctl sends the request file's targets as they are, so an `apt` entry
/// reaches the coordinator whole. Moving it moves the `job_workflow_ref` of the coordinator's
/// allow-list entry, so it is a reviewed change here.
pub const CLIENT_COMMIT: &str = "30141591c292d8881ce5bc16c9d0b3ba7fcfdca1";

/// The hostkeeper release of [`CLIENT_COMMIT`], whose hkctl makes the call.
pub const CLIENT_RELEASE: &str = "0.1.317";

/// The host group the version is set in and waited on: the six Linux build nodes. Naming it makes
/// the call a deployment to one group (hostkeeper's hostgroups design, section 2.1), not to each
/// group that lists tierstore.
pub const HOST_GROUP: &str = "all-in-one";

/// The audience of the job's OIDC token, which the coordinator's allow-list entry names.
pub const AUDIENCE: &str = "tierstore-hostkeeper-desired";

/// How long the wait is held, in tens of seconds: the most the pinned client takes (half an hour).
pub const MAX_POLLS: &str = "180";

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
    let concurrency = entry(&doc, "concurrency");
    for (key, want) in [("group", GROUP), ("cancel-in-progress", "false")] {
        let got = concurrency.and_then(|c| at(c, &[key]));
        if got != Some(want) {
            problems.push(format!("concurrency: {key} is {got:?}, not {want}"));
        }
    }
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

/// One line per way the job `check` is not the read-only check that hands on the release.
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
    let command = check_command();
    if desired.and_then(|s| at(s, &["run"])).map(str::trim_end) != Some(command.as_str()) {
        problems.push(format!(
            "check: the step `desired` does not run `{command}`"
        ));
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
    problems
}

/// What each input of hostkeeper's workflow must be, and no other input is given.
pub fn with_wanted() -> [(&'static str, &'static str); 16] {
    [
        ("service", "tierstore"),
        ("host_group", HOST_GROUP),
        ("version", "${{ needs.check.outputs.package-version }}"),
        ("targets", "${{ needs.check.outputs.targets }}"),
        ("hostkeeper_version", CLIENT_RELEASE),
        ("hostkeeper_commit", CLIENT_COMMIT),
        ("coordinator_url", "${{ vars.HOSTKEEPER_COORDINATOR_URL }}"),
        ("coordinator_ca", "${{ vars.HOSTKEEPER_COORDINATOR_CA }}"),
        ("audience", AUDIENCE),
        ("wait", "true"),
        ("max_polls", MAX_POLLS),
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
    // No other input: not `package_digest`, which the client refuses beside the targets, and not
    // `release_index` or `release_index_digest`, which would have to list archive entries the
    // targets do not have.
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
