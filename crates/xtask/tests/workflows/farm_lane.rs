//! The rules of `farm.yml` ([`FILE`]), the farm lane: `buck2 build //:ci` on the build farm,
//! additive and advisory beside ci.yml, which stays the pull-request gate (docs/farm.md). Not to be
//! confused with the release's lab jobs (`farm.rs`), which install a release on the lab's hosts.
//!
//! It is the one workflow that runs for a pull request with a job that holds a write scope: its
//! job [`JOB`] holds `id-token: write`, so that komira's farm-connect action ([`FARM_CONNECT`])
//! can join the farm's private network with the job's OIDC token (workload identity federation:
//! no stored secret). [`super::lint::WRITE_ALLOWED`] allows that scope to this job alone, under its
//! exact `if:` ([`JOB_IF`]: a push to main, or a pull request from a branch of this repository that
//! is not Dependabot's; a fork's pull request gets no token and is skipped), and
//! [`unversioned_pin_allowed`] lets this file, and no other, pin farm-connect by commit with no
//! version comment (komira tags no release of it). Every other rule of [`super::lint`] holds for
//! the file as for any workflow.
//!
//! [`problems`] holds the file to its shape: the triggers, the one job, its rights, farm-connect's
//! inputs, the check of the build and the steps that offer the sync patch. It does not fix the
//! other steps of the job, each of which could ask for the token; a change to them is reviewed
//! like any change to a workflow:
//! - its `on` is exactly `pull_request` with no filter and `push` to `branches: [main]`;
//! - its jobs are exactly [`JOB`], on [`RUNNER`], with `permissions` exactly `contents: read`
//!   and `id-token: write`;
//! - farm-connect is used by exactly one step, whose `with:` is exactly [`CONNECT_INPUTS`]: the
//!   tailnet identity (`TS_CLIENT_ID`, `TS_AUDIENCE`) and the farm's address, instance, worker
//!   property set and TLS switch, each from the organisation's variable of its name, and nothing
//!   else (no secret, no token, no input a pull request could set);
//! - a step after farm-connect's runs exactly [`BUILD_RUN`]: `cargo xtask farm-build` of `//:ci`,
//!   which refuses a build with no farm workers or whose summary line does not say `local: 0`
//!   (crates/xtask/src/farm.rs);
//! - after it, in this order, the three steps that offer the regenerated buildrules tables of a
//!   failed build (buildrules' docs/how.md, "A stale table in a pull request"; docs/farm.md): the
//!   step [`SYNC_ID`] runs [`SYNC_RUN`] under `if: failure()`, an [`UPLOAD`] step uploads its
//!   `offer/` with exactly [`OFFER_INPUTS`] under [`OFFER_IF`], and a step runs [`SUMMARY_RUN`]
//!   under [`SUMMARY_IF`]. Each is held word for word, so a pull request that changes a manifest
//!   keeps getting the patch and the commands that apply it.

use std::collections::BTreeMap;

use super::lint::{entries, entry, names};
use super::yaml::{Node, Value};

/// The workflow.
pub const FILE: &str = "farm.yml";
/// Its one job, the one that may hold `id-token: write` on a pull request.
pub const JOB: &str = "build";
/// The runner of [`JOB`].
pub const RUNNER: &str = "ubuntu-24.04";
/// The exact `if:` of [`JOB`].
pub const JOB_IF: &str = "${{ github.event_name == 'push' || (github.event.pull_request.head.repo.full_name == github.repository && !startsWith(github.head_ref, 'dependabot/')) }}";
/// komira's action that joins the farm's network and writes the farm's buckconfig.
pub const FARM_CONNECT: &str = "komira-ai/komira/.github/actions/farm-connect";
/// The `with:` of the farm-connect step, exactly.
pub const CONNECT_INPUTS: [(&str, &str); 6] = [
    ("ts-client-id", "${{ vars.TS_CLIENT_ID }}"),
    ("ts-audience", "${{ vars.TS_AUDIENCE }}"),
    ("farm-address", "${{ vars.FARM_ADDRESS }}"),
    ("farm-instance", "${{ vars.FARM_INSTANCE }}"),
    (
        "farm-properties",
        "${{ vars.FARM_LINUX_X86_64_PROPERTIES }}",
    ),
    ("tls", "${{ vars.FARM_TLS || 'false' }}"),
];
/// The build step's `run:`.
pub const BUILD_RUN: &str =
    "cargo xtask farm-build --buck2 \"$RUNNER_TEMP/bin/buck2\" --target //:ci";

/// The `id:` of the step that builds buildrules' `//:sync_patch`, which the two after it read.
pub const SYNC_ID: &str = "sync_patch";
/// The `if:` of that step: it runs only when a step before it failed.
pub const SYNC_IF: &str = "failure()";
/// Its `run:`: `buildrules sync` on the farm, whose output (the verdict and `offer/`) goes under
/// the runner's temporary directory.
pub const SYNC_RUN: &str =
    r#""$RUNNER_TEMP/bin/buck2" build //:sync_patch --out "$RUNNER_TEMP/sync-patch""#;
/// The action of the step that uploads the patch (its pin is held by the lint's pin rule).
pub const UPLOAD: &str = "actions/upload-artifact@";
/// The `if:` of the upload: only when the sync finished.
pub const OFFER_IF: &str = "failure() && steps.sync_patch.outcome == 'success'";
/// The `with:` of the upload, exactly: the artifact `sync-patch`, made of `offer/` (empty when
/// there is no patch, which then uploads nothing).
pub const OFFER_INPUTS: [(&str, &str); 3] = [
    ("name", "sync-patch"),
    ("path", "${{ runner.temp }}/sync-patch/offer/"),
    ("if-no-files-found", "ignore"),
];
/// The `if:` of the summary: whenever the sync ran, so a sync that failed is said too.
pub const SUMMARY_IF: &str = "failure() && steps.sync_patch.outcome != 'skipped'";
/// The summary's `run:`: buildrules' verdict and the two commands that take the patch of this
/// run's artifact, in the job's summary.
pub const SUMMARY_RUN: &str = r#""$RUNNER_TEMP/bin/buck2" run buildrules//rules/rust:sync_patch -- summary --dir "$RUNNER_TEMP/sync-patch" --run "$GITHUB_RUN_ID" --repository "$GITHUB_REPOSITORY" --artifact sync-patch --to "$GITHUB_STEP_SUMMARY""#;
/// What a job whose three steps of the patch are not in order after the build is refused with.
pub const OFFER_ORDER_PROBLEM: &str = "job build: the steps that offer the sync patch must follow the build, in the order sync, upload, summary";

/// What a [`FILE`] whose `on` is anything else is refused with.
pub const ON_PROBLEM: &str =
    "farm.yml: on must be pull_request with no filter and push to branches [main] alone";

/// Whether the `uses:` value `value` in `file` is farm-connect pinned where it may go without a
/// version comment (the pin itself is still held to a 40-digit commit).
pub fn unversioned_pin_allowed(file: &str, value: &str) -> bool {
    file == FILE
        && value
            .strip_prefix(FARM_CONNECT)
            .is_some_and(|rest| rest.starts_with('@'))
}

/// The scalars of the mapping `node`, by key; `None` when a key or value is not a scalar.
fn scalar_map(node: &Node) -> Option<BTreeMap<&str, &str>> {
    match &node.value {
        Value::Map(entries) => entries
            .iter()
            .map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
            .collect(),
        _ => None,
    }
}

/// Whether `on` is exactly `pull_request` with no filter and `push` with `branches: [main]`.
fn pull_request_and_push_to_main(on: Option<&Node>) -> bool {
    let Some(on) = on.filter(|on| matches!(on.value, Value::Map(_))) else {
        return false;
    };
    let unfiltered = entry(on, "pull_request").is_some_and(|v| {
        v.is_empty_map() || (matches!(v.value, Value::Scalar { .. }) && v.as_str().is_none())
    });
    let to_main = entry(on, "push").is_some_and(|push| match entries(push) {
        [(key, branches)] => key.as_str() == Some("branches") && only_main(branches),
        _ => false,
    });
    names(Some(on)) == ["pull_request", "push"] && unfiltered && to_main
}

/// Whether `branches` is the list `[main]`.
fn only_main(branches: &Node) -> bool {
    match &branches.value {
        Value::Seq(items) => matches!(&items[..], [only] if only.as_str() == Some("main")),
        _ => false,
    }
}

/// The steps of `job`.
fn steps(job: &Node) -> &[Node] {
    match entry(job, "steps").map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    }
}

/// Whether the step `step` uses farm-connect.
fn connects(step: &Node) -> bool {
    entry(step, "uses")
        .and_then(Node::as_str)
        .is_some_and(|u| unversioned_pin_allowed(FILE, u))
}

/// The scalar of `key` in the step `step`.
fn scalar<'a>(step: &'a Node, key: &str) -> Option<&'a str> {
    entry(step, key).and_then(Node::as_str)
}

/// Whether `step` is the step that builds `//:sync_patch`.
fn syncs(step: &Node) -> bool {
    scalar(step, "id") == Some(SYNC_ID)
        && scalar(step, "if") == Some(SYNC_IF)
        && scalar(step, "run") == Some(SYNC_RUN)
}

/// Whether `step` uploads the patch as the artifact `sync-patch`.
fn offers(step: &Node) -> bool {
    scalar(step, "uses").is_some_and(|u| u.starts_with(UPLOAD))
        && scalar(step, "if") == Some(OFFER_IF)
        && entry(step, "with").and_then(scalar_map) == Some(BTreeMap::from(OFFER_INPUTS))
}

/// Whether `step` writes the verdict and the commands to the job's summary.
fn summarises(step: &Node) -> bool {
    scalar(step, "if") == Some(SUMMARY_IF) && scalar(step, "run") == Some(SUMMARY_RUN)
}

/// What is wrong with the three steps that offer the patch, given the position of the build.
fn offer_problems(steps: &[Node], build: Option<usize>) -> Vec<String> {
    let mut problems = Vec::new();
    let sync = steps.iter().position(syncs);
    if sync.is_none() {
        problems.push(format!(
            "job {JOB}: no step `id: {SYNC_ID}` runs `{SYNC_RUN}` under `if: {SYNC_IF}`"
        ));
    }
    let upload = steps.iter().position(offers);
    if upload.is_none() {
        problems.push(format!(
            "job {JOB}: no {UPLOAD} step under `if: {OFFER_IF}` has the `with:` {OFFER_INPUTS:?}"
        ));
    }
    let summary = steps.iter().position(summarises);
    if summary.is_none() {
        problems.push(format!(
            "job {JOB}: no step runs `{SUMMARY_RUN}` under `if: {SUMMARY_IF}`"
        ));
    }
    let found: Vec<usize> = [build, sync, upload, summary]
        .into_iter()
        .flatten()
        .collect();
    if found.windows(2).any(|pair| pair[0] >= pair[1]) {
        problems.push(OFFER_ORDER_PROBLEM.to_owned());
    }
    problems
}

/// What is wrong with the shape of [`FILE`], the document `doc`.
pub fn problems(doc: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if !pull_request_and_push_to_main(entry(doc, "on")) {
        problems.push(ON_PROBLEM.to_owned());
    }
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    if !jobs.iter().any(|(name, _)| name.as_str() == Some(JOB)) {
        problems.push(format!("{FILE}: no job {JOB}"));
    }
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        if name != JOB {
            problems.push(format!("job {name} is not a job of {FILE}"));
            continue;
        }
        problems.extend(job_problems(job));
    }
    problems
}

/// What is wrong with the job [`JOB`].
fn job_problems(job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if entry(job, "runs-on").and_then(Node::as_str) != Some(RUNNER) {
        problems.push(format!("job {JOB}: runs-on must be {RUNNER}"));
    }
    let permissions = entry(job, "permissions").and_then(scalar_map);
    let wanted = BTreeMap::from([("contents", "read"), ("id-token", "write")]);
    if permissions.as_ref() != Some(&wanted) {
        problems.push(format!(
            "job {JOB}: permissions must be exactly contents: read and id-token: write"
        ));
    }
    let steps = steps(job);
    let connecting: Vec<&Node> = steps.iter().filter(|s| connects(s)).collect();
    let inputs = BTreeMap::from(CONNECT_INPUTS);
    match &connecting[..] {
        [only] if entry(only, "with").and_then(scalar_map).as_ref() == Some(&inputs) => {}
        [_] => problems.push(format!(
            "job {JOB}: the farm-connect step's `with:` must be exactly {CONNECT_INPUTS:?}"
        )),
        _ => problems.push(format!(
            "job {JOB}: exactly one step must use {FARM_CONNECT}"
        )),
    }
    let connect = steps.iter().position(connects);
    let build = steps
        .iter()
        .position(|s| entry(s, "run").and_then(Node::as_str) == Some(BUILD_RUN));
    match (connect, build) {
        (_, None) => problems.push(format!("job {JOB}: no step runs `{BUILD_RUN}`")),
        (Some(c), Some(b)) if c < b => {}
        _ => problems.push(format!(
            "job {JOB}: the build does not come after the farm-connect step"
        )),
    }
    problems.extend(offer_problems(steps, build));
    problems
}
