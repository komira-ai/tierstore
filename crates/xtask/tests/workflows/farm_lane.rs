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
//! [`unversioned_pin_allowed`] lets this file, and no other, pin farm-connect and buck2-farm
//! ([`BUCK2_FARM`]) by commit with no version comment (komira tags no release of them). Every other
//! rule of [`super::lint`] holds for the file as for any workflow, [`super::farm_steps`]' among
//! them: no step runs buck2's build itself, and every step has a time limit under the job's.
//!
//! [`problems`] holds the file to its shape: the triggers, the one job, its rights, farm-connect's
//! inputs and the build through buck2-farm. It does not fix the other steps of the job, each of
//! which could ask for the token; a change to them is reviewed like any change to a workflow:
//! - its `on` is exactly `pull_request` with no filter and `push` to `branches: [main]`;
//! - its jobs are exactly [`JOB`], on [`RUNNER`], with `permissions` exactly `contents: read`
//!   and `id-token: write`;
//! - farm-connect is used by exactly one step, whose `with:` is exactly [`CONNECT_INPUTS`]: the
//!   tailnet identity (`TS_CLIENT_ID`, `TS_AUDIENCE`) and the farm's address, instance, worker
//!   property set and TLS switch, each from the organisation's variable of its name, and nothing
//!   else (no secret, no token, no input a pull request could set);
//! - exactly one step uses buck2-farm, with `with:` exactly [`BUILD_INPUTS`]: `build //:ci` with
//!   the buck2 this job installs and the assert_remote it builds. The action runs the build again
//!   after a cut or hung run, and fails it unless every summary line says `local: 0`;
//! - in this order: a step checks out komira's sources ([`KOMIRA`]), a step runs exactly
//!   [`TOOL_RUN`] (the build of assert_remote from them), the farm-connect step, a step that runs
//!   exactly [`CHECK_RUN`] (crates/xtask/src/farm.rs: no build without the farm's worker property
//!   set), and the buck2-farm step;
//! - farm-connect, buck2-farm and the checkout of komira's sources are at one komira commit: the
//!   action and the tool it runs are one version, and farm-connect writes the buckconfig both
//!   read.

use std::collections::BTreeMap;

pub use super::farm_steps::{BUCK2_FARM, FARM_CONNECT};
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
/// The `with:` of the buck2-farm step, exactly.
pub const BUILD_INPUTS: [(&str, &str); 3] = [
    ("args", "build //:ci"),
    ("assert-remote", "${{ runner.temp }}/bin/assert_remote"),
    ("buck2", "${{ runner.temp }}/bin/buck2"),
];
/// The check before the build, exactly.
pub const CHECK_RUN: &str = "cargo xtask farm-check --buck2 \"$RUNNER_TEMP/bin/buck2\"";
/// The build of buck2-farm's tool from komira's sources, exactly.
pub const TOOL_RUN: &str = "\"$RUNNER_TEMP/zig/zig\" build-exe -OReleaseSafe -fstrip -target x86_64-linux-musl -femit-bin=\"$RUNNER_TEMP/bin/assert_remote\" target/komira/tools/build/ci/assert_remote/assert_remote.zig";
/// The repository of the two actions and of the tool's sources.
pub const KOMIRA: &str = "komira-ai/komira";

/// What a [`FILE`] whose `on` is anything else is refused with.
pub const ON_PROBLEM: &str =
    "farm.yml: on must be pull_request with no filter and push to branches [main] alone";

/// Whether the `uses:` value `value` in `file` is farm-connect pinned where it may go without a
/// version comment (the pin itself is still held to a 40-digit commit).
pub fn unversioned_pin_allowed(file: &str, value: &str) -> bool {
    file == FILE && commit_of(value).is_some()
}

/// The ref `value`, a `uses:` value, pins farm-connect or buck2-farm to; `None` for another action.
fn commit_of(value: &str) -> Option<&str> {
    [FARM_CONNECT, BUCK2_FARM]
        .iter()
        .find_map(|action| value.strip_prefix(action)?.strip_prefix('@'))
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

/// Whether the step `step` uses the action `action` at some ref.
fn uses_action(step: &Node, action: &str) -> bool {
    entry(step, "uses")
        .and_then(Node::as_str)
        .and_then(|u| u.strip_prefix(action))
        .is_some_and(|rest| rest.starts_with('@'))
}

/// Whether the step `step` uses farm-connect.
fn connects(step: &Node) -> bool {
    uses_action(step, FARM_CONNECT)
}

/// Whether the step `step` checks out [`KOMIRA`].
fn checks_out_komira(step: &Node) -> bool {
    entry(step, "with")
        .and_then(|w| entry(w, "repository"))
        .and_then(Node::as_str)
        == Some(KOMIRA)
}

/// What is wrong with the shape of `file`, the document `doc`: nothing unless it is [`FILE`].
pub fn problems(file: &str, doc: &Node) -> Vec<String> {
    if file != FILE {
        return Vec::new();
    }
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
    let building: Vec<&Node> = steps
        .iter()
        .filter(|s| uses_action(s, BUCK2_FARM))
        .collect();
    let build_inputs = BTreeMap::from(BUILD_INPUTS);
    match &building[..] {
        [only] if entry(only, "with").and_then(scalar_map).as_ref() == Some(&build_inputs) => {}
        [_] => problems.push(format!(
            "job {JOB}: the buck2-farm step's `with:` must be exactly {BUILD_INPUTS:?}"
        )),
        _ => problems.push(format!("job {JOB}: exactly one step must use {BUCK2_FARM}")),
    }
    problems.extend(order_problems(steps));
    problems.extend(commit_problems(steps));
    problems
}

/// One line per step of the build's chain that is missing or out of order: komira's sources, the
/// tool built from them, farm-connect, the check, and the build through buck2-farm.
fn order_problems(steps: &[Node]) -> Vec<String> {
    let runs = |wanted: &'static str| {
        move |s: &Node| entry(s, "run").and_then(Node::as_str) == Some(wanted)
    };
    let chain: [(&str, Option<usize>); 5] = [
        (
            "checks out komira's sources",
            steps.iter().position(checks_out_komira),
        ),
        (
            "builds assert_remote",
            steps.iter().position(runs(TOOL_RUN)),
        ),
        ("uses farm-connect", steps.iter().position(connects)),
        (
            "runs the farm check",
            steps.iter().position(runs(CHECK_RUN)),
        ),
        (
            "uses buck2-farm",
            steps.iter().position(|s| uses_action(s, BUCK2_FARM)),
        ),
    ];
    let mut problems: Vec<String> = chain
        .iter()
        .filter(|(_, at)| at.is_none())
        .map(|(what, _)| format!("job {JOB}: no step {what}"))
        .collect();
    let found: Vec<&(&str, Option<usize>)> = chain.iter().filter(|(_, at)| at.is_some()).collect();
    for pair in found.windows(2) {
        if pair[0].1 > pair[1].1 {
            problems.push(format!(
                "job {JOB}: the step that {} must come before the step that {}",
                pair[0].0, pair[1].0
            ));
        }
    }
    problems
}

/// A line when farm-connect, buck2-farm and the checkout of komira's sources are not at one
/// komira commit.
fn commit_problems(steps: &[Node]) -> Vec<String> {
    let mut commits: Vec<&str> = steps
        .iter()
        .filter_map(|s| entry(s, "uses").and_then(Node::as_str).and_then(commit_of))
        .collect();
    commits.extend(steps.iter().filter(|s| checks_out_komira(s)).map(|s| {
        entry(s, "with")
            .and_then(|w| entry(w, "ref"))
            .and_then(Node::as_str)
            .unwrap_or_default()
    }));
    commits.sort_unstable();
    commits.dedup();
    if commits.len() > 1 {
        vec![format!(
            "job {JOB}: farm-connect, buck2-farm and komira's sources must be at one komira commit, not {commits:?}"
        )]
    } else {
        Vec::new()
    }
}
