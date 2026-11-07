//! The macOS checks of a pull request: `macos.yml`, the one workflow that runs a pull request on a
//! macOS runner, and only when the pull request changes the macOS surface
//! (`xtask::mac_surface::SURFACE`).
//!
//! [`macos_pr_problems`] refuses `macos.yml` when:
//! - its triggers are not `pull_request` alone, or its concurrency is not one group per pull
//!   request ([`GROUP`]) that cancels the older run;
//! - its jobs are not `surface`, `macos-arm64` and `macos`;
//! - `surface` does not run on ubuntu, from a checkout of the merge commit and its parents
//!   (`fetch-depth: 2`), exactly [`surface_commands`] (the changed paths, the answer of `cargo
//!   xtask mac-surface`, the version of the build), with the version under [`VERSION_IF`] alone,
//!   and hand the answer and the version on as its outputs;
//! - `macos-arm64` does not need `surface` and run under [`ON_THE_SURFACE`] on the runner of the
//!   macOS release build, with its `TARGET` and the version `surface` gives, exactly
//!   [`pr_mac_commands`] (the commands of artifacts.yml's macOS build job, with the tests of the
//!   whole workspace for the tests of the binary's crates), with no action but the checkout and a
//!   cache restore (it uploads, saves and signs nothing), and with no `if:`, `env` or
//!   `continue-on-error` on a step;
//! - `macos`, the check to require, does not need both, run under [`GATE_IF`] on ubuntu, and hold
//!   one step, `exit 1` under [`GATE_STEP_IF`]: it fails unless `surface` passed and
//!   `macos-arm64` passed on the surface or was skipped off it;
//! - the cargo-cyclonedx pins of its `env` are not artifacts.yml's.
//!
//! [`pull_request_job_problems`] refuses a job a pull request runs on macOS that holds anything
//! but `contents: read`, names an environment, or names a secret or the token anywhere in it or in
//! its workflow's `env` (which every job sees). `platforms::pull_request_macos_problems` applies
//! it to every workflow, and refuses a job a pull request runs on macOS outside `macos.yml`.

use super::lint::{entries, entry, names};
use super::platforms::{
    BUILDS, MAC_TESTS, action_problems, at, build_commands, command_problems, commands,
    execution_problems, steps,
};
use super::yaml::{self, Node, Value};

/// The workflow of the macOS checks of a pull request.
pub const MACOS_PR: &str = "macos.yml";

/// The one group of a pull request's runs.
pub const GROUP: &str = "macos-${{ github.event.pull_request.number }}";

/// The `if:` of the macOS job: the pull request changes the macOS surface.
pub const ON_THE_SURFACE: &str = "${{ needs.surface.outputs.mac == 'true' }}";

/// The `if:` of the version step of `surface`: on the surface alone.
pub const VERSION_IF: &str = "${{ steps.surface.outputs.mac == 'true' }}";

/// The `if:` of the check to require: it runs whatever the jobs it needs did, unless the run was
/// cancelled (a newer run of the pull request reports it).
pub const GATE_IF: &str = "${{ !cancelled() }}";

/// The `if:` of the one step of `macos`, which fails it.
pub const GATE_STEP_IF: &str = "${{ needs.surface.result != 'success' || (needs.surface.outputs.mac == 'true' && needs.macos-arm64.result != 'success') || (needs.surface.outputs.mac != 'true' && needs.macos-arm64.result != 'skipped') }}";

/// The tests of the macOS job: every test of the workspace, none left out.
pub const PR_MAC_TESTS: &str =
    "cargo test --locked --workspace --features tierstore/s3 --no-fail-fast";

/// The jobs of `macos.yml`, sorted.
pub const JOBS: [&str; 3] = ["macos", "macos-arm64", "surface"];

/// The variables of the file's `env` that pin cargo-cyclonedx, which must be artifacts.yml's.
const PINS: [&str; 2] = ["CYCLONEDX_VERSION", "CYCLONEDX_SHA256_AARCH64_MACOS"];

/// The `run:` commands of `surface`, in order.
pub fn surface_commands() -> Vec<String> {
    vec![
        "rustup toolchain install".to_owned(),
        "cargo xtask --help".to_owned(),
        "git diff --name-only -z --no-renames HEAD^1 HEAD > \"$RUNNER_TEMP/changed\"".to_owned(),
        "cargo xtask mac-surface --changed \"$RUNNER_TEMP/changed\" --output \"$GITHUB_OUTPUT\""
            .to_owned(),
        "cargo xtask release version --repo \"$GITHUB_REPOSITORY\" --version-file VERSION --output \"$GITHUB_OUTPUT\""
            .to_owned(),
    ]
}

/// The build of the macOS release platform.
fn mac_build() -> &'static super::platforms::Build {
    BUILDS
        .iter()
        .find(|b| b.platform == "macos-arm64")
        .expect("the macOS build")
}

/// The `run:` commands of `macos-arm64`, in order: artifacts.yml's macOS build job's, with the
/// tests of the workspace in place of the tests of the binary's crates.
pub fn pr_mac_commands() -> Vec<String> {
    build_commands(mac_build())
        .into_iter()
        .map(|c| {
            if c == MAC_TESTS {
                PR_MAC_TESTS.to_owned()
            } else {
                c
            }
        })
        .collect()
}

/// Every scalar of `node`, keys and values.
pub fn scalars<'a>(node: &'a Node, out: &mut Vec<&'a str>) {
    match &node.value {
        Value::Scalar { .. } => out.extend(node.as_str()),
        Value::Seq(items) => items.iter().for_each(|i| scalars(i, out)),
        Value::Map(entries) => {
            for (k, v) in entries {
                scalars(k, out);
                scalars(v, out);
            }
        }
    }
}

/// What in `text` names a secret or the token, if anything: `secrets` in any case (the context,
/// `secrets: inherit`), `github.token` or `GITHUB_TOKEN`.
fn names_a_secret(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    ["secrets", "github.token", "github_token"]
        .into_iter()
        .find(|word| lower.contains(word))
}

/// One line per way the job `name` of `file`, which a pull request runs on macOS, can reach a
/// secret, the token or a write or OIDC scope: permissions other than `contents: read` alone, an
/// `environment`, or a secret or the token named in the job or in `workflow_env` (the workflow's
/// `env`, which the job sees).
pub fn pull_request_job_problems(
    file: &str,
    name: &str,
    job: &Node,
    workflow_env: Option<&Node>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let held: Vec<String> = entry(job, "permissions")
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
    if held != ["contents: read"] {
        problems.push(format!(
            "{file}: the job {name} runs on macOS for a pull request with permissions {held:?}, not contents: read alone"
        ));
    }
    if entry(job, "environment").is_some() {
        problems.push(format!(
            "{file}: the job {name} runs on macOS for a pull request in an environment, whose secrets it would hold"
        ));
    }
    let mut found = Vec::new();
    scalars(job, &mut found);
    if let Some(env) = workflow_env {
        scalars(env, &mut found);
    }
    for text in found {
        if let Some(word) = names_a_secret(text) {
            problems.push(format!(
                "{file}: the job {name} runs on macOS for a pull request and names {word} (`{text}`); it holds no secret and no token"
            ));
        }
    }
    problems
}

/// One line per way `macos.yml` (`text`) is not the macOS checks of a pull request, with
/// `artifacts` the text of artifacts.yml (whose cargo-cyclonedx pins it must hold).
pub fn macos_pr_problems(text: &str, artifacts: &str) -> Vec<String> {
    let (doc, theirs) = match (yaml::load(text), yaml::load(artifacts)) {
        (Ok(doc), Ok(theirs)) => (doc, theirs),
        (Err(e), _) | (_, Err(e)) => return vec![e],
    };
    let mut problems = Vec::new();
    let triggers = names(entry(&doc, "on"));
    if triggers != ["pull_request"] {
        problems.push(format!(
            "{MACOS_PR}: triggers are {triggers:?}, not [\"pull_request\"]"
        ));
    }
    let group = at(&doc, &["concurrency", "group"]);
    if group != Some(GROUP) {
        problems.push(format!(
            "{MACOS_PR}: concurrency group is {group:?}, not {GROUP}"
        ));
    }
    let cancel = at(&doc, &["concurrency", "cancel-in-progress"]);
    if cancel != Some("true") {
        problems.push(format!(
            "{MACOS_PR}: concurrency cancel-in-progress is {cancel:?}, not true"
        ));
    }
    for pin in PINS {
        let (ours, want) = (at(&doc, &["env", pin]), at(&theirs, &["env", pin]));
        if ours.is_none() || ours != want {
            problems.push(format!(
                "{MACOS_PR}: env {pin} is {ours:?}, not artifacts.yml's {want:?}"
            ));
        }
    }
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let mut listed: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    listed.sort_unstable();
    if listed != JOBS {
        problems.push(format!("{MACOS_PR}: jobs are {listed:?}, not {JOBS:?}"));
    }
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    problems.extend(surface_problems(job("surface")));
    problems.extend(mac_job_problems(job("macos-arm64"), entry(&doc, "env")));
    problems.extend(gate_problems(job("macos")));
    problems
}

/// One line per way `surface` is not the answer and the version.
fn surface_problems(job: Option<&Node>) -> Vec<String> {
    let Some(surface) = job else {
        return vec![format!("{MACOS_PR}: no job surface")];
    };
    let mut problems = Vec::new();
    if !at(surface, &["runs-on"]).is_some_and(|r| r.starts_with("ubuntu-")) {
        problems.push("surface: does not run on an ubuntu runner".to_owned());
    }
    for key in ["if", "continue-on-error"] {
        if entry(surface, key).is_some() {
            problems.push(format!("surface: has `{key}`"));
        }
    }
    let checkout = steps(job)
        .iter()
        .find(|s| at(s, &["uses"]).is_some_and(|u| u.starts_with("actions/checkout@")));
    if checkout.and_then(|s| at(s, &["with", "fetch-depth"])) != Some("2") {
        problems.push(
            "surface: the checkout is not of the merge commit and its parents (fetch-depth: 2)"
                .to_owned(),
        );
    }
    problems.extend(command_problems(
        "surface",
        &commands(job),
        &surface_commands(),
    ));
    problems.extend(action_problems("surface", job, &["actions/checkout@"]));
    for step in steps(job) {
        let run = at(step, &["run"]).unwrap_or_default();
        let condition = at(step, &["if"]);
        let id = at(step, &["id"]);
        let (want_if, want_id) = if run.contains("release version") {
            (Some(VERSION_IF), Some("version"))
        } else if run.contains("mac-surface") {
            (None, Some("surface"))
        } else {
            (None, id)
        };
        if condition != want_if {
            problems.push(format!(
                "surface: the step `{}` runs under `if: {condition:?}`, not {want_if:?}",
                run.trim_end()
            ));
        }
        if id != want_id {
            problems.push(format!(
                "surface: the step `{}` has the id {id:?}, not {want_id:?}",
                run.trim_end()
            ));
        }
        if entry(step, "continue-on-error").is_some() {
            problems.push(format!(
                "surface: the step `{}` has continue-on-error",
                run.trim_end()
            ));
        }
    }
    for (output, want) in [
        ("mac", "${{ steps.surface.outputs.mac }}"),
        ("version", "${{ steps.version.outputs.version }}"),
    ] {
        let got = at(surface, &["outputs", output]);
        if got != Some(want) {
            problems.push(format!("surface: output {output} is {got:?}, not {want}"));
        }
    }
    problems
}

/// One line per way `macos-arm64` is not the macOS build of artifacts.yml with the workspace's
/// tests, on the surface alone, holding nothing.
fn mac_job_problems(job: Option<&Node>, workflow_env: Option<&Node>) -> Vec<String> {
    let Some(mac) = job else {
        return vec![format!("{MACOS_PR}: no job macos-arm64")];
    };
    let b = mac_build();
    let name = "macos-arm64";
    let mut problems = Vec::new();
    let needs = names(entry(mac, "needs"));
    if needs != ["surface"] {
        problems.push(format!("{name}: needs are {needs:?}, not [\"surface\"]"));
    }
    let runner = at(mac, &["runs-on"]);
    if runner != Some(b.runner) {
        problems.push(format!(
            "{name}: runs on {runner:?}, not {:?}, the runner of the macOS build",
            b.runner
        ));
    }
    let mut keys: Vec<&str> = entry(mac, "env")
        .map_or(&[][..], entries)
        .iter()
        .filter_map(|(k, _)| k.as_str())
        .collect();
    keys.sort_unstable();
    if keys != ["RELEASE_VERSION", "TARGET"] {
        problems.push(format!(
            "{name}: its env sets {keys:?}, not RELEASE_VERSION and TARGET alone"
        ));
    }
    if at(mac, &["env", "TARGET"]) != Some(b.target) {
        problems.push(format!("{name}: TARGET is not {}", b.target));
    }
    let version = "${{ needs.surface.outputs.version }}";
    if at(mac, &["env", "RELEASE_VERSION"]) != Some(version) {
        problems.push(format!("{name}: env RELEASE_VERSION is not {version}"));
    }
    problems.extend(execution_problems(name, mac, Some(ON_THE_SURFACE)));
    for step in steps(job) {
        if entry(step, "env").is_some() {
            let label = at(step, &["name"])
                .or_else(|| at(step, &["run"]))
                .unwrap_or_default();
            problems.push(format!("{name}: the step `{label}` sets its own env"));
        }
    }
    problems.extend(command_problems(name, &commands(job), &pr_mac_commands()));
    problems.extend(action_problems(
        name,
        job,
        &["actions/checkout@", "actions/cache/restore@"],
    ));
    problems.extend(pull_request_job_problems(MACOS_PR, name, mac, workflow_env));
    problems
}

/// One line per way `macos` is not the check that passes exactly when the macOS checks passed or
/// had nothing to check.
fn gate_problems(job: Option<&Node>) -> Vec<String> {
    let Some(gate) = job else {
        return vec![format!("{MACOS_PR}: no job macos")];
    };
    let mut problems = Vec::new();
    let mut needs = names(entry(gate, "needs"));
    needs.sort();
    if needs != ["macos-arm64", "surface"] {
        problems.push(format!(
            "macos: needs are {needs:?}, not macos-arm64 and surface"
        ));
    }
    if at(gate, &["if"]) != Some(GATE_IF) {
        problems.push(format!("macos: `if` is not {GATE_IF}"));
    }
    if !at(gate, &["runs-on"]).is_some_and(|r| r.starts_with("ubuntu-")) {
        problems.push("macos: does not run on an ubuntu runner".to_owned());
    }
    if entry(gate, "continue-on-error").is_some() {
        problems.push("macos: has continue-on-error".to_owned());
    }
    let all = steps(job);
    let fails = all.len() == 1
        && at(&all[0], &["if"]) == Some(GATE_STEP_IF)
        && at(&all[0], &["run"]).map(str::trim_end) == Some("exit 1")
        && entry(&all[0], "continue-on-error").is_none();
    if !fails {
        problems.push(format!(
            "macos: its steps are not the one `exit 1` under `if: {GATE_STEP_IF}`"
        ));
    }
    problems
}
