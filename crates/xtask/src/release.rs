//! `cargo xtask release`: a GitHub release of the exact files a push to `main` built and
//! attested for one commit. Nothing is rebuilt; `.github/workflows/release.yml` runs these
//! verbs, in this order, and it is started by hand.
//!
//! - `release locate --repo <owner/repo> --commit <sha> [--output <file>] [--gh <program>]`
//!   ([`locate`]): refuses a commit that is not on `main` (GitHub's compare of
//!   `main...<sha>` is neither `identical` nor `behind`), then a commit for which there is no
//!   successful `push` run on `main` of each of [`BUILD_WORKFLOW`] (the build and the signing)
//!   and [`CI_WORKFLOW`] (the lint and the tests), then a build run that no longer holds every
//!   artifact a release needs ([`ARTIFACTS`]). Prints `run-id=<id>` of the build run and
//!   appends it to `--output` (on Actions, `GITHUB_OUTPUT`). So a commit that failed its tests
//!   is never released, though the signing does not wait for them.
//! - `release check <dir> --commit <sha>` ([`check`]): `<dir>` passes `verify-assets sums`,
//!   `SHA256SUMS` lists exactly the assets of that commit ([`expected_assets`]) and no other,
//!   and each tarball has exactly one provenance bundle whose one subject is that
//!   tarball's digest ([`crate::bundles`]), and no bundle is there for anything else.
//! - `release publish <dir> --repo <owner/repo> --tag <tag> --commit <sha> --run-id <id>
//!   --notes-file <file> [--note <text>]... [--gh <program>]` ([`publish`]): `check`, then
//!   creates release `<tag>` at `<sha>` as a draft with every file of `<dir>`, reads the
//!   draft's assets back and refuses unless they are exactly those files, and only then
//!   publishes it (which is when GitHub creates the tag). A tag or a published release that
//!   exists is refused; a draft a failed run left is replaced.

mod publish;

pub use publish::{Gh, RealGh, publish};

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use crate::Error;
use crate::build_commit::full_commit;
use crate::bundles;
use crate::fetch::Sha256;
use crate::package::{Platform, asset_stem};
use crate::sums::{self, SUMS};

/// The branch a released commit must be on.
pub const MAIN: &str = "main";

/// The binary a release ships.
pub const BINARY: &str = "tierstore";

/// The workflow that builds and signs the assets: the signer a deployment names.
pub const BUILD_WORKFLOW: &str = "artifacts.yml";

/// The workflow that lints and tests: a release needs a green run of it for the commit.
pub const CI_WORKFLOW: &str = "ci.yml";

/// The artifacts a push run of [`BUILD_WORKFLOW`] leaves, all of which a release needs: the
/// files of each platform, the sums, and the provenance bundles.
pub const ARTIFACTS: [&str; 4] = [
    "assets-linux-x86_64",
    "assets-linux-arm64",
    "sha256sums",
    "bundles-linux",
];

/// The files `SHA256SUMS` lists for `commit`, sorted: for each platform the tarball and the
/// SBOM of [`BINARY`].
pub fn expected_assets(commit: &str) -> Vec<String> {
    let mut names = Vec::new();
    for platform in Platform::ALL {
        let stem = asset_stem(BINARY, commit, platform);
        names.push(format!("{stem}.tar.gz"));
        names.push(format!("{stem}.cdx.json"));
    }
    names.sort();
    names
}

/// The `release check` verb: the lines to print.
pub fn check(dir: &Path, commit: &str) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    let mut printed = vec![sums::verify_sums(dir, Sha256::of)?];
    let lines = sums::read_sums(&dir.join(SUMS))?;
    let mut problems = Vec::new();
    let listed: Vec<&str> = lines.iter().map(|l| l.name.as_str()).collect();
    let expected = expected_assets(commit);
    for name in &expected {
        if !listed.contains(&name.as_str()) {
            problems.push(format!("{SUMS} does not list {name}"));
        }
    }
    for name in &listed {
        if !expected.iter().any(|e| e == name) {
            problems.push(format!(
                "{SUMS} lists {name}, which is not an asset of {commit}"
            ));
        }
    }
    problems.extend(bundles::pairing(dir, &lines)?);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    printed.push(format!(
        "release check: {} files, the assets of {commit}, each tarball with its bundle",
        lines.len()
    ));
    Ok(printed)
}

/// Whether `commit` is on `main`: `main` holds it (compared, `main` is identical to it or
/// ahead of it). An unknown commit is not.
fn on_main(gh: &dyn Gh, repo: &str, commit: &str) -> Result<bool, Error> {
    let compared = gh.api(&format!("repos/{repo}/compare/{MAIN}...{commit}"))?;
    let status = compared.as_ref().and_then(|v| v["status"].as_str());
    Ok(matches!(status, Some("identical" | "behind")))
}

/// The id of the successful push run on `main` of `workflow` for `commit`, if there is one.
fn push_run(gh: &dyn Gh, repo: &str, workflow: &str, commit: &str) -> Result<Option<u64>, Error> {
    let path = format!(
        "repos/{repo}/actions/workflows/{workflow}/runs?head_sha={commit}&event=push&branch={MAIN}&status=success&per_page=20"
    );
    let runs = gh.api(&path)?;
    let found = runs
        .as_ref()
        .and_then(|v| v["workflow_runs"].as_array())
        .into_iter()
        .flatten()
        .find(|r| {
            r["head_sha"] == commit
                && r["head_branch"] == MAIN
                && r["event"] == "push"
                && r["conclusion"] == "success"
        })
        .and_then(|r| r["id"].as_u64());
    Ok(found)
}

/// The `release locate` verb: the lines to print.
pub fn locate(
    gh: &dyn Gh,
    repo: &str,
    commit: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !on_main(gh, repo, commit)? {
        return Err(Error::Failed(format!(
            "{commit} is not on {MAIN}: a release is cut only from a commit on {MAIN}"
        )));
    }
    let mut problems = Vec::new();
    let build = push_run(gh, repo, BUILD_WORKFLOW, commit)?;
    if build.is_none() {
        problems.push(format!(
            "no successful push run of {BUILD_WORKFLOW} on {MAIN} for {commit}"
        ));
    }
    if push_run(gh, repo, CI_WORKFLOW, commit)?.is_none() {
        problems.push(format!(
            "no successful push run of {CI_WORKFLOW} on {MAIN} for {commit}"
        ));
    }
    let Some(id) = build.filter(|_| problems.is_empty()) else {
        return Err(Error::Refused(problems));
    };
    let artifacts = gh.api(&format!(
        "repos/{repo}/actions/runs/{id}/artifacts?per_page=100"
    ))?;
    let kept: Vec<&str> = artifacts
        .as_ref()
        .and_then(|v| v["artifacts"].as_array())
        .into_iter()
        .flatten()
        .filter(|a| a["expired"] == false)
        .filter_map(|a| a["name"].as_str())
        .collect();
    let missing: Vec<String> = ARTIFACTS
        .iter()
        .filter(|a| !kept.contains(*a))
        .map(|a| format!("run {id} holds no unexpired artifact {a}"))
        .collect();
    if !missing.is_empty() {
        return Err(Error::Refused(missing));
    }
    let line = format!("run-id={id}");
    if let Some(output) = output {
        append_output(output, std::slice::from_ref(&line))?;
    }
    Ok(vec![
        format!("{commit} is on {MAIN}; run {id} built it and its tests passed"),
        line,
    ])
}

/// Appends `lines` to `output` (on Actions, `GITHUB_OUTPUT`), one per line.
fn append_output(output: &Path, lines: &[String]) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .and_then(|mut f| writeln!(f, "{}", lines.join("\n")))
        .map_err(Error::spawn(format!("append to {}", output.display())))
}

/// `value` as an array of objects, or none.
pub(crate) fn array(value: &Option<Value>) -> &[Value] {
    value
        .as_ref()
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

#[cfg(test)]
mod tests;
