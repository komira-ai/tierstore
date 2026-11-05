//! `release hostkeeper-index <dir> --commit <sha> --event <event> [--output <file>]`: the release
//! index of the hostkeeper packages in `<dir>`, `<dir>/index.json`, and the line of that file in
//! `<dir>/SHA256SUMS.index`, which `sums assemble` merges into `SHA256SUMS` beside the platforms'
//! lines ([`crate::sums`]).
//!
//! The index is hostkeeper's (`hostkeeper_config::index`, komira-ai/hostkeeper#23): schema 1, the
//! service `tierstore` and the packages' version, and for each platform its file, SHA-256 and
//! size, sorted by platform, in canonical JSON (keys sorted, no white space, no final newline),
//! written by hostkeeper's own writer, so the same packages give the same bytes on every run and
//! every runner.
//!
//! The packages are the files `tierstore-<version>-<platform>.tar.zst` of `<dir>` ([`packages`]):
//! of one version, `<major>.<minor>.<patch>+<the first 12 digits of --commit>`, so at most one per
//! platform. A file named `tierstore-*.tar.zst` that is not one of them is refused.
//!
//! A release's index lists every platform: a host whose platform the index lacks is refused
//! (`no_package_for_platform`), so a release without the macos-arm64 package would leave every Mac
//! on its old version. Unless `--event` is `pull_request`, a `<dir>` without the package of each
//! of linux-x86_64, linux-arm64 and macos-arm64 is refused and nothing is written. For a pull
//! request (`--event pull_request`, on Actions `GITHUB_EVENT_NAME`) the macOS job does not run:
//! the two Linux packages must be there and the macOS one must not, and no index is written,
//! since an index of a pull request would be one of two platforms.
//!
//! `--output` appends `package-version=<version>` to a file (on Actions, `GITHUB_OUTPUT`: the
//! attest job names the packages it signs by it). The packages are read once each, in pieces.

use std::io::Read;
use std::path::Path;

use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::index::{INDEX_FILE, IndexError, PackageEntry, ReleaseIndex};
use hostkeeper_config::names::{ServiceName, Version};
use hostkeeper_config::platform::Platform as HostPlatform;
use sha2::{Digest, Sha256};

use super::append_output;
use super::hostkeeper_package::{SERVICE, file_name, is_release_version};
use crate::Error;
use crate::build_commit::full_commit;
use crate::fetch;
use crate::package::{Platform, write_new};
use crate::sums::{self, Line, files_in};

/// The release index's file name.
pub const INDEX: &str = INDEX_FILE;

/// The suffix of the sums file this verb writes, `SHA256SUMS.index`, which `sums assemble` takes
/// with the `SHA256SUMS.<platform>` files.
pub const INDEX_PART: &str = "index";

/// The event of a pull request's run (`GITHUB_EVENT_NAME`), for which no index is written.
pub const PULL_REQUEST: &str = "pull_request";

/// The platforms a pull request builds: the macOS job does not run for one.
pub const PULL_REQUEST_PLATFORMS: [Platform; 2] = [Platform::LinuxX86_64, Platform::LinuxArm64];

/// The prefix of a package's file name.
const PREFIX: &str = "tierstore-";

/// The suffix of a package's file name.
pub const SUFFIX: &str = ".tar.zst";

/// hostkeeper's platform for `platform`.
pub fn host_platform(platform: Platform) -> HostPlatform {
    match platform {
        Platform::LinuxX86_64 => HostPlatform::LinuxX86_64,
        Platform::LinuxArm64 => HostPlatform::LinuxArm64,
        Platform::MacosArm64 => HostPlatform::MacosArm64,
    }
}

/// This crate's platform for hostkeeper's `platform`.
pub fn our_platform(platform: HostPlatform) -> Platform {
    match platform {
        HostPlatform::LinuxX86_64 => Platform::LinuxX86_64,
        HostPlatform::LinuxArm64 => Platform::LinuxArm64,
        HostPlatform::MacosArm64 => Platform::MacosArm64,
    }
}

/// The version and the platform of the package file name `name`
/// (`tierstore-<version>-<platform>.tar.zst`), when it is one of a release platform. The version
/// is not checked here.
pub fn parse_name(name: &str) -> Option<(&str, Platform)> {
    let stem = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    Platform::ALL.into_iter().find_map(|platform| {
        let version = stem.strip_suffix(platform.name())?.strip_suffix('-')?;
        Some((version, platform))
    })
}

/// The hostkeeper packages of a directory: their one version and the platforms they are of, in
/// the order of their names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packages {
    pub version: String,
    pub platforms: Vec<Platform>,
}

/// The hostkeeper packages of `dir` for the full commit `commit`.
///
/// # Errors
/// `dir` cannot be listed; a file named `tierstore-*.tar.zst` there is not the package of a
/// release platform, or not of a release version of `commit`, or not of the version of the others
/// (one line each); or there is no package at all.
pub fn packages(dir: &Path, commit: &str) -> Result<Packages, Error> {
    let short = commit.get(..12).unwrap_or(commit);
    let mut problems = Vec::new();
    let mut version: Option<String> = None;
    let mut platforms = Vec::new();
    for name in files_in(dir)? {
        if !(name.starts_with(PREFIX) && name.ends_with(SUFFIX)) {
            continue;
        }
        let Some((found, platform)) = parse_name(&name) else {
            problems.push(format!(
                "{name}: not tierstore-<version>-<platform>{SUFFIX} of a release platform"
            ));
            continue;
        };
        if !(is_release_version(found) && found.ends_with(&format!("+{short}"))) {
            problems.push(format!(
                "{name}: {found:?} is not a version of {short}, <major>.<minor>.<patch>+{short}"
            ));
            continue;
        }
        match &version {
            Some(first) if first != found => {
                problems.push(format!(
                    "{name}: of the version {found}, not {first}, the version of the other packages"
                ));
                continue;
            }
            Some(_) => {}
            None => version = Some(found.to_owned()),
        }
        platforms.push(platform);
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let Some(version) = version else {
        return Err(Error::Failed(format!(
            "{} holds no hostkeeper package ({PREFIX}<version>-<platform>{SUFFIX})",
            dir.display()
        )));
    };
    Ok(Packages { version, platforms })
}

/// One line per platform of `wanted` that `found` lacks and per platform of `found` that is not
/// wanted, saying why each must be there or must not.
pub fn set_problems(found: &[Platform], wanted: &[Platform]) -> Vec<String> {
    let mut problems = Vec::new();
    for platform in wanted.iter().filter(|p| !found.contains(p)) {
        problems.push(format!(
            "no hostkeeper package for {}: {INDEX} lists every platform, and hostkeeper refuses a host of a platform it lacks (no_package_for_platform)",
            platform.name()
        ));
    }
    for platform in found.iter().filter(|p| !wanted.contains(p)) {
        problems.push(format!(
            "a hostkeeper package for {}, which a pull request does not build (the macOS job does not run for one)",
            platform.name()
        ));
    }
    problems
}

/// The SHA-256 and the size of the file at `path`, read once, in pieces.
///
/// # Errors
/// The file cannot be read.
pub fn digest_file(path: &Path) -> Result<(Sha256Hex, u64), Error> {
    let what = || format!("read {}", path.display());
    let mut file = std::fs::File::open(path).map_err(Error::spawn(what()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buffer).map_err(Error::spawn(what()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    let hex = hex::encode(hasher.finalize());
    let digest = Sha256Hex::parse(&hex).ok_or_else(|| Error::Failed(hex.clone()))?;
    Ok((digest, bytes))
}

/// The service name of the packages, as hostkeeper spells it.
///
/// # Errors
/// Never for [`SERVICE`]; the type admits a name outside hostkeeper's grammar.
pub fn service() -> Result<ServiceName, Error> {
    ServiceName::new(SERVICE).map_err(|e| Error::Failed(e.to_string()))
}

/// `version` as hostkeeper's version.
///
/// # Errors
/// A version outside hostkeeper's grammar, which a release version never is.
pub fn version(version: &str) -> Result<Version, Error> {
    Version::new(version).map_err(|e| Error::Failed(e.to_string()))
}

/// The release index of the packages `found` of `dir`.
///
/// # Errors
/// A package cannot be read.
pub fn index_of(dir: &Path, found: &Packages) -> Result<ReleaseIndex, Error> {
    let mut entries = Vec::new();
    for platform in &found.platforms {
        let (sha256, bytes) = digest_file(&dir.join(file_name(&found.version, *platform)))?;
        entries.push(PackageEntry {
            platform: host_platform(*platform),
            sha256,
            bytes,
        });
    }
    let index = ReleaseIndex::new(service()?, version(&found.version)?, entries);
    index.map_err(index_failed)
}

/// The error of an index hostkeeper's reader or writer refuses: its words, with the reason token a
/// host reports for it.
pub fn index_failed(e: IndexError) -> Error {
    Error::Failed(format!("{e} ({})", e.reason()))
}

/// The `release hostkeeper-index` verb: the lines to print.
///
/// # Errors
/// A `--commit` that is not a full SHA (a usage error); the packages of `dir` are not one
/// version of the commit, or not the set the event builds; a package cannot be read; `index.json`
/// or `SHA256SUMS.index` is there already; `--output` cannot be appended to.
pub fn hostkeeper_index(
    dir: &Path,
    commit: &str,
    event: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    let found = packages(dir, commit)?;
    let pull_request = event == PULL_REQUEST;
    let wanted: &[Platform] = if pull_request {
        &PULL_REQUEST_PLATFORMS
    } else {
        &Platform::ALL
    };
    let problems = set_problems(&found.platforms, wanted);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    if pull_request {
        return Ok(vec![format!(
            "a pull request: the macOS job does not run for one, so no {INDEX} is written; the Linux packages of {} are there",
            found.version
        )]);
    }
    let index = index_of(dir, &found)?;
    let json = index.to_json();
    write_new(&dir.join(INDEX), json.as_bytes())?;
    let line = Line {
        sha256: fetch::Sha256::of(json.as_bytes()),
        name: INDEX.to_owned(),
    };
    let part = dir.join(format!("{}.{INDEX_PART}", sums::SUMS));
    write_new(&part, sums::render(std::slice::from_ref(&line)).as_bytes())?;
    if let Some(output) = output {
        append_output(output, &[format!("package-version={}", found.version)])?;
    }
    Ok(vec![
        format!(
            "{INDEX} of tierstore {}: {} packages, {}",
            found.version,
            index.packages().len(),
            sums::render(std::slice::from_ref(&line)).trim_end()
        ),
        json,
    ])
}

#[cfg(test)]
mod tests;
