//! `cargo xtask release`: a GitHub release of the exact files a push to `main` built and
//! attested for one commit. Nothing is rebuilt; `.github/workflows/release.yml` runs these
//! verbs, in this order. It starts when `ci.yml` and `artifacts.yml` have finished on a push to
//! `main`, or by hand.
//!
//! - `release plan` and `release manual` ([`plan()`]): which commit of `main` is released and as
//!   which tag (the first job); see that module.
//! - `release set-version --version <X.Y.Z>` ([`set_version`]): writes the version into the manifest
//!   of `tierstore` and `Cargo.lock`, which the build jobs of `artifacts.yml` run before they build.
//! - `release locate --repo <owner/repo> --commit <sha> [--output <file>] [--gh <program>]`
//!   ([`locate`]): refuses a commit that is not on `main` (GitHub's compare of
//!   `main...<sha>` is neither `identical` nor `behind`), then a commit for which there is no
//!   successful `push` run on `main` of each of [`BUILD_WORKFLOW`] (the build and the signing)
//!   and [`CI_WORKFLOW`] (the lint and the tests), then a build run that no longer holds every
//!   artifact a release needs ([`ARTIFACTS`]). Prints `run-id=<id>` of the build run and
//!   appends it to `--output` (on Actions, `GITHUB_OUTPUT`). So a commit that failed its tests
//!   is never released, though the signing does not wait for them.
//! - `release check <dir> --commit <sha>` ([`check`]): `<dir>` passes `verify-assets sums`,
//!   `SHA256SUMS` lists exactly the assets of that commit ([`expected_assets`],
//!   [`hostkeeper_assets`] of the one version of its hostkeeper packages, and
//!   [`distribution_assets`] of the release version that version names) and no other,
//!   and each tarball, package, Debian package and RPM package has exactly one provenance bundle whose one
//!   subject is that file's digest ([`crate::bundles`]), and no bundle is there for anything
//!   else.
//! - `release publish <dir> --repo <owner/repo> --tag <tag> --commit <sha> --run-id <id>
//!   --notes-file <file> [--note <text>]... [--gh <program>]` ([`publish`]): `check`, then
//!   creates release `<tag>` at `<sha>` as a draft with every file of `<dir>`, reads the
//!   draft's assets back and refuses unless they are exactly those files, and only then
//!   publishes it (which is when GitHub creates the tag). A tag or a published release that
//!   exists is refused; a draft a failed run left is replaced.
//!
//! Beside them, the hostkeeper packages, which `artifacts.yml` makes and checks:
//! `release hostkeeper-package` ([`hostkeeper_package`]) assembles the package of one platform's
//! built binary, `tierstore-<version>-<platform>.tar.zst`; `release hostkeeper-index`
//! ([`hostkeeper_index`]) writes the release index of the three, `index.json`; and
//! `verify-assets hostkeeper-package` and `verify-assets hostkeeper-index`
//! ([`hostkeeper_verify`]) check them as a host does. See those modules.
//!
//! `release check-glibc <binary> --max <major>.<minor>` ([`glibc`]) refuses a Linux binary that
//! needs a glibc symbol version above `--max`: the build jobs of `artifacts.yml` run it with the
//! glibc the release binaries are linked against (2.28).
//!
//! And the files the distribution channels take: `release tap-tarball` ([`tap_tarball`]) names a
//! platform's tarball as the Homebrew tap's formula downloads it, `tierstore-<version>-<target>.tar.gz`,
//! `release deb` ([`deb`]) builds `tierstore_<version>_<arch>.deb` of a Linux binary, and `release rpm`
//! ([`rpm`]) builds `tierstore-<version>-1.<arch>.rpm` of it.
//!
//! After a release, or by hand: `release desired` ([`desired`]) reads the release index and the
//! hostkeeper packages of a published release and gives the version, the index and the targets
//! ([`desired_targets`]) `.github/workflows/hostkeeper-desired.yml` sets on the hostkeeper
//! coordinator, or refuses a release whose index lacks a platform.

mod binary;
pub mod deb;
pub mod desired;
pub mod desired_apt;
pub mod desired_targets;
pub mod glibc;
pub mod hostkeeper_index;
pub mod hostkeeper_package;
pub mod hostkeeper_verify;
mod plan;
mod publish;
pub mod rpm;
mod set_version;
pub mod tap_tarball;

pub use binary::{check_binary, check_built, check_said};
pub use plan::{
    DOCS_PATHS, Request, VERSION_ARTIFACT, docs_only, is_docs_path, manual, next_tag, plan,
    read_version, version,
};
pub use publish::{Gh, RealGh, publish};
pub use set_version::set_version;

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
/// files of each platform (`assets-<platform>`, one per [`Platform::ALL`]), the sums, and the
/// provenance bundles of every platform's tarball and hostkeeper package.
pub const ARTIFACTS: [&str; 5] = [
    "assets-linux-x86_64",
    "assets-linux-arm64",
    "assets-macos-arm64",
    "sha256sums",
    "bundles",
];

/// The files `SHA256SUMS` lists for `commit` besides its hostkeeper packages, sorted: for each
/// platform the tarball and the SBOM of [`BINARY`].
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

/// The hostkeeper files `SHA256SUMS` lists for the package version `version`, sorted: the
/// release index and the package of each platform.
pub fn hostkeeper_assets(version: &str) -> Vec<String> {
    let mut names: Vec<String> = Platform::ALL
        .iter()
        .map(|p| hostkeeper_package::file_name(version, *p))
        .collect();
    names.push(hostkeeper_index::INDEX.to_owned());
    names.sort();
    names
}

/// The files `SHA256SUMS` lists for the release `version` (`<major>.<minor>.<patch>`) besides
/// the commit-named tarballs and the hostkeeper packages, sorted: the tap tarball of each
/// platform ([`tap_tarball`]) and the Debian and RPM packages of each Linux platform ([`deb`],
/// [`rpm`]).
pub fn distribution_assets(version: &str) -> Vec<String> {
    let mut names: Vec<String> = Platform::ALL
        .iter()
        .map(|p| tap_tarball::file_name(version, *p))
        .collect();
    for platform in Platform::ALL {
        if let Some(architecture) = deb::architecture(platform) {
            names.push(deb::file_name(version, architecture));
        }
        if let Some(architecture) = rpm::architecture(platform) {
            names.push(rpm::file_name(version, architecture));
        }
    }
    names.sort();
    names
}

/// The one version of the hostkeeper packages of `commit` that `listed` names (by their names,
/// `tierstore-<version>-<platform>.tar.zst`, the version ending `+<the first 12 digits>`), or
/// the problem.
fn package_version(listed: &[&str], commit: &str) -> Result<String, String> {
    let short = &commit[..12];
    let mut versions: Vec<&str> = listed
        .iter()
        .filter_map(|name| hostkeeper_index::parse_name(name))
        .map(|(version, _)| version)
        .filter(|version| version.ends_with(&format!("+{short}")))
        .collect();
    versions.sort_unstable();
    versions.dedup();
    match versions.as_slice() {
        [one] => Ok((*one).to_owned()),
        [] => Err(format!("{SUMS} lists no hostkeeper package of {commit}")),
        many => Err(format!(
            "{SUMS} lists hostkeeper packages of {} versions of {commit}: {}",
            many.len(),
            many.join(", ")
        )),
    }
}

/// The `release check` verb: the lines to print.
pub fn check(dir: &Path, commit: &str) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    let mut printed = vec![sums::verify_sums(dir, Sha256::of)?];
    let lines = sums::read_sums(&dir.join(SUMS))?;
    let mut problems = Vec::new();
    let listed: Vec<&str> = lines.iter().map(|l| l.name.as_str()).collect();
    let mut expected = expected_assets(commit);
    match package_version(&listed, commit) {
        Ok(version) => {
            expected.extend(hostkeeper_assets(&version));
            // `X.Y.Z+<12 hex>`: the release's version is what is before the `+`.
            let release = version.split_once('+').map_or(&version[..], |(v, _)| v);
            expected.extend(distribution_assets(release));
        }
        Err(problem) => problems.push(problem),
    }
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
        "release check: {} files, the assets of {commit}, each tarball and package with its bundle",
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
pub(crate) fn append_output(output: &Path, lines: &[String]) -> Result<(), Error> {
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
