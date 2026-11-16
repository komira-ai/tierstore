//! The `macos-arm64` target of `release desired --macos-pkg`: one `macos-pkg` entry of the signed
//! `.pkg` the release holds, `tierstore-<X.Y.Z>-macos-arm64.pkg`.
//!
//! The entry is hostkeeper's named-version form: it names the host's repository entry (the one
//! the apt entries name, [`REPOSITORY`]), the version, the SHA-256 and the size of the file, and
//! the manifest ([`manifest_json`]); the coordinator turns it into the location
//! `pkg/tierstore/<X.Y.Z>/<file>` of the repository. The file is downloaded from the release and
//! held to the digest the release's `SHA256SUMS` lists for it, so the digest sent is the one the
//! release attests and `testing-pkg-upload` of `release.yml` published.

use std::path::Path;

use serde_json::{Value, json};

use super::desired_apt::REPOSITORY;
use super::macos_pkg::manifest_json;
use super::{Gh, macos_pkg_name};
use crate::Error;
use crate::fetch::Sha256;
use crate::sums::SUMS;

/// The platform of the entry.
pub const PLATFORM: &str = "macos-arm64";

/// The entry of the installer `macos-pkg` for the `.pkg` of `version` of `size` bytes whose
/// SHA-256 is `digest`. `bytes` is a string: it is an `int64` of hostkeeper's `release.proto`,
/// which its JSON writes as a string.
pub fn entry(version: &str, digest: &str, size: u64, min_hostkeeper: &str) -> Value {
    json!({
        "installer": "macos-pkg",
        "repository": REPOSITORY,
        "packageVersion": version,
        "digest": digest,
        "bytes": size.to_string(),
        "manifest": manifest_json(min_hostkeeper),
    })
}

/// The entry of the release `tag` of `repo`: its `.pkg` downloaded into `dir`, which already
/// holds the release's `SHA256SUMS`, and held to the digest that lists.
///
/// # Errors
/// `SHA256SUMS` lists no `.pkg` of the version; the download fails; the downloaded file cannot be
/// read, or has other bytes than `SHA256SUMS` lists.
pub fn macos_entry(
    gh: &dyn Gh,
    repo: &str,
    tag: &str,
    version: &str,
    dir: &Path,
    min_hostkeeper: &str,
) -> Result<Value, Error> {
    let name = macos_pkg_name(version);
    let listed = crate::sums::read_sums(&dir.join(SUMS))?;
    let Some(line) = listed.iter().find(|l| l.name == name) else {
        return Err(Error::Refused(vec![format!(
            "{SUMS} of release {tag} lists no {name}: the release has no macOS installer"
        )]));
    };
    gh.run(&[
        "release".to_owned(),
        "download".to_owned(),
        tag.to_owned(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--dir".to_owned(),
        dir.display().to_string(),
        "--pattern".to_owned(),
        name.clone(),
    ])?;
    let path = dir.join(&name);
    let bytes = std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let digest = Sha256::of(&bytes);
    if digest != line.sha256 {
        return Err(Error::Refused(vec![format!(
            "{name} of release {tag} has sha256 {digest}, but {SUMS} lists {}",
            line.sha256
        )]));
    }
    Ok(entry(
        version,
        &digest.to_string(),
        bytes.len() as u64,
        min_hostkeeper,
    ))
}

#[cfg(test)]
mod tests;
