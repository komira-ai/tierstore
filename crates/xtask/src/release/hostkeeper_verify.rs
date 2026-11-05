//! The checks a host makes of the hostkeeper packages, made where they are built and released.
//!
//! - `verify-assets hostkeeper-package <dir> --version <X.Y.Z+12 hex> --platform <platform>
//!   --scratch <dir>` ([`verify_package`]): `<dir>/tierstore-<version>-<platform>.tar.zst` is at
//!   most [`MAX_PACKAGE_BYTES`], carries no extended attribute ([`attribute_problems`]), and
//!   hostkeeper's own unpacker (`hostkeeper_unpack::unpack`, with the bounds of hostkeeper's sample
//!   daemon config, [`LIMITS`]) unpacks it into `<scratch>/packages/tierstore/<version>` as the
//!   daemon does for an order of tierstore `<version>` on `<platform>`: every entry keeps
//!   hostkeeper's rules, and the manifest loads and names that service, version and platform. On
//!   macOS the build job then checks the code signature of the binary unpacked there
//!   (`codesign --verify --strict`): an arm64 Mac runs no binary without a valid one.
//! - `verify-assets hostkeeper-index <dir> --commit <sha> --scratch <dir>` ([`verify_index`]):
//!   `<dir>/index.json` is read with hostkeeper's strict reader for tierstore and the version of
//!   the packages of `<dir>` (`release hostkeeper-index`'s [`packages`]), is its canonical form,
//!   lists a package for each of linux-x86_64, linux-arm64 and macos-arm64 (an index that lacks one
//!   is refused: a host of that platform would be refused with `no_package_for_platform`) and none
//!   that `<dir>` does not hold, each entry's size and SHA-256 are its file's, and then each package
//!   passes [`verify_package`] into `<scratch>/<platform>`.
//!
//! The bundle of each package, its own provenance attestation, is verified offline with the rest of
//! the release's bundles by `verify-assets attestations` ([`crate::sums`]).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use hostkeeper_config::index::ReleaseIndex;
use hostkeeper_unpack::{HostFs, Limits, Owner, UnpackOrder};

use super::hostkeeper_index::{
    INDEX, Packages, digest_file, host_platform, index_failed, our_platform, packages, service,
    version as host_version,
};
use super::hostkeeper_package::{file_name, is_release_version};
use crate::Error;
use crate::build_commit::full_commit;
use crate::package::Platform;

/// The most bytes a package may have: `max_package_bytes` of hostkeeper's sample daemon config.
pub const MAX_PACKAGE_BYTES: u64 = 536_870_912;

/// The bounds of an unpack: `max_unpacked_bytes` and `max_entries` of hostkeeper's sample daemon
/// config.
pub const LIMITS: Limits = Limits {
    max_unpacked_bytes: 2_147_483_648,
    max_entries: 20_000,
};

/// The keys of the pax records that carry an extended attribute or an ACL, by their prefix.
pub const ATTRIBUTE_RECORDS: [&str; 4] = [
    "SCHILY.xattr.",
    "LIBARCHIVE.xattr.",
    "SCHILY.acl.",
    "security.",
];

/// What the name of an AppleDouble file starts with: macOS's `tar` writes a file's extended
/// attributes as a `._<name>` entry beside it.
pub const APPLE_DOUBLE: &str = "._";

/// What carries an extended attribute in the package at `path`: one line per pax record of an
/// attribute or an ACL, and per AppleDouble entry; none when it carries none.
///
/// # Errors
/// The file cannot be opened. A file that is not a tar in a zstd frame is one problem line.
pub fn attribute_problems(path: &Path) -> Result<Vec<String>, Error> {
    let file =
        std::fs::File::open(path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let decoder = match ruzstd::decoding::StreamingDecoder::new(file) {
        Ok(decoder) => decoder,
        Err(e) => return Ok(vec![format!("not a zstd frame: {e:?}")]),
    };
    let mut archive = tar::Archive::new(decoder);
    let mut problems = Vec::new();
    // Only an archive already read from has no entries to list, and this one is fresh. Reading
    // the next entry reads past the content of the one before, so a frame that breaks inside a
    // file's content is found as the error of the entry after it.
    for entry in archive.entries().into_iter().flatten() {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!("not a tar: {e}"));
                break;
            }
        };
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let last = name
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        if last.starts_with(APPLE_DOUBLE) {
            problems.push(format!(
                "{name}: an AppleDouble file, the extended attributes of another entry"
            ));
        }
        // A malformed record is left to hostkeeper's unpacker, which refuses the header.
        let keys: Vec<String> = entry
            .pax_extensions()
            .ok()
            .flatten()
            .into_iter()
            .flatten()
            .flatten()
            .map(|record| String::from_utf8_lossy(record.key_bytes()).into_owned())
            .collect();
        for key in keys {
            if ATTRIBUTE_RECORDS
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                problems.push(format!(
                    "{name}: the pax record {key}, an extended attribute or ACL"
                ));
            }
        }
    }
    Ok(problems)
}

/// Unpacks the package at `path` (SHA-256 `digest`) with hostkeeper's unpacker into
/// `<scratch>/packages`, as the order of tierstore `version` on `platform`, owned by this process's
/// user: the version directory.
fn unpack(
    path: &Path,
    version: &str,
    platform: Platform,
    digest: hostkeeper_config::daemon::Sha256Hex,
    scratch: &Path,
) -> Result<PathBuf, Error> {
    let packages = scratch.join("packages");
    std::fs::create_dir_all(&packages)
        .map_err(Error::spawn(format!("create {}", packages.display())))?;
    let meta = std::fs::metadata(&packages)
        .map_err(Error::spawn(format!("read {}", packages.display())))?;
    let owner = Owner {
        uid: meta.uid(),
        gid: meta.gid(),
    };
    let fs = HostFs::open(&packages, owner)
        .map_err(Error::spawn(format!("open {}", packages.display())))?;
    let order = UnpackOrder {
        service: service()?,
        version: host_version(version)?,
        platform: host_platform(platform),
        package_digest: digest,
    };
    // The unpack of a Linux host, on every runner: the archive's rules and the manifest. A Mac's
    // daemon also checks the staged tree for quarantine attributes and code signatures through an
    // adapter of its own (hostkeeper's unpacker has none); the macOS build job checks the code
    // signature of the binary unpacked here with `codesign --verify --strict` instead.
    let host = hostkeeper_unpack::Platform::Linux;
    let unpacked = hostkeeper_unpack::unpack(path, &order, &LIMITS, &fs, host).map_err(|e| {
        Error::Failed(format!(
            "{}: hostkeeper does not unpack it: {e} ({})",
            path.display(),
            e.outcome().2
        ))
    })?;
    Ok(unpacked.dir)
}

/// The `verify-assets hostkeeper-package` verb: the line to print.
///
/// # Errors
/// A `--version` that is not a release version (a usage error); the package is missing, larger
/// than [`MAX_PACKAGE_BYTES`], carries an extended attribute, or is refused by hostkeeper's
/// unpacker; the scratch directory cannot be made.
pub fn verify_package(
    dir: &Path,
    version: &str,
    platform: Platform,
    scratch: &Path,
) -> Result<Vec<String>, Error> {
    verify_package_within(dir, version, platform, scratch, MAX_PACKAGE_BYTES)
}

/// [`verify_package`], with `max_bytes` for [`MAX_PACKAGE_BYTES`].
fn verify_package_within(
    dir: &Path,
    version: &str,
    platform: Platform,
    scratch: &Path,
    max_bytes: u64,
) -> Result<Vec<String>, Error> {
    if !is_release_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>+<12 lowercase hex digits>"
        )));
    }
    let name = file_name(version, platform);
    let path = dir.join(&name);
    let (digest, bytes) = digest_file(&path)?;
    if bytes > max_bytes {
        return Err(Error::Failed(format!(
            "{name}: {bytes} bytes, over the {max_bytes} a host fetches"
        )));
    }
    let problems = attribute_problems(&path)?;
    if !problems.is_empty() {
        return Err(Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{name}: {p}"))
                .collect(),
        ));
    }
    let sha256 = digest.as_str().to_owned();
    let unpacked = unpack(&path, version, platform, digest, scratch)?;
    Ok(vec![format!(
        "{name}: {bytes} bytes, sha256 {sha256}, no extended attribute; unpacked as a host does into {}",
        unpacked.display()
    )])
}

/// What is wrong with the index `index` (whose text is `text`) of the packages `found` of `dir`:
/// one line per problem.
fn index_problems(
    dir: &Path,
    text: &str,
    index: &ReleaseIndex,
    found: &Packages,
) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    if text != index.to_json() {
        problems.push(format!(
            "{INDEX} is not the canonical JSON of its index (keys sorted, no white space)"
        ));
    }
    for platform in Platform::ALL {
        if let Err(e) = index.entry_for(host_platform(platform)) {
            problems.push(format!(
                "{INDEX}: {e} ({}): a host of {} would be refused",
                e.reason(),
                platform.name()
            ));
        }
    }
    for entry in index.packages() {
        let platform = our_platform(entry.platform);
        let name = file_name(&found.version, platform);
        if !found.platforms.contains(&platform) {
            problems.push(format!(
                "{INDEX} lists {name}, which is not in {}",
                dir.display()
            ));
            continue;
        }
        let (digest, bytes) = digest_file(&dir.join(&name))?;
        if digest != entry.sha256 || bytes != entry.bytes {
            problems.push(format!(
                "{name}: sha256 {} and {bytes} bytes, not the sha256 {} and {} bytes {INDEX} lists (package_digest_mismatch)",
                digest.as_str(),
                entry.sha256.as_str(),
                entry.bytes
            ));
        }
    }
    Ok(problems)
}

/// The `verify-assets hostkeeper-index` verb: the lines to print.
///
/// # Errors
/// A `--commit` that is not a full SHA (a usage error); the packages of `dir` are not one version
/// of the commit; `index.json` is missing, refused by hostkeeper's reader, not canonical, lacks a
/// platform, lists a file `dir` does not hold or a size or digest other than its file's; a package
/// fails [`verify_package`].
pub fn verify_index(dir: &Path, commit: &str, scratch: &Path) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    let found = packages(dir, commit)?;
    let path = dir.join(INDEX);
    let text =
        std::fs::read_to_string(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let index = ReleaseIndex::parse(&text, &service()?, &host_version(&found.version)?)
        .map_err(index_failed)?;
    let problems = index_problems(dir, &text, &index, &found)?;
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let mut printed = vec![format!(
        "{INDEX}: tierstore {}, a package for each of {} platforms, each the size and SHA-256 of its file",
        found.version,
        Platform::ALL.len()
    )];
    for platform in Platform::ALL {
        printed.extend(verify_package(
            dir,
            &found.version,
            platform,
            &scratch.join(platform.name()),
        )?);
    }
    Ok(printed)
}

#[cfg(test)]
mod tests;
