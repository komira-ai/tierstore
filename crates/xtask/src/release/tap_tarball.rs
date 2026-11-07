//! `release tap-tarball <dir> --commit <sha> --version <X.Y.Z> --platform <platform>`: the tarball
//! the Homebrew tap's formula downloads, `<dir>/tierstore-<version>-<target>.tar.gz`, where
//! `<version>` is the release's `<major>.<minor>.<patch>` and `<target>` is `macos-arm64`,
//! `linux-aarch64` or `linux-x86_64`. The names are the ones the tap's guard accepts
//! (`.github/guard/guard.js` of `komira-ai/homebrew-tap`: `<name>-<version>-<target>.tar.gz`),
//! and the formula installs the file `tierstore` from the top of the archive.
//!
//! The tarball is a copy of `<dir>/tierstore-<commit>-<platform>.tar.gz`, which `package`
//! wrote: the same bytes, so the binary is the one the commit-named tarball holds, byte for byte,
//! and so is the licence notice beside it. The copy is refused unless the source's first entry is
//! the regular file `tierstore`. Its line is appended to `<dir>/SHA256SUMS.<platform>`
//! ([`crate::sums`]); the file may not exist already, and the sums file may not list it yet.
//!
//! Because the bytes are the source's, the SBOM attestation of the source's digest covers the
//! copy too; `artifacts.yml` signs the copy as a subject of its own and names its bundle
//! `<file>.sigstore.json`, as for every tarball.

use std::io::Read;
use std::path::Path;

use crate::Error;
use crate::build_commit::full_commit;
use crate::fetch::Sha256;
use crate::package::{Platform, asset_stem, open_sums, read, write_new, write_sums};
use crate::release::BINARY;
use crate::release::hostkeeper_package::is_plain_version;
use crate::sums;

/// The target the tap's formula names `platform` by.
pub fn target(platform: Platform) -> &'static str {
    match platform {
        Platform::LinuxX86_64 => "linux-x86_64",
        Platform::LinuxArm64 => "linux-aarch64",
        Platform::MacosArm64 => "macos-arm64",
    }
}

/// `tierstore-<version>-<target>.tar.gz`.
pub fn file_name(version: &str, platform: Platform) -> String {
    format!("{BINARY}-{version}-{}.tar.gz", target(platform))
}

/// What is wrong with `tgz` as the tarball of the binary: it is a gzip stream of a tar whose
/// first entry is the regular file [`BINARY`].
fn check_source(tgz: &[u8]) -> Result<(), String> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(tgz)
        .read_to_end(&mut tar)
        .map_err(|e| format!("not a gzip stream: {e}"))?;
    let mut archive = tar::Archive::new(&tar[..]);
    // A fresh archive can always list its entries; the first of them may not be readable.
    let first = archive
        .entries()
        .ok()
        .and_then(|mut entries| entries.next());
    let Some(Ok(entry)) = first else {
        return Err("holds no readable first entry".to_owned());
    };
    let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
    if path != BINARY || entry.header().entry_type() != tar::EntryType::Regular {
        return Err(format!(
            "starts with {path:?}, not the regular file {BINARY:?}"
        ));
    }
    Ok(())
}

/// The `release tap-tarball` verb: the line to print.
///
/// # Errors
/// A commit or version that is not of its form, a source tarball that is missing or does not
/// start with the binary, a tarball that is there already or that the sums file lists already.
pub fn tap_tarball(
    dir: &Path,
    commit: &str,
    version: &str,
    platform: Platform,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    let source = dir.join(format!("{}.tar.gz", asset_stem(BINARY, commit, platform)));
    let bytes = read(&source)?;
    check_source(&bytes).map_err(|why| Error::Failed(format!("{}: {why}", source.display())))?;
    let name = file_name(version, platform);
    let (sums_path, mut lines) = open_sums(dir, platform)?;
    if lines.iter().any(|l| l.name == name) {
        return Err(Error::Failed(format!(
            "{} already lists {name}",
            sums_path.display()
        )));
    }
    write_new(&dir.join(&name), &bytes)?;
    let sha256 = Sha256::of(&bytes);
    lines.push(sums::Line {
        sha256,
        name: name.clone(),
    });
    write_sums(&sums_path, &lines)?;
    Ok(vec![format!(
        "{name}: a copy of {}, sha256 {sha256}, {} bytes",
        source.display(),
        bytes.len()
    )])
}

#[cfg(test)]
mod tests;
