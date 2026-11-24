//! The macOS release files built on a Mac of the build farm (docs/mac-farm.md): artifacts.yml's
//! job [`CALLER`], which calls [`FILE`], as the workflow lint holds them.
//!
//! [`caller_problems`] refuses `artifacts.yml` when [`CALLER`] is not exactly a call of [`CALL`]
//! that needs `version` alone, runs under [`FARM_IF`] (a push to main once the variable
//! `MACOS_PACKAGE_ON_FARM` is `true`), holds `contents: read` and `id-token: write` and nothing
//! else, hands on the version and nothing else (no `secrets`), and when the hosted `macos-arm64`
//! does not run under [`HOSTED_IF`], the negation of the farm's condition for a push: on every run
//! exactly one of the two can run.
//!
//! [`problems`] refuses [`FILE`] when:
//! - its `on` is anything but `workflow_call` with the one input `version`, a required string;
//! - its jobs are anything but [`JOB`], on `ubuntu-24.04`, under [`SIGN_IF`], with `permissions`
//!   exactly `contents: read` and `id-token: write` (the tailnet join), `env` exactly
//!   `RELEASE_VERSION` from the input, no `continue-on-error`, and no step with an `if:`;
//! - its `run:` commands are not exactly [`COMMANDS`]: the toolchain, the build of xtask, and the
//!   one `mac-farm` command, which sends `release macos-assets` with the run's version and commit,
//!   cargo-cyclonedx by URL and SHA-256, and `dist` and `pkg` as the outputs;
//! - it uses an action other than checkout, the tailnet join (once, after the build of xtask, so no
//!   build script sees the farm's network, and before `mac-farm`; its `with:` exactly
//!   [`JOIN_INPUTS`]) and the upload, or its uploads are not exactly the two artifacts of the
//!   hosted job, from the same directories;
//! - the `mac-farm` step's `env` is not exactly [`FARM_ENV`], the organisation's variables;
//! - its cargo-cyclonedx pin (`CYCLONEDX_VERSION`, `CYCLONEDX_SHA256_AARCH64_MACOS` of the file's
//!   `env`) is not artifacts.yml's;
//! - it names a secret.
//!
//! [`assets_step_problems`] refuses a `release macos-assets` whose steps are not the hosted job's
//! commands, the same arguments in the same order (the runner's own steps aside: the toolchain and
//! the download of cargo-cyclonedx, which on the farm is an input of the action).

use super::lint::{Allowed, SIGN_IF, entries, entry, names, scalars};
use super::platforms::{BUILDS, UPLOAD, action_problems, at, build_commands, commands, steps};
use super::yaml::Node;

/// The called workflow.
pub const FILE: &str = "macos-farm.yml";
/// Its one job.
pub const JOB: &str = "package";
/// The job of artifacts.yml that calls it.
pub const CALLER: &str = "macos-arm64-farm";
/// The `uses:` of [`CALLER`]: the workflow of the same commit.
pub const CALL: &str = "./.github/workflows/macos-farm.yml";

/// The `if:` of [`CALLER`]: a push to main, once the variable turns the farm build on.
pub const FARM_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && vars.MACOS_PACKAGE_ON_FARM == 'true' }}";

/// The write scopes of the farm build, beside [`super::lint::WRITE_ALLOWED`]: `id-token` for the
/// tailnet join, held by [`CALLER`] (which grants it to the workflow it calls) and [`JOB`], each
/// under its exact condition, a push to main.
pub const WRITE_ALLOWED: &[Allowed] = &[
    Allowed {
        file: "artifacts.yml",
        job: CALLER,
        scope: "id-token",
        condition: FARM_IF,
    },
    Allowed {
        file: FILE,
        job: JOB,
        scope: "id-token",
        condition: SIGN_IF,
    },
];

/// The `if:` of the hosted `macos-arm64`: not for a pull request, and not when [`FARM_IF`] holds.
pub const HOSTED_IF: &str = "${{ github.event_name != 'pull_request' && !(github.event_name == 'push' && github.ref == 'refs/heads/main' && vars.MACOS_PACKAGE_ON_FARM == 'true') }}";

/// The `if:` of `sign-pkg`: a push to main, after whichever of the two macOS jobs ran and passed.
pub const SIGN_PKG_IF: &str = "${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && (needs.macos-arm64.result == 'success' || needs.macos-arm64-farm.result == 'success') }}";

/// The tailnet join and the `with:` of its step, exactly.
pub const JOIN: &str = "tailscale/github-action@";
pub const JOIN_INPUTS: [(&str, &str); 3] = [
    ("oauth-client-id", "${{ vars.TS_CLIENT_ID }}"),
    ("audience", "${{ vars.TS_AUDIENCE }}"),
    ("tags", "tag:ci"),
];

/// The `env` of the `mac-farm` step, exactly: the farm, from the organisation's variables.
pub const FARM_ENV: [(&str, &str); 4] = [
    ("FARM_ADDRESS", "${{ vars.FARM_ADDRESS }}"),
    ("FARM_INSTANCE", "${{ vars.FARM_INSTANCE }}"),
    ("FARM_TLS", "${{ vars.FARM_TLS || 'false' }}"),
    (
        "FARM_PROPERTIES",
        "${{ vars.FARM_DARWIN_ARM64_PROPERTIES }}",
    ),
];

/// The scratch directory `release macos-assets` is given on the Mac, in the action's checkout.
pub const MAC_SCRATCH: &str = "target/macos-assets";

/// The one `mac-farm` command.
pub const MAC_FARM_RUN: &str = "cargo xtask mac-farm --address \"$FARM_ADDRESS\" --instance \"$FARM_INSTANCE\" --tls \"$FARM_TLS\" --properties \"$FARM_PROPERTIES\" --scratch \"$RUNNER_TEMP/mac-farm\" --tool-url \"https://github.com/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-$CYCLONEDX_VERSION/cargo-cyclonedx-aarch64-apple-darwin.tar.xz\" --tool-sha256 \"$CYCLONEDX_SHA256_AARCH64_MACOS\" --tool-member cargo-cyclonedx-aarch64-apple-darwin/cargo-cyclonedx --output dist --output pkg -- xtask release macos-assets --version \"$RELEASE_VERSION\" --commit \"$GITHUB_SHA\" --scratch target/macos-assets";

/// The `run:` commands of [`JOB`], in order.
pub const COMMANDS: [&str; 3] = [
    "rustup toolchain install",
    "cargo xtask --help",
    MAC_FARM_RUN,
];

/// The artifacts [`JOB`] uploads, as the hosted `macos-arm64` does: `(name, path)`.
pub const UPLOADS: [(&str, &str); 2] = [
    ("pkg-unsigned-macos-arm64", "pkg/"),
    ("assets-macos-arm64", "dist/"),
];

/// The keys of the cargo-cyclonedx pin the two files share.
pub const PINS: [&str; 2] = ["CYCLONEDX_VERSION", "CYCLONEDX_SHA256_AARCH64_MACOS"];

fn pairs(node: Option<&Node>) -> Vec<(&str, &str)> {
    node.map_or(&[][..], entries)
        .iter()
        .filter_map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
        .collect()
}

/// One line per way the jobs `jobs` of artifacts.yml do not call [`FILE`] as the module says.
pub fn caller_problems(jobs: &[(Node, Node)]) -> Vec<String> {
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut problems = Vec::new();
    match job("macos-arm64").and_then(|j| at(j, &["if"])) {
        Some(HOSTED_IF) => {}
        other => problems.push(format!(
            "macos-arm64: runs under `if: {other:?}`, not {HOSTED_IF}, so it and {CALLER} could both run or neither"
        )),
    }
    let Some(caller) = job(CALLER) else {
        problems.push(format!("{CALLER}: no such job"));
        return problems;
    };
    if at(caller, &["uses"]) != Some(CALL) {
        problems.push(format!("{CALLER}: does not call {CALL}"));
    }
    if names(entry(caller, "needs")) != ["version"] {
        problems.push(format!("{CALLER}: does not need exactly [\"version\"]"));
    }
    if at(caller, &["if"]) != Some(FARM_IF) {
        problems.push(format!("{CALLER}: does not run under `if: {FARM_IF}`"));
    }
    if pairs(entry(caller, "permissions")) != [("contents", "read"), ("id-token", "write")] {
        problems.push(format!(
            "{CALLER}: permissions are not exactly `contents: read` and `id-token: write`"
        ));
    }
    if pairs(entry(caller, "with")) != [("version", "${{ needs.version.outputs.version }}")] {
        problems.push(format!(
            "{CALLER}: does not hand on exactly the version of the `version` job"
        ));
    }
    let keys: Vec<&str> = entries(caller)
        .iter()
        .filter_map(|(k, _)| k.as_str())
        .collect();
    for key in keys {
        if !["needs", "if", "permissions", "uses", "with"].contains(&key) {
            problems.push(format!(
                "{CALLER}: has `{key}`, which a call of {CALL} does not"
            ));
        }
    }
    problems
}

/// One line per way the workflow `doc` ([`FILE`]) is not as the module says, with `artifacts`
/// artifacts.yml's document (whose cargo-cyclonedx pin it shares).
pub fn problems(doc: &Node, artifacts: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    let on = entry(doc, "on");
    let triggers = names(on);
    let inputs = on
        .and_then(|o| entry(o, "workflow_call"))
        .and_then(|c| entry(c, "inputs"));
    let input_names = names(inputs);
    let version = inputs.and_then(|i| entry(i, "version"));
    let typed = version.is_some_and(|v| {
        at(v, &["type"]) == Some("string") && at(v, &["required"]) == Some("true")
    });
    if triggers != ["workflow_call"] || input_names != ["version"] || !typed {
        problems.push(format!(
            "{FILE}: `on` is not exactly `workflow_call` with the one required string input `version`"
        ));
    }
    for key in PINS {
        if at(doc, &["env", key]).is_none()
            || at(doc, &["env", key]) != at(artifacts, &["env", key])
        {
            problems.push(format!("{FILE}: env {key} is not artifacts.yml's"));
        }
    }
    let mut found = Vec::new();
    scalars(doc, &mut found);
    if found.iter().any(|s| s.contains("secrets.")) {
        problems.push(format!("{FILE}: reads a secret"));
    }
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let job_names: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    if job_names != [JOB] {
        problems.push(format!("{FILE}: its jobs are {job_names:?}, not [{JOB:?}]"));
    }
    let Some((_, job)) = jobs.iter().find(|(k, _)| k.as_str() == Some(JOB)) else {
        return problems;
    };
    problems.extend(job_problems(job));
    problems
}

fn job_problems(job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    let name = format!("{FILE} {JOB}");
    if at(job, &["runs-on"]) != Some("ubuntu-24.04") {
        problems.push(format!("{name}: does not run on ubuntu-24.04"));
    }
    if at(job, &["if"]) != Some(SIGN_IF) {
        problems.push(format!("{name}: does not run under `if: {SIGN_IF}`"));
    }
    if pairs(entry(job, "permissions")) != [("contents", "read"), ("id-token", "write")] {
        problems.push(format!(
            "{name}: permissions are not exactly `contents: read` and `id-token: write`"
        ));
    }
    if pairs(entry(job, "env")) != [("RELEASE_VERSION", "${{ inputs.version }}")] {
        problems.push(format!(
            "{name}: env is not exactly RELEASE_VERSION from the input"
        ));
    }
    if entry(job, "continue-on-error").is_some() {
        problems.push(format!("{name}: has continue-on-error"));
    }
    let all = steps(Some(job));
    for step in all {
        if entry(step, "if").is_some() || entry(step, "continue-on-error").is_some() {
            let label = at(step, &["name"])
                .or_else(|| at(step, &["uses"]))
                .or_else(|| at(step, &["run"]))
                .unwrap_or_default();
            problems.push(format!(
                "{name}: the step `{label}` has an `if:` or continue-on-error"
            ));
        }
    }
    let got = commands(Some(job));
    let want: Vec<String> = COMMANDS.map(str::to_owned).to_vec();
    problems.extend(super::platforms::command_problems(&name, &got, &want));
    problems.extend(action_problems(
        &name,
        Some(job),
        &["actions/checkout@", JOIN, UPLOAD],
    ));
    let position = |test: &dyn Fn(&Node) -> bool| all.iter().position(test);
    let joins: Vec<&Node> = all
        .iter()
        .filter(|s| at(s, &["uses"]).is_some_and(|u| u.starts_with(JOIN)))
        .collect();
    if joins.len() != 1 || pairs(joins.first().and_then(|j| entry(j, "with"))) != JOIN_INPUTS {
        problems.push(format!(
            "{name}: does not join the tailnet exactly once with exactly {JOIN_INPUTS:?}"
        ));
    }
    let join = position(&|s| at(s, &["uses"]).is_some_and(|u| u.starts_with(JOIN)));
    let build = position(&|s| at(s, &["run"]).map(str::trim_end) == Some(COMMANDS[1]));
    let farm = position(&|s| at(s, &["run"]).map(str::trim_end) == Some(MAC_FARM_RUN));
    match (build, join, farm) {
        (Some(b), Some(j), Some(f)) if b < j && j < f => {}
        _ => problems.push(format!(
            "{name}: does not build xtask, then join the tailnet, then run mac-farm"
        )),
    }
    if let Some(f) = farm
        && pairs(entry(&all[f], "env")) != FARM_ENV
    {
        problems.push(format!(
            "{name}: the mac-farm step's env is not exactly {FARM_ENV:?}"
        ));
    }
    let uploads: Vec<(&str, &str)> = all
        .iter()
        .filter(|s| at(s, &["uses"]).is_some_and(|u| u.starts_with(UPLOAD)))
        .map(|s| {
            (
                at(s, &["with", "name"]).unwrap_or_default(),
                at(s, &["with", "path"]).unwrap_or_default(),
            )
        })
        .collect();
    let checked = all
        .iter()
        .filter(|s| at(s, &["uses"]).is_some_and(|u| u.starts_with(UPLOAD)))
        .all(|s| at(s, &["with", "if-no-files-found"]) == Some("error"));
    if uploads != UPLOADS || !checked {
        problems.push(format!(
            "{name}: uploads {uploads:?}, not exactly {UPLOADS:?}, each with if-no-files-found: error"
        ));
    }
    problems
}

/// The hosted job's command `command` as `release macos-assets` says it on the farm: the
/// variables of the job given the values `version`, `commit` and `scratch`, the quotes gone.
pub fn as_on_the_farm(command: &str, version: &str, commit: &str, scratch: &str) -> String {
    command
        .replace("${GITHUB_SHA::12}", &commit[..12])
        .replace("${GITHUB_SHA}", commit)
        .replace("$GITHUB_SHA", commit)
        .replace("${RELEASE_VERSION}", version)
        .replace("$RELEASE_VERSION", version)
        .replace("$RUNNER_TEMP", scratch)
        .replace("$TARGET", xtask::release::macos_assets::TARGET)
        .replace('"', "")
}

/// The hosted job's commands that are the runner's own and not steps of `release macos-assets`:
/// the toolchain, and the download of cargo-cyclonedx (an input of the farm action).
fn runner_only(command: &str) -> bool {
    command == "rustup toolchain install"
        || command.starts_with("cargo xtask fetch --url \"https://github.com/CycloneDX/")
}

/// One line per way the steps `got` (each as `release macos-assets` shows it) are not the commands
/// `hosted` of the hosted macOS job, for `version`, `commit` and `scratch`.
pub fn assets_step_problems(
    got: &[String],
    hosted: &[String],
    version: &str,
    commit: &str,
    scratch: &str,
) -> Vec<String> {
    let want: Vec<String> = hosted
        .iter()
        .filter(|c| !runner_only(c))
        .map(|c| as_on_the_farm(c, version, commit, scratch))
        .collect();
    let got: Vec<&str> = got.iter().map(String::as_str).collect();
    super::platforms::command_problems("release macos-assets", &got, &want)
}

/// The commands of the hosted macOS job, as the lint pins them ([`build_commands`]).
pub fn hosted_commands() -> Vec<String> {
    let mac = BUILDS
        .iter()
        .find(|b| b.platform == "macos-arm64")
        .expect("the macOS build");
    build_commands(mac)
}

/// Whether `value` is the one `uses:` of a local path the lint lets a workflow name: [`CALL`] in
/// artifacts.yml.
pub fn local_call_allowed(file: &str, value: &str) -> bool {
    file == "artifacts.yml" && value == CALL
}

/// Whether the job `job` is [`CALLER`] as it must be for the pull-request rule: a call of
/// [`CALL`] under [`FARM_IF`], which is never true for a pull request, of a workflow whose one
/// job [`problems`] holds to `ubuntu-24.04`.
pub fn is_the_farm_call(name: &str, job: &Node) -> bool {
    name == CALLER && at(job, &["uses"]) == Some(CALL) && at(job, &["if"]) == Some(FARM_IF)
}
