//! `release desired --repo <owner/repo> (--version <X.Y.Z> | --after-run <id>) --min-hostkeeper
//! <x.y.z> --dir <dir> [--apt-url <url>] [--output <file>] [--gh <program>] [--curl <program>]`:
//! what `.github/workflows/hostkeeper-desired.yml` sets as the desired version of tierstore on the
//! hostkeeper coordinator, or why it sets nothing. It runs before any step of that workflow can
//! hold a token, so a refusal here means the coordinator is never called.
//!
//! Which release:
//!
//! - `--version X.Y.Z` (a run by hand): the release `vX.Y.Z`, whichever it is, an older one
//!   included (a rollback).
//! - `--after-run <id>` (the completion of a run of `release.yml`): when the job `publish` of that
//!   run succeeded, the newest published release of `<owner/repo>` (the highest `vX.Y.Z`, not a
//!   draft, not a pre-release). Release runs are one at a time, so that is the release the run
//!   published, or a newer one whose run then comes next. When `publish` did not succeed (most
//!   runs plan no release, and their `publish` is skipped) nothing is set: the output is `set=false`
//!   and the verb succeeds.
//!
//! Exactly one of the two is given; an empty one counts as not given, so the workflow passes both
//! from its two events and one is always empty. `--min-hostkeeper` and `--apt-url` are checked
//! first, whichever way it is.
//!
//! What is set for the release is what the lab stage of `release.yml` sets for it
//! ([`super::desired_apt`], with the same `--min-hostkeeper`): the hosts install tierstore from the
//! testing suite through hostkeeper's apt installer, so each Linux platform's list is one `apt`
//! entry of the host's repository entry `komira-testing`, the package version `X.Y.Z` and the
//! `.deb`'s SHA-256, held to the suite as a client reads it and to the release's `SHA256SUMS`. A
//! list of `hostkeeper-archive` entries alone orders no host: the coordinator holds every host that
//! runs tierstore from apt `installer_dropped`. There is no `macos-arm64` list: tierstore ships no
//! package a Mac's daemon installs (no `.pkg`; the archive installer is withdrawn), so a Mac is
//! ordered nothing (`no_target`) and keeps what it runs. No release index is sent: it would list
//! archive entries the targets do not have.
//!
//! When the run published nothing, `--output` (on Actions, `GITHUB_OUTPUT`) gets `set=false`.
//! Otherwise every problem found is reported, one line each, and when there is none `--output`
//! gets `set=true`, `package-version=<X.Y.Z>` and `targets=<JSON>`.

use std::path::Path;

use super::desired_apt::{self, apt_targets};
use super::hostkeeper_package::is_plain_version;
use super::{Gh, append_output, array};
use crate::Error;
use crate::build_commit::is_commit;
use crate::channels::check::Host;

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
        let lines = vec!["set=false".to_owned()];
        if let Some(output) = request.output {
            append_output(output, &lines)?;
        }
        return Ok(vec![
            format!(
                "run {} of {} did not publish a release ({PUBLISH_JOB} did not succeed): nothing is set",
                request.after_run, request.repo
            ),
            lines[0].clone(),
        ]);
    };
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
    let [package_version, targets] = found.lines;
    let lines = ["set=true".to_owned(), package_version, targets];
    if let Some(output) = request.output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![found.summary];
    printed.extend(lines);
    Ok(printed)
}

#[cfg(test)]
mod tests;
