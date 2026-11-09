//! `release desired --repo <owner/repo> (--version <X.Y.Z> | --after-run <id>) --dir <dir>
//! [--output <file>] [--gh <program>]`: what `.github/workflows/hostkeeper-desired.yml` sets as
//! the desired version of tierstore on the hostkeeper coordinator, or why it sets nothing. It runs
//! before any step of that workflow can hold a token, so a refusal here means the coordinator is
//! never called.
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
//! from its two events and one is always empty.
//!
//! The release must be a published GitHub release (not a draft, not a pre-release) whose commit is
//! a full SHA, with the assets `index.json` and `SHA256SUMS`. These, the package of each platform
//! and its `.sigstore.json` bundle are downloaded into `<dir>` (`gh release download`), and then:
//!
//! - `index.json` has the SHA-256 that `SHA256SUMS` lists for it, and is at most
//!   [`MAX_INDEX_BYTES`] (hostkeeper's CI client passes no longer one);
//! - hostkeeper's strict reader (`hostkeeper_config::index`) reads it as the index of tierstore
//!   `<X.Y.Z>+<the first 12 digits of the release's commit>`, the version the packages carry, and it
//!   is in that reader's canonical form;
//! - it lists a package for each of linux-x86_64, linux-arm64 and macos-arm64. A host whose
//!   platform a release lacks is ordered nothing, so an index without the macOS package would
//!   leave every Mac behind while the Linux hosts converge;
//! - each package it lists is an asset of the release, `tierstore-<version>-<platform>.tar.zst`,
//!   of the size the index gives; the downloaded file has that size and the SHA-256 the index
//!   gives; its bundle is a provenance bundle whose one subject is that SHA-256
//!   ([`crate::bundles::check`]); and hostkeeper's own unpacker unpacks it into `<dir>/unpacked`
//!   as a host does, which gives the `min_hostkeeper` of its manifest.
//!
//! Every problem found is reported, one line each. When there is none, `--output` (on Actions,
//! `GITHUB_OUTPUT`) gets `set=true`, `package-version=<X.Y.Z+12 hex>` (the version the
//! coordinator is given), `release-index=<the text of index.json>`, `release-index-digest=<its
//! SHA-256>` and `targets=<JSON>`, the targets of the release ([`super::desired_targets`]).

use std::path::Path;

use hostkeeper_config::index::ReleaseIndex;

use super::desired_targets::{Archive, Release, targets};
use super::hostkeeper_index::{
    INDEX, host_platform, index_failed, our_platform, service, sha256_file, version as host_version,
};
use super::hostkeeper_package::{file_name, is_plain_version};
use super::hostkeeper_verify::unpack;
use super::{Gh, append_output, array};
use crate::Error;
use crate::build_commit::is_commit;
use crate::bundles;
use crate::fetch::Sha256;
use crate::package::Platform;
use crate::sums::{SUMS, read_sums};

/// The most bytes of `index.json`: the `release_index` input of hostkeeper's CI client is at
/// most 3 KiB.
pub const MAX_INDEX_BYTES: usize = 3 * 1024;

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
    /// Where the release's files are downloaded and the packages unpacked.
    pub dir: &'a Path,
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

/// One line per way the index `text` of the package version `version` does not let every host
/// take the version from the release with the assets `assets`.
fn index_problems(text: &str, index: &ReleaseIndex, version: &str, assets: &Assets) -> Vec<String> {
    let mut problems = Vec::new();
    if text != index.to_json() {
        problems.push(format!(
            "{INDEX} is not the canonical JSON of its index (keys sorted, no white space)"
        ));
    }
    if text.len() > MAX_INDEX_BYTES {
        problems.push(format!(
            "{INDEX} is {} bytes, over the {MAX_INDEX_BYTES} hostkeeper's CI client passes",
            text.len()
        ));
    }
    for platform in Platform::ALL {
        let entry = match index.entry_for(host_platform(platform)) {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!(
                    "{INDEX}: {e} ({}): a host of {} would be ordered nothing, so it is not set",
                    e.reason(),
                    platform.name()
                ));
                continue;
            }
        };
        let name = file_name(version, platform);
        match assets.iter().find(|(n, _)| *n == name) {
            None => problems.push(format!(
                "{INDEX} lists {name}, which is not an asset of the release"
            )),
            Some((_, size)) if *size != Some(entry.bytes) => problems.push(format!(
                "{name}: the release's asset has {size:?} bytes, not the {} {INDEX} lists",
                entry.bytes
            )),
            Some(_) => {}
        }
        let bundle = bundles::name_of(&name);
        if !assets.iter().any(|(n, _)| *n == bundle) {
            problems.push(format!(
                "{name} has no bundle {bundle} in the release: every archive entry carries its attestation"
            ));
        }
    }
    problems
}

/// The archive of `platform` in `dir`, measured against the index entry `listed`, its bundle
/// checked and its manifest read by unpacking it into `<dir>/unpacked`; or every problem found.
fn archive(
    dir: &Path,
    version: &str,
    platform: Platform,
    listed: &hostkeeper_config::index::PackageEntry,
) -> Result<Result<Archive, Vec<String>>, Error> {
    let name = file_name(version, platform);
    let (sha, bytes) = sha256_file(&dir.join(&name))?;
    let mut problems = Vec::new();
    if sha.to_string() != listed.sha256.as_str() || bytes != listed.bytes {
        problems.push(format!(
            "{name}: sha256 {sha} and {bytes} bytes, not the sha256 {} and {} bytes {INDEX} lists (package_digest_mismatch)",
            listed.sha256.as_str(),
            listed.bytes
        ));
    }
    let bundle_name = bundles::name_of(&name);
    let path = dir.join(&bundle_name);
    let bundle = std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
    problems.extend(
        bundles::check(&bundle, &sha)
            .into_iter()
            .map(|p| format!("{bundle_name}: {p}")),
    );
    if !problems.is_empty() {
        return Ok(Err(problems));
    }
    let scratch = dir.join("unpacked").join(platform.name());
    let unpacked = unpack(
        &dir.join(&name),
        version,
        platform,
        listed.sha256.clone(),
        &scratch,
    )?;
    Ok(Ok(Archive {
        platform,
        digest: sha.to_string(),
        bytes,
        bundle_digest: Sha256::of(&bundle).to_string(),
        bundle_bytes: bundle.len() as u64,
        min_hostkeeper: unpacked.manifest.min_hostkeeper.to_string(),
    }))
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

/// Downloads `names` of the release `tag` of `repo` into `dir`.
fn download(gh: &dyn Gh, repo: &str, tag: &str, dir: &Path, names: &[String]) -> Result<(), Error> {
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let mut args: Vec<String> = ["release", "download", tag, "--repo", repo, "--dir"]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    args.push(dir.display().to_string());
    for name in names {
        args.push("--pattern".to_owned());
        args.push(name.clone());
    }
    gh.run(&args).map(drop)
}

/// The `release desired` verb: the lines to print.
///
/// # Errors
/// Neither or both of `--version` and `--after-run`, a `--version` that is not
/// `<major>.<minor>.<patch>` or an `--after-run` that is not digits (usage errors); a run of
/// `--after-run` that GitHub does not know or lists in more than one page, or a repository with no
/// published release; no published release of the version, or one whose commit is not a full SHA;
/// a release without `index.json` or `SHA256SUMS`; the download fails; `index.json` is not what
/// `SHA256SUMS` lists, is refused by hostkeeper's reader, is not canonical, is too long, lacks a
/// platform, or lists a package the release does not hold at that size or without its bundle; a
/// package is not the one the index lists, its bundle is not its provenance bundle, or
/// hostkeeper's unpacker refuses it; `--output` cannot be appended to.
pub fn desired(gh: &dyn Gh, request: &Request<'_>) -> Result<Vec<String>, Error> {
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
    let (repo, dir) = (request.repo, request.dir);
    let tag = format!("v{version}");
    let (commit, assets) = release(gh, repo, &tag)?;
    let missing: Vec<String> = [INDEX, SUMS]
        .iter()
        .filter(|name| !assets.iter().any(|(n, _)| n == *name))
        .map(|name| {
            format!("release {tag} has no {name}: it has no hostkeeper packages for a host to take")
        })
        .collect();
    if !missing.is_empty() {
        return Err(Error::Refused(missing));
    }
    let package_version = format!("{version}+{}", &commit[..12]);
    download(gh, repo, &tag, dir, &[INDEX.to_owned(), SUMS.to_owned()])?;
    let path = dir.join(INDEX);
    let bytes = std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let digest = Sha256::of(&bytes);
    let listed = read_sums(&dir.join(SUMS))?
        .into_iter()
        .find(|l| l.name == INDEX)
        .map(|l| l.sha256);
    if listed.as_ref() != Some(&digest) {
        let said = listed.map_or_else(|| "no line".to_owned(), |s| format!("sha256 {s}"));
        return Err(Error::Failed(format!(
            "{INDEX} has sha256 {digest}; {SUMS} of release {tag} lists {said} for it"
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Failed(format!("{INDEX} of release {tag} is not UTF-8")))?;
    let index = ReleaseIndex::parse(&text, &service()?, &host_version(&package_version)?)
        .map_err(index_failed)?;
    let problems = index_problems(&text, &index, &package_version, &assets);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let packages: Vec<String> = Platform::ALL
        .iter()
        .map(|p| file_name(&package_version, *p))
        .flat_map(|name| [bundles::name_of(&name), name])
        .collect();
    download(gh, repo, &tag, dir, &packages)?;
    let mut archives = Vec::new();
    let mut problems = Vec::new();
    // Every platform: index_problems refused an index that lacks one.
    for listed in index.packages() {
        let platform = our_platform(listed.platform);
        match archive(dir, &package_version, platform, listed)? {
            Ok(found) => archives.push(found),
            Err(found) => problems.extend(found),
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let release = Release {
        repo,
        tag: &tag,
        version: &package_version,
    };
    let lines = [
        "set=true".to_owned(),
        format!("package-version={package_version}"),
        format!("release-index={text}"),
        format!("release-index-digest={digest}"),
        format!("targets={}", targets(release, &archives)?),
    ];
    if let Some(output) = request.output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![format!(
        "release {tag} ({commit}): {INDEX} lists tierstore {package_version} for each of {} platforms; each package is the one it lists, attested, and unpacks as a host unpacks it",
        Platform::ALL.len()
    )];
    printed.extend(lines);
    Ok(printed)
}

#[cfg(test)]
mod tests;
