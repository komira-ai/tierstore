//! The macOS checks of a pull request: `macos.yml`, the one workflow that runs a pull request on a
//! macOS runner, for every pull request.
//!
//! [`macos_pr_problems`] refuses `macos.yml` when:
//! - its triggers are not `pull_request` alone, or its concurrency is not one group per pull
//!   request ([`GROUP`]) that cancels the older run;
//! - its jobs are not `version` and [`MAC_JOB`] (a job named `macos-arm64` among them: a pull
//!   request's run of artifacts.yml has a skipped job of that name, which a check of that name
//!   would pass on);
//! - `version` does not run on ubuntu, under no `if:`, exactly [`version_commands`] (artifacts.yml's
//!   version, unrecorded), with no action but the checkout, and hand the version on as its output;
//! - [`MAC_JOB`] does not need `version` and run under no `if:` on the runner of the macOS release
//!   build, with its `TARGET` and the version `version` gives, exactly [`pr_mac_commands`] (the
//!   commands of artifacts.yml's macOS build job, with the tests of the whole workspace for the
//!   tests of the binary's crates), with no action but the checkout and a cache restore of
//!   artifacts.yml's macOS cache ([`CACHE_KEY`], [`CACHE_RESTORE_KEYS`]: it uploads, saves and
//!   signs nothing), a checkout that keeps no token (`persist-credentials: false`), and with no
//!   `if:`, `env` or `continue-on-error` on a step;
//! - the cargo-cyclonedx pins of its `env` are not artifacts.yml's.
//!
//! [`pull_request_job_problems`] refuses a job a pull request runs on macOS that holds anything
//! but `contents: read`, names an environment, or names a secret or the token anywhere in it or in
//! its workflow's `env` (which every job sees). `platforms::pull_request_macos_problems` applies
//! it to every workflow, and refuses a job a pull request runs on macOS other than [`MAC_JOB`] of
//! `macos.yml`.

use super::lint::{entries, entry, names};
use super::platforms::{
    BUILDS, MAC_TESTS, action_problems, at, build_commands, command_problems, commands,
    execution_problems, steps, using,
};
use super::yaml::{self, Node, Value};

/// The workflow of the macOS checks of a pull request.
pub const MACOS_PR: &str = "macos.yml";

/// The one group of a pull request's runs.
pub const GROUP: &str = "macos-${{ github.event.pull_request.number }}";

/// The macOS job of `macos.yml`: not `macos-arm64`, the name of artifacts.yml's macOS job, which a
/// pull request's run of artifacts.yml skips.
pub const MAC_JOB: &str = "macos";

/// The key of the cache the macOS job restores: artifacts.yml's macOS cache, its `${{ github.job }}`
/// spelled out as that job's name.
pub const CACHE_KEY: &str = "cargo-${{ runner.os }}-${{ runner.arch }}-macos-arm64-${{ hashFiles('Cargo.lock', 'rust-toolchain.toml') }}";

/// The restore keys of the cache the macOS job restores, likewise artifacts.yml's.
pub const CACHE_RESTORE_KEYS: &str = "cargo-${{ runner.os }}-${{ runner.arch }}-macos-arm64-";

/// The tests of the macOS job: every test of the workspace, none left out.
pub const PR_MAC_TESTS: &str =
    "cargo test --locked --workspace --features tierstore/s3 --no-fail-fast";

/// The jobs of `macos.yml`, sorted.
pub const JOBS: [&str; 2] = [MAC_JOB, "version"];

/// The variables of the file's `env` that pin cargo-cyclonedx, which must be artifacts.yml's.
const PINS: [&str; 2] = ["CYCLONEDX_VERSION", "CYCLONEDX_SHA256_AARCH64_MACOS"];

/// The `run:` commands of `version`, in order.
pub fn version_commands() -> Vec<String> {
    vec![
        "rustup toolchain install".to_owned(),
        "cargo xtask --help".to_owned(),
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
/// `secrets: inherit`), `github.token` or `GITHUB_TOKEN`, or the `github` context indexed or
/// whole (`github['token']`, `toJSON(github)`), which holds the token.
fn names_a_secret(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    [
        "secrets",
        "github.token",
        "github_token",
        "github[",
        "tojson(github",
    ]
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
    problems.extend(version_problems(job("version")));
    problems.extend(mac_job_problems(job(MAC_JOB), entry(&doc, "env")));
    problems
}

/// One line per way `version` is not the version of the build.
fn version_problems(job: Option<&Node>) -> Vec<String> {
    let Some(version) = job else {
        return vec![format!("{MACOS_PR}: no job version")];
    };
    let mut problems = Vec::new();
    if !at(version, &["runs-on"]).is_some_and(|r| r.starts_with("ubuntu-")) {
        problems.push("version: does not run on an ubuntu runner".to_owned());
    }
    problems.extend(execution_problems("version", version, None));
    problems.extend(command_problems(
        "version",
        &commands(job),
        &version_commands(),
    ));
    problems.extend(action_problems("version", job, &["actions/checkout@"]));
    let ids: Vec<Option<&str>> = steps(job).iter().map(|s| at(s, &["id"])).collect();
    let id = steps(job)
        .iter()
        .find(|s| at(s, &["run"]).is_some_and(|r| r.contains("release version")))
        .and_then(|s| at(s, &["id"]));
    if id != Some("version") || ids.iter().flatten().count() != 1 {
        problems.push(format!(
            "version: the ids of its steps are {ids:?}, not `version` on the release version step alone"
        ));
    }
    let output = at(version, &["outputs", "version"]);
    let want = "${{ steps.version.outputs.version }}";
    if output != Some(want) {
        problems.push(format!("version: output version is {output:?}, not {want}"));
    }
    problems
}

/// One line per way [`MAC_JOB`] is not the macOS build of artifacts.yml with the workspace's
/// tests, on every pull request, holding nothing.
fn mac_job_problems(job: Option<&Node>, workflow_env: Option<&Node>) -> Vec<String> {
    let Some(mac) = job else {
        return vec![format!("{MACOS_PR}: no job {MAC_JOB}")];
    };
    let b = mac_build();
    let name = MAC_JOB;
    let mut problems = Vec::new();
    let needs = names(entry(mac, "needs"));
    if needs != ["version"] {
        problems.push(format!("{name}: needs are {needs:?}, not [\"version\"]"));
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
    let version = "${{ needs.version.outputs.version }}";
    if at(mac, &["env", "RELEASE_VERSION"]) != Some(version) {
        problems.push(format!("{name}: env RELEASE_VERSION is not {version}"));
    }
    problems.extend(execution_problems(name, mac, None));
    let restores = using(job, "actions/cache/restore@");
    let keys: Vec<(Option<&str>, Option<&str>)> = restores
        .iter()
        .map(|s| (at(s, &["with", "key"]), at(s, &["with", "restore-keys"])))
        .collect();
    if keys != [(Some(CACHE_KEY), Some(CACHE_RESTORE_KEYS))] {
        problems.push(format!(
            "{name}: its cache restores are {keys:?}, not the one of artifacts.yml's macOS cache ({CACHE_KEY}, {CACHE_RESTORE_KEYS})"
        ));
    }
    let keeps_token = steps(job).iter().any(|s| {
        at(s, &["uses"]).is_some_and(|u| u.starts_with("actions/checkout@"))
            && at(s, &["with", "persist-credentials"]) != Some("false")
    });
    if keeps_token {
        problems.push(format!(
            "{name}: a checkout keeps the token in .git/config (persist-credentials is not false)"
        ));
    }
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
