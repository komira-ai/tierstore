//! `release desired --repo <owner/repo> (--version <X.Y.Z> | --after-run <id>) [--skip-superseded]
//! --min-hostkeeper <x.y.z> --dir <dir> [--apt-url <url>] [--output <file>] [--gh <program>]
//! [--curl <program>]`: what the deploy of `.github/workflows/release.yml` (`deploy-check`) and
//! `.github/workflows/hostkeeper-desired.yml` (by hand) set as the desired version of tierstore on
//! the hostkeeper coordinator, or why they set nothing. It runs before any step of those workflows
//! can hold a token, so a refusal here means the coordinator is never called.
//!
//! Which release:
//!
//! - `--version X.Y.Z`: the release `vX.Y.Z`, whichever it is (by hand, an older one included: a
//!   rollback; after a release, the release the run published).
//! - `--after-run <id>` (the completion of a run of `release.yml`): when the job `publish` of that
//!   run succeeded, the newest published release of `<owner/repo>` (the highest `vX.Y.Z`, not a
//!   draft, not a pre-release). Release runs are one at a time, so that is the release the run
//!   published, or a newer one whose run then comes next. When `publish` did not succeed (most
//!   runs plan no release, and their `publish` is skipped) nothing is set: the output is `set=false`
//!   and the verb succeeds.
//!
//! Exactly one of the two is given; an empty one counts as not given. `--min-hostkeeper` and
//! `--apt-url` are checked first, whichever way it is.
//!
//! With `--skip-superseded` (the deploy of `release.yml`, which runs it for the release that run
//! published), a release older than the newest published one is not set: the output is
//! `set=false` and the verb succeeds. Release runs are one at a time, but a release made by hand
//! runs beside them, and a run that waited behind it must not move the fleet back.
//!
//! What is set for the release is what the lab stage of `release.yml` sets for it
//! ([`super::desired_apt`], with the same `--min-hostkeeper`): the hosts install tierstore from the
//! testing suite through hostkeeper's apt installer, so each Linux platform's list is one `apt`
//! entry of the host's repository entry `komira-testing`, the package version `X.Y.Z` and the
//! `.deb`'s SHA-256, held to the suite as a client reads it and to the release's `SHA256SUMS`. A
//! list of `hostkeeper-archive` entries alone orders no host: the coordinator holds every host that
//! runs tierstore from apt `installer_dropped`. Without `--macos-pkg`, `targets` has no
//! `macos-arm64` list: they are exactly the lab's. With `--macos-pkg` (the workflows' way)
//! `targets` also has a `macos-arm64` list of one `macos-pkg` entry of the release's signed `.pkg`
//! ([`super::desired_macos`]), downloaded and held to the digest of `SHA256SUMS`. Both
//! `set-desired` calls, the Linux build nodes' and the Macs' (host group `mac-build`), send these
//! same targets: the coordinator keeps one release per version, whatever the host group, and
//! refuses the same version set again with other targets (`release_exists`), so a second call
//! with other lists would never set the version for its group. A host is ordered its own
//! platform's list and ignores the others. No release index is sent: it would list archive
//! entries the targets do not have.
//!
//! When the run published nothing, `--output` (on Actions, `GITHUB_OUTPUT`) gets `set=false`.
//! Otherwise every problem found is reported, one line each, and when there is none `--output`
//! gets `set=true`, `package-version=<X.Y.Z>` and `targets=<JSON>`.

use std::path::Path;

use super::desired_apt::{self, apt_targets};
use super::desired_macos;
use super::hostkeeper_package::is_plain_version;
use super::{Gh, append_output, array};
use crate::Error;
use crate::build_commit::is_commit;
use crate::channels::check::Host;
use crate::sums::SUMS;

/// The job of `release.yml` that publishes a release.
pub const PUBLISH_JOB: &str = "publish";

/// The release's assets: each name with its size in bytes.
type Assets = Vec<(String, Option<u64>)>;

/// What `release desired` is asked.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    /// `--version`; empty when not given.
    pub version: &'a str,
    /// `--after-run`; empty when not given.
    pub after_run: &'a str,
    /// The `minHostkeeper` of every entry: the first hostkeeper release whose daemon runs the
    /// hook command of an `apt` entry.
    pub min_hostkeeper: &'a str,
    /// Where the release's `SHA256SUMS` and the `Packages` files read are put.
    pub dir: &'a Path,
    /// The apt repository whose suite `testing` the hosts install from.
    pub apt_url: &'a str,
    /// Add to `targets` the `macos-arm64` list, one `macos-pkg` entry of the release's signed
    /// `.pkg`, beside the apt lists. Without it `targets` is what the lab sets.
    pub macos_pkg: bool,
    /// Set nothing (`set=false`) when a published release is newer than the one chosen: the
    /// deploy of a release run, which must never move the fleet back to a release a newer one
    /// has superseded. Not given by hand, where an older release is a rollback.
    pub skip_superseded: bool,
    /// The program that reads the suite.
    pub curl: &'a Path,
    pub output: Option<&'a Path>,
}

/// The published release `tag` of `repo`: its commit and its assets.
pub(super) fn release(gh: &dyn Gh, repo: &str, tag: &str) -> Result<(String, Assets), Error> {
    let Some(found) = gh.api(&format!("repos/{repo}/releases/tags/{tag}"))? else {
        return Err(Error::Failed(format!(
            "{repo} has no release {tag}: --version names a released version of tierstore"
        )));
    };
    if found["draft"] == true || found["prerelease"] == true {
        return Err(Error::Failed(format!(
            "release {tag} of {repo} is a draft or a pre-release, not a released version"
        )));
    }
    let commit = found["target_commitish"].as_str().unwrap_or_default();
    if !(commit.len() == 40 && is_commit(commit)) {
        return Err(Error::Failed(format!(
            "release {tag} of {repo} names {commit:?} as its commit, not a full commit SHA"
        )));
    }
    let assets = array(&found.get("assets").cloned())
        .iter()
        .filter_map(|a| Some((a["name"].as_str()?.to_owned(), a["size"].as_u64())))
        .collect();
    Ok((commit.to_owned(), assets))
}

/// `X.Y.Z` as three numbers, for a plain version.
fn numbers(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Whether the job [`PUBLISH_JOB`] of the run `run` of `repo` succeeded.
fn published(gh: &dyn Gh, repo: &str, run: &str) -> Result<bool, Error> {
    let path = format!("repos/{repo}/actions/runs/{run}/jobs?per_page=100");
    let Some(found) = gh.api(&path)? else {
        return Err(Error::Failed(format!("{repo} has no run {run}")));
    };
    let jobs = array(&found.get("jobs").cloned()).to_vec();
    // One page holds every job of release.yml; a run with more is refused, not half read.
    if found["total_count"].as_u64() != Some(jobs.len() as u64) {
        return Err(Error::Failed(format!(
            "run {run} of {repo} has {} jobs; one page of {} was read",
            found["total_count"],
            jobs.len()
        )));
    }
    Ok(jobs
        .iter()
        .any(|j| j["name"] == PUBLISH_JOB && j["conclusion"] == "success"))
}

/// The highest `X.Y.Z` of the published releases of `repo` (their tags `vX.Y.Z`).
fn newest(gh: &dyn Gh, repo: &str) -> Result<String, Error> {
    let found = gh.api(&format!("repos/{repo}/releases?per_page=100"))?;
    array(&found)
        .iter()
        .filter(|r| r["draft"] == false && r["prerelease"] == false)
        .filter_map(|r| r["tag_name"].as_str()?.strip_prefix('v'))
        .filter(|v| is_plain_version(v))
        .max_by_key(|v| numbers(v))
        .map(str::to_owned)
        .ok_or_else(|| Error::Failed(format!("{repo} has no published release vX.Y.Z")))
}

/// The plain version the request names: `--version`, or the newest release when the run of
/// `--after-run` published one; `None` when it published none.
fn chosen(gh: &dyn Gh, request: &Request<'_>) -> Result<Option<String>, Error> {
    match (request.version, request.after_run) {
        (version, "") if !version.is_empty() => {
            if !is_plain_version(version) {
                return Err(Error::Usage(format!(
                    "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
                )));
            }
            Ok(Some(version.to_owned()))
        }
        ("", run) if !run.is_empty() => {
            if !run.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Usage(format!(
                    "--after-run {run:?} is not the id of a workflow run"
                )));
            }
            if published(gh, request.repo, run)? {
                newest(gh, request.repo).map(Some)
            } else {
                Ok(None)
            }
        }
        _ => Err(Error::Usage(
            "give one of --version and --after-run, not both and not neither".to_owned(),
        )),
    }
}

/// The `release desired` verb: the lines to print.
///
/// # Errors
/// A `--min-hostkeeper` that is not `<major>.<minor>.<patch>` or an `--apt-url` that is not an
/// https URL; neither or both of `--version` and `--after-run`, a `--version` that is not
/// `<major>.<minor>.<patch>` or an `--after-run` that is not digits (usage errors); a run of
/// `--after-run` that GitHub does not know or lists in more than one page, or a repository with no
/// published release; then those of [`apt_targets`] for the release chosen; `--output` cannot be
/// appended to.
pub fn desired(gh: &dyn Gh, host: &dyn Host, request: &Request<'_>) -> Result<Vec<String>, Error> {
    desired_apt::usage(request.min_hostkeeper, request.apt_url)?;
    let Some(version) = chosen(gh, request)? else {
        return set_nothing(
            request,
            format!(
                "run {} of {} did not publish a release ({PUBLISH_JOB} did not succeed): nothing is set",
                request.after_run, request.repo
            ),
        );
    };
    if request.skip_superseded {
        let newest = newest(gh, request.repo)?;
        if numbers(&newest) > numbers(&version) {
            return set_nothing(
                request,
                format!(
                    "v{version} is superseded by v{newest}, the newest published release of {}: nothing is set",
                    request.repo
                ),
            );
        }
    }
    let found = apt_targets(
        gh,
        host,
        &desired_apt::Request {
            curl: request.curl,
            repo: request.repo,
            version: &version,
            min_hostkeeper: request.min_hostkeeper,
            dir: request.dir,
            apt_url: request.apt_url,
        },
    )?;
    let [package_version, mut targets] = found.lines;
    let mut summary = found.summary;
    if request.macos_pkg {
        let entry = desired_macos::macos_entry(
            gh,
            request.repo,
            &format!("v{version}"),
            &version,
            request.dir,
            request.min_hostkeeper,
        )?;
        let mut all = found.targets;
        all.insert(
            desired_macos::PLATFORM.to_owned(),
            serde_json::json!({"entries": [entry]}),
        );
        targets = format!("targets={}", serde_json::Value::Object(all));
        summary.push_str(&format!(
            "; the release's {} has the digest {SUMS} lists",
            super::macos_pkg_name(&version)
        ));
    }
    let lines = vec!["set=true".to_owned(), package_version, targets];
    if let Some(output) = request.output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![summary];
    printed.extend(lines);
    Ok(printed)
}

/// Hand on `set=false` and say `why`.
fn set_nothing(request: &Request<'_>, why: String) -> Result<Vec<String>, Error> {
    let lines = vec!["set=false".to_owned()];
    if let Some(output) = request.output {
        append_output(output, &lines)?;
    }
    Ok(vec![why, lines[0].clone()])
}

#[cfg(test)]
mod tests;
