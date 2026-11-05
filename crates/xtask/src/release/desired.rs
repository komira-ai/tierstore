//! `release desired --repo <owner/repo> --version <X.Y.Z> --dir <dir> [--output <file>]
//! [--gh <program>]`: what `.github/workflows/hostkeeper-desired.yml` sets as the desired version
//! of tierstore on the hostkeeper coordinator for the release `v<X.Y.Z>`, or why it sets nothing.
//! It runs before any step of that workflow can hold a token, so a refusal here means the
//! coordinator is never called.
//!
//! The release must be a published GitHub release of `<owner/repo>` (not a draft, not a
//! pre-release) whose commit is a full SHA, with the assets `index.json` and `SHA256SUMS`. The two
//! are downloaded into `<dir>` (`gh release download`), and then:
//!
//! - `index.json` has the SHA-256 that `SHA256SUMS` lists for it;
//! - hostkeeper's strict reader (`hostkeeper_config::index`) reads it as the index of tierstore
//!   `<X.Y.Z>+<the first 12 digits of the release's commit>`, the version the packages carry, and it
//!   is in that reader's canonical form;
//! - it lists a package for each of linux-x86_64, linux-arm64 and macos-arm64. The coordinator
//!   orders a service and a version and never a platform, so a host whose platform the index lacks
//!   would be refused the version (`no_package_for_platform`) while the others take it: an index
//!   without the macOS package would leave every Mac behind while the Linux hosts converge;
//! - each package it lists is an asset of the release, `tierstore-<version>-<platform>.tar.zst`,
//!   of the size the index gives.
//!
//! Every problem found is reported, one line each. When there is none, `--output` (on Actions,
//! `GITHUB_OUTPUT`) gets `package-version=<X.Y.Z+12 hex>`, the version the coordinator is given,
//! and `package-digest=<SHA-256 of index.json>`, the one digest the coordinator records for that
//! version: the index pins the SHA-256 of every platform's package, and the daemon checks each
//! package it fetches against the index, not against this digest.

use std::path::Path;

use hostkeeper_config::index::ReleaseIndex;

use super::hostkeeper_index::{
    INDEX, host_platform, index_failed, service, version as host_version,
};
use super::hostkeeper_package::{file_name, is_plain_version};
use super::{Gh, append_output, array};
use crate::Error;
use crate::build_commit::is_commit;
use crate::fetch::Sha256;
use crate::package::Platform;
use crate::sums::{SUMS, read_sums};

/// The release's assets: each name with its size in bytes.
type Assets = Vec<(String, Option<u64>)>;

/// The published release `tag` of `repo`: its commit and its assets.
fn release(gh: &dyn Gh, repo: &str, tag: &str) -> Result<(String, Assets), Error> {
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

/// One line per way the index `text` of the package version `version` does not let every host
/// take the version from the release with the assets `assets`.
fn index_problems(text: &str, index: &ReleaseIndex, version: &str, assets: &Assets) -> Vec<String> {
    let mut problems = Vec::new();
    if text != index.to_json() {
        problems.push(format!(
            "{INDEX} is not the canonical JSON of its index (keys sorted, no white space)"
        ));
    }
    for platform in Platform::ALL {
        let entry = match index.entry_for(host_platform(platform)) {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!(
                    "{INDEX}: {e} ({}): a host of {} would be refused the version, so it is not set",
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
    }
    problems
}

/// The `release desired` verb: the lines to print.
///
/// # Errors
/// A `--version` that is not `<major>.<minor>.<patch>` (a usage error); no published release of
/// that version, or one whose commit is not a full SHA; a release without `index.json` or
/// `SHA256SUMS`; the download fails; `index.json` is not what `SHA256SUMS` lists, is refused by
/// hostkeeper's reader, is not canonical, lacks a platform, or lists a package the release does
/// not hold at that size; `--output` cannot be appended to.
pub fn desired(
    gh: &dyn Gh,
    repo: &str,
    version: &str,
    dir: &Path,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
        )));
    }
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
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let into = dir.display().to_string();
    let download: Vec<String> = [
        "release",
        "download",
        tag.as_str(),
        "--repo",
        repo,
        "--dir",
        into.as_str(),
        "--pattern",
        INDEX,
        "--pattern",
        SUMS,
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    gh.run(&download)?;
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
    let package_version = format!("{version}+{}", &commit[..12]);
    let index = ReleaseIndex::parse(&text, &service()?, &host_version(&package_version)?)
        .map_err(index_failed)?;
    let problems = index_problems(&text, &index, &package_version, &assets);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let lines = [
        format!("package-version={package_version}"),
        format!("package-digest={digest}"),
    ];
    if let Some(output) = output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![format!(
        "release {tag} ({commit}): {INDEX} lists tierstore {package_version} for each of {} platforms, each an asset of the release",
        Platform::ALL.len()
    )];
    printed.extend(lines);
    Ok(printed)
}

#[cfg(test)]
mod tests;
