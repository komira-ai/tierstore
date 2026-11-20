//! The two ways the desired version of tierstore is set on the hostkeeper coordinator, by its
//! targets, once `cargo xtask release desired` has found the release's `.deb`s in the testing
//! suite with the digests of its `SHA256SUMS`, each waiting for the fleet to converge on it: the
//! deploy of `release.yml` (its jobs `deploy-check`, `deploy` and `deploy-mac`, as soon as the
//! release is in the suite `testing`), and `hostkeeper-desired.yml`, by hand.
//!
//! [`desired_problems`] refuses `hostkeeper-desired.yml` when:
//! - anything but `workflow_dispatch` starts it, or the dispatch's inputs are not the one required
//!   string `version` (a release is deployed by release.yml; this run is a rollback by hand);
//! - its concurrency is not [`GROUP`] with `cancel-in-progress: false` (no run is cancelled);
//! - its jobs are not `check`, `set-desired` and `set-desired-mac`; `check`'s `if:` is not [`DESIRED_IF`] (the
//!   repository variable is the gate, at the job level, and only `main` runs it) or the `if:` of
//!   `set-desired` is not [`SET_IF`] (the same gate, and `check` said `set=true`);
//! - `check` needs a job, holds other than `contents: read`, does not run [`check_command`] as
//!   its step `desired` with `VERSION` read from the input, or does not hand on that step's
//!   outputs ([`OUTPUTS`]).
//!   The command gives `--min-hostkeeper` [`APT_FLOOR`], the floor the workflow lint holds the lab
//!   stage of `release.yml` to, and `--macos-pkg`, as the lab stage does: the same release set by
//!   both gets the same targets, and the coordinator refuses a version set again with other targets
//!   (`release_exists`);
//! - `set-desired` needs anything but `check`, is not hostkeeper's reusable workflow
//!   ([`SET_DESIRED`]) pinned to [`CLIENT_COMMIT`] with the comment `# v`[`CLIENT_RELEASE`],
//!   holds other than `contents: read` and `id-token: write`, or its inputs are not exactly those
//!   [`with_wanted`] gives, no more: the service, the version and targets `check` handed on (no
//!   release index, no package digest), the same targets for `set-desired-mac` as for
//!   `set-desired` (a version is one release on the coordinator, whatever the host group, so a
//!   second set with other targets is refused as `release_exists`), the hostkeeper release the pin
//!   names, the coordinator's
//!   two repository variables, the audience, `wait: true` with `max_polls` [`MAX_POLLS`], and the
//!   tailnet's inputs from their repository variables. The wait is what makes a set that orders
//!   no host red: such a version never converges, and the client ends the job `not_converged`;
//!   without it the job ends green, `not_waited`, whatever the hosts do.
//!
//! [`deploy_problems`] holds the deploy of `release.yml` to the same: `deploy-check` is `check`
//! for the release the run published ([`deploy_command`]: `--version` from the plan's tag and
//! `--skip-superseded`, so a release a newer one superseded is not set), after the suite `testing`
//! lists it ([`DEPLOY_CHECK_IF`], needing the plan and the three attempts of `testing-apt-write`),
//! and `deploy` and `deploy-mac` are `set-desired` and `set-desired-mac` with the targets
//! `deploy-check` hands on, under [`DEPLOY_IF`]. Nothing else of the release (the install checks,
//! the lab, the promotion) is in their `needs`: none of them holds the deploy.
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

/// The `if:` of `check`: a run of `main` while the repository variable is `true`.
pub const DESIRED_IF: &str =
    "${{ github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' }}";

/// The concurrency group of the runs by hand.
pub const GROUP: &str = "hostkeeper-desired";

/// The `if:` of release.yml's `deploy-check`: that of the install check `check-apt` (an attempt
/// of `testing-apt-write` listed the release in the suite `testing`, and nothing before it failed),
/// and the repository variable that turns the deploy on.
pub const DEPLOY_CHECK_IF: &str = "${{ !cancelled() && !failure() && needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && (needs.testing-apt-write.outputs.conflict == 'false' || needs.testing-apt-write-2.outputs.conflict == 'false' || needs.testing-apt-write-3.outputs.conflict == 'false') && vars.TIERSTORE_SETS_DESIRED == 'true' }}";

/// What `deploy-check` waits for: the plan and the attempts that write the suite `testing`.
pub const DEPLOY_CHECK_NEEDS: [&str; 4] = [
    "plan",
    "testing-apt-write",
    "testing-apt-write-2",
    "testing-apt-write-3",
];

/// The `if:` of release.yml's `deploy` and `deploy-mac`: the variable, and `deploy-check` said
/// `set=true` (which it says only when it ran, so the run is a release on main).
pub const DEPLOY_IF: &str = "${{ !cancelled() && github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' && needs.deploy-check.outputs.set == 'true' }}";

/// The command of the step `desired` of `deploy-check`: the release the run published, unless a
/// newer one superseded it, with the lab's hostkeeper floor.
pub fn deploy_command() -> String {
    format!(
        "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"${{TAG#v}}\" --skip-superseded --min-hostkeeper {APT_FLOOR} --macos-pkg --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\""
    )
}

/// The `if:` of `set-desired`: the gate again, and `check` chose a release.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.TIERSTORE_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// The command of the step `desired` of `check`, with the lab's hostkeeper floor.
pub fn check_command() -> String {
    format!(
        "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --min-hostkeeper {APT_FLOOR} --macos-pkg --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\""
    )
}

/// The outputs `check` hands on, each its step `desired`'s: `targets` holds the apt lists and the
/// Macs' `macos-arm64` list, and every host group is set them.
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

/// The host group of the Macs, to which the same version and targets are set a second time
/// (`set-desired-mac`).
pub const MAC_HOST_GROUP: &str = "mac-build";

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

/// One line per way the triggers of the workflow (its `on`) are not a dispatch with the one input
/// `version`.
fn trigger_problems(on: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
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
    if found != ["check", "set-desired", "set-desired-mac"] {
        problems.push(format!(
            "jobs are {found:?}, not check, set-desired and set-desired-mac"
        ));
    }
    for (name, want) in [
        ("check", DESIRED_IF),
        ("set-desired", SET_IF),
        ("set-desired-mac", SET_IF),
    ] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is {condition:?}, not {want}"));
        }
    }
    problems.extend(check_problems(
        "check",
        job(jobs, "check"),
        &[],
        &check_command(),
        &[("VERSION", "${{ inputs.version }}")],
    ));
    for (name, group) in [
        ("set-desired", HOST_GROUP),
        ("set-desired-mac", MAC_HOST_GROUP),
    ] {
        problems.extend(set_desired_problems(
            text,
            name,
            "check",
            group,
            job(jobs, name),
        ));
    }
    problems
}

/// One line per way the deploy of `release.yml` (`doc`, from `text`) is not `check`,
/// `set-desired` and `set-desired-mac` for the release the run published, after the suite
/// `testing` lists it.
pub fn deploy_problems(doc: &Node, text: &str) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut problems = Vec::new();
    for (name, want) in [
        ("deploy-check", DEPLOY_CHECK_IF),
        ("deploy", DEPLOY_IF),
        ("deploy-mac", DEPLOY_IF),
    ] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is {condition:?}, not {want}"));
        }
    }
    let check = job(jobs, "deploy-check");
    problems.extend(check_problems(
        "deploy-check",
        check,
        &DEPLOY_CHECK_NEEDS,
        &deploy_command(),
        &[],
    ));
    if check.and_then(|j| at(j, &["env", "TAG"])) != Some("${{ needs.plan.outputs.tag }}") {
        problems.push("deploy-check: env TAG is not ${{ needs.plan.outputs.tag }}".to_owned());
    }
    for (name, group) in [("deploy", HOST_GROUP), ("deploy-mac", MAC_HOST_GROUP)] {
        problems.extend(set_desired_problems(
            text,
            name,
            "deploy-check",
            group,
            job(jobs, name),
        ));
    }
    // Nothing but the report waits for the deploy: a job of the release that needed it would
    // tie the release's checks or promotion to the fleet.
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        let waits = names(entry(job, "needs"));
        for deploy in ["deploy-check", "deploy", "deploy-mac"] {
            let allowed = name == "red"
                || (deploy == "deploy-check" && (name == "deploy" || name == "deploy-mac"));
            if !allowed && waits.iter().any(|w| w == deploy) {
                problems.push(format!(
                    "{name}: needs {deploy}; only the deploy and red wait for it"
                ));
            }
        }
    }
    problems
}

/// One line per way the job `name` is not the read-only check that hands on the release: it
/// needs `needs`, holds `contents: read` alone, runs `command` as its step `desired` with the
/// environment `env` on that step, and hands on the step's outputs.
fn check_problems(
    name: &str,
    check: Option<&Node>,
    needs: &[&str],
    command: &str,
    env: &[(&str, &str)],
) -> Vec<String> {
    let mut problems = Vec::new();
    let got = names(check.and_then(|j| entry(j, "needs")));
    if got != needs {
        problems.push(format!("{name}: needs are {got:?}, not {needs:?}"));
    }
    let held = held(check);
    if held != ["contents: read"] {
        problems.push(format!(
            "{name}: permissions are {held:?}, not contents: read"
        ));
    }
    let all = steps(check);
    let step = all.iter().position(|s| at(s, &["id"]) == Some("desired"));
    let desired = step.map(|i| &all[i]);
    if desired.and_then(|s| at(s, &["run"])).map(str::trim_end) != Some(command) {
        problems.push(format!(
            "{name}: the step `desired` does not run `{command}`"
        ));
    }
    for (var, want) in env {
        if desired.and_then(|s| at(s, &["env", var])) != Some(*want) {
            problems.push(format!(
                "{name}: the step `desired` does not read {var} from {want}"
            ));
        }
    }
    for output in OUTPUTS {
        let got = check.and_then(|j| at(j, &["outputs", output]));
        let want = format!("${{{{ steps.desired.outputs.{output} }}}}");
        if got != Some(want.as_str()) {
            problems.push(format!("{name}: output {output} is {got:?}, not {want}"));
        }
    }
    problems
}

/// What each input of hostkeeper's workflow must be for the host group `group`, with the
/// release the job `check` hands on, and no other input is given.
pub fn with_wanted_from(check: &str, group: &'static str) -> Vec<(&'static str, String)> {
    let output = |key: &str| format!("${{{{ needs.{check}.outputs.{key} }}}}");
    let fixed: [(&'static str, &'static str); 14] = [
        ("service", "tierstore"),
        ("host_group", group),
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
    ];
    let mut wanted = vec![
        ("version", output("package-version")),
        ("targets", output("targets")),
    ];
    wanted.extend(fixed.iter().map(|(k, v)| (*k, (*v).to_owned())));
    wanted
}

/// What each input of hostkeeper's workflow must be in `hostkeeper-desired.yml` for the host
/// group [`HOST_GROUP`].
pub fn with_wanted() -> Vec<(&'static str, String)> {
    with_wanted_from("check", HOST_GROUP)
}

/// One line per way the job `name` (`set-desired` or `deploy` for [`HOST_GROUP`],
/// `set-desired-mac` or `deploy-mac` for [`MAC_HOST_GROUP`]) is not hostkeeper's workflow, called
/// with the release the job `check` handed on, and needing that job alone.
fn set_desired_problems(
    text: &str,
    name: &str,
    check: &str,
    group: &'static str,
    job: Option<&Node>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(job.and_then(|j| entry(j, "needs")));
    if needs != [check] {
        problems.push(format!("{name}: needs are {needs:?}, not [{check:?}]"));
    }
    let held = held(job);
    if held != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "{name}: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let uses = job.and_then(|j| entry(j, "uses"));
    let want = format!("{SET_DESIRED}@{CLIENT_COMMIT}");
    if uses.and_then(Node::as_str) != Some(want.as_str()) {
        problems.push(format!("{name}: `uses` is not {want}"));
    }
    // The version comment of the pin, on the line of `uses:`; the general lint holds its form.
    let release = uses
        .and_then(|u| text.lines().nth(u.line() - 1))
        .and_then(|line| line.split_once(" # v"))
        .map(|(_, version)| version.trim_end());
    if release != Some(CLIENT_RELEASE) {
        problems.push(format!(
            "{name}: the pin's comment names {release:?}, not v{CLIENT_RELEASE}"
        ));
    }
    let wanted = with_wanted_from(check, group);
    for (key, want) in &wanted {
        let got = job.and_then(|j| at(j, &["with", key]));
        if got != Some(want.as_str()) {
            problems.push(format!("{name}: with.{key} is {got:?}, not {want:?}"));
        }
    }
    // No other input: not `package_digest`, which the client refuses beside the targets, and not
    // `release_index` or `release_index_digest`, which would have to list archive entries the
    // targets do not have.
    let given = job.and_then(|j| entry(j, "with")).map_or(&[][..], entries);
    for (key, _) in given {
        let key = key.as_str().unwrap_or_default();
        if !wanted.iter().any(|(k, _)| *k == key) {
            problems.push(format!("{name}: with.{key} is not an input it gives"));
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
/// calls it; a person starts it.
pub fn caller_problems(files: &[(String, String)]) -> Vec<String> {
    files
        .iter()
        .filter(|(name, text)| name != FILE && text.contains(FILE))
        .map(|(name, _)| format!("{name}: names {FILE}, which no other workflow calls"))
        .collect()
}
