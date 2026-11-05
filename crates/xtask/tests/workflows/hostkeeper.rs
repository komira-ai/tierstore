//! The release index of the hostkeeper packages in the workflows: the `sums` job of
//! `artifacts.yml` writes it before it assembles the sums and hands it, with the sums, to the
//! signing job, and every job that checks a release makes it again from the packages and takes
//! each package from it as a host does. (The build jobs' commands, which make and unpack each
//! package, and the signing job's, which sign each package and name its bundle, are pinned with
//! the rest of those jobs' commands in [`super::platforms`].)
//!
//! [`artifacts_index_problems`] refuses `artifacts.yml` when:
//! - `sums` does not run [`SUMS_INDEX`] as its step `index` before it assembles the sums (the
//!   sums would not list the index), does not hand the step's `package-version` on as its own
//!   output (the signing job names the packages by it), or does not upload `index.json` with
//!   the sums (the signing job checks every listed file before it signs);
//! - `attest` does not need `sums`;
//! - `verify` does not make the index again ([`regenerate`]) before it assembles the sums it
//!   holds to the published ones, or does not take each package from it ([`verify_index`]) after
//!   it checks the set of the release.
//!
//! [`release_index_problems`] refuses `release.yml` when its `verify` job does not make the index
//! again before it assembles the sums and take each package from it, or its `publish` job does not
//! take each package from it before it publishes.

use super::lint::{entries, entry, names, runs};
use super::yaml::{self, Node, Value};

/// The step of `sums` that writes the index: every package of the run, the event (for a pull
/// request no index is written), and the version of the packages to the job's outputs.
pub const SUMS_INDEX: &str = "cargo xtask release hostkeeper-index dist --commit \"$GITHUB_SHA\" --event \"$GITHUB_EVENT_NAME\" --output \"$GITHUB_OUTPUT\"";

/// The output of `sums` the signing job names the packages by.
pub const PACKAGE_VERSION: &str = "${{ steps.index.outputs.package-version }}";

/// What the upload of the sums takes: the sums and the index.
pub const SUMS_PATHS: &str = "dist/SHA256SUMS\ndist/index.json\n";

/// The command that makes the index again from the packages of the commit `commit` (a shell
/// variable).
pub fn regenerate(commit: &str) -> String {
    format!(
        "cargo xtask release hostkeeper-index dist --commit \"{commit}\" --event \"$GITHUB_EVENT_NAME\""
    )
}

/// The command that takes each package of the commit `commit` from the index, as a host does.
pub fn verify_index(commit: &str) -> String {
    format!(
        "cargo xtask verify-assets hostkeeper-index dist --commit \"{commit}\" --scratch \"$RUNNER_TEMP/hostkeeper\""
    )
}

/// The jobs of the workflow `doc`.
fn jobs(doc: &Node) -> &[(Node, Node)] {
    entry(doc, "jobs").map_or(&[][..], entries)
}

/// The job `name` of `doc`.
fn job<'a>(doc: &'a Node, name: &str) -> Option<&'a Node> {
    jobs(doc)
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// The position among the `run:` commands of `job` of the first that is `command`, or that
/// starts with it when `prefix`.
fn position(job: Option<&Node>, command: &str, prefix: bool) -> Option<usize> {
    runs(job).iter().map(|r| r.trim_end()).position(|r| {
        if prefix {
            r.starts_with(command)
        } else {
            r == command
        }
    })
}

/// One line per way the job `name` does not run `first` before the command that starts with
/// `then`, both there.
fn order_problems(name: &str, job: Option<&Node>, first: &str, then: &str) -> Vec<String> {
    let at = position(job, first, false);
    let after = position(job, then, true);
    match (at, after) {
        (None, _) => vec![format!("{name}: does not run `{first}`")],
        (_, None) => vec![format!("{name}: does not run `{then}...`")],
        (Some(at), Some(after)) if at > after => {
            vec![format!("{name}: runs `{first}` after `{then}...`")]
        }
        _ => Vec::new(),
    }
}

/// One line per way the job `name` does not run `command`.
fn runs_problems(name: &str, job: Option<&Node>, command: &str) -> Vec<String> {
    match position(job, command, false) {
        Some(_) => Vec::new(),
        None => vec![format!("{name}: does not run `{command}`")],
    }
}

/// The steps of `job`.
pub fn steps(job: Option<&Node>) -> &[Node] {
    match job.and_then(|j| entry(j, "steps")).map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    }
}

/// The text at `path` (keys, one level each) under `node`.
pub fn at<'a>(node: &'a Node, path: &[&str]) -> Option<&'a str> {
    let mut here = node;
    for key in path {
        here = entry(here, key)?;
    }
    here.as_str()
}

/// One line per way `artifacts.yml` (`text`) does not write the index in `sums`, hand it and the
/// packages' version to `attest`, and make it again and check each package from it in `verify`.
pub fn artifacts_index_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = Vec::new();
    let sums = job(&doc, "sums");
    problems.extend(order_problems(
        "sums",
        sums,
        SUMS_INDEX,
        "cargo xtask sums assemble dist",
    ));
    let index_step = steps(sums)
        .iter()
        .find(|s| at(s, &["run"]).is_some_and(|r| r.trim_end() == SUMS_INDEX));
    if index_step.and_then(|s| at(s, &["id"])) != Some("index") {
        problems.push(format!("sums: the step `{SUMS_INDEX}` is not `id: index`"));
    }
    let output = sums.and_then(|j| at(j, &["outputs", "package-version"]));
    if output != Some(PACKAGE_VERSION) {
        problems.push(format!(
            "sums: output package-version is {output:?}, not {PACKAGE_VERSION}"
        ));
    }
    let uploaded = steps(sums)
        .iter()
        .find(|s| at(s, &["with", "name"]) == Some("sha256sums"))
        .and_then(|s| at(s, &["with", "path"]));
    if uploaded != Some(SUMS_PATHS) {
        problems.push(format!(
            "sums: the sha256sums upload takes {uploaded:?}, not {SUMS_PATHS:?}"
        ));
    }
    let needs = names(job(&doc, "attest").and_then(|j| entry(j, "needs")));
    if !needs.iter().any(|n| n == "sums") {
        problems.push("attest: does not need sums, whose outputs it reads".to_owned());
    }
    let verify = job(&doc, "verify");
    problems.extend(order_problems(
        "verify",
        verify,
        &regenerate("$GITHUB_SHA"),
        "cargo xtask sums assemble dist --expect",
    ));
    problems.extend(order_problems(
        "verify",
        verify,
        "cargo xtask release check dist --commit \"$GITHUB_SHA\"",
        &verify_index("$GITHUB_SHA"),
    ));
    problems
}

/// One line per way `release.yml` (`text`) does not make the index again in `verify` and check
/// each package from it there and in `publish`.
pub fn release_index_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = Vec::new();
    let verify = job(&doc, "verify");
    problems.extend(order_problems(
        "release.yml verify",
        verify,
        &regenerate("$COMMIT"),
        "cargo xtask sums assemble dist --expect",
    ));
    problems.extend(runs_problems(
        "release.yml verify",
        verify,
        &verify_index("$COMMIT"),
    ));
    problems.extend(order_problems(
        "release.yml publish",
        job(&doc, "publish"),
        &verify_index("$COMMIT"),
        "cargo xtask release publish",
    ));
    problems
}
