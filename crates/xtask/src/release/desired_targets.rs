//! The `targets` of a release of tierstore, as `.github/workflows/hostkeeper-desired.yml` gives
//! them to hostkeeper's CI client (`set-desired.yml`, komira-ai/hostkeeper,
//! `docs/deploy/ci-client.md`, "Targets"): one JSON object keyed by platform, each value
//! `{"entries": [...]}`, the entries in the order tierstore prefers them.
//!
//! Today each platform's list is one `hostkeeper-archive` entry, the package the release index
//! lists for it, so the index and the targets name the same archives, as the coordinator requires
//! of a set that gives both (`index_invalid` otherwise):
//!
//! ```text
//! installer    hostkeeper-archive
//! locations    channel  tierstore/<V>/tierstore-<V>-<platform>.tar.zst   (the release channel)
//!              github   <repo>/releases/download/v<X.Y.Z>/tierstore-<V>-<platform>.tar.zst
//! digest       the archive's SHA-256, bytes its size (the index's, measured again)
//! attestation  the archive's `.sigstore.json` bundle: its SHA-256, its size, the same two
//!              stores at the bundle's path
//! manifest     { minHostkeeper }: the `min_hostkeeper` of the archive's own manifest.toml
//! ```
//!
//! `<V>` is the packages' version, `X.Y.Z+<12 hex>`. The `channel` location comes first: it is
//! the store a daemon without `stores.toml` has; a daemon skips a store it does not define.
//!
//! An `apt` entry joins the Linux lists, ahead of the archive, once hostkeeper's apt installer
//! ships (komira-ai/hostkeeper, `docs/design/packages-repositories.md`, A7): a host whose daemon
//! runs that installer is then ordered the `.deb` of the suite tierstore publishes, and every
//! other host keeps the archive. Until then a Linux list is the archive alone, as is the macOS
//! list for good.
//!
//! The JSON is proto3 canonical JSON of hostkeeper's `TargetList` (`hostkeeper.release.v1`):
//! lower camel case names, and every `uint64` as a decimal string.

use std::collections::BTreeMap;

use hostkeeper_config::index::{attestation_path, package_path};
use serde_json::{Value, json};

use super::hostkeeper_index::{host_platform, service, version as host_version};
use super::hostkeeper_package::file_name;
use crate::Error;
use crate::bundles;
use crate::package::Platform;

/// The installer of every entry today.
pub const ARCHIVE_INSTALLER: &str = "hostkeeper-archive";
/// The store of the release channel: the one a daemon without `stores.toml` has.
pub const CHANNEL_STORE: &str = "channel";
/// The store of GitHub's release assets, as a host's `stores.toml` names it.
pub const GITHUB_STORE: &str = "github";

/// One platform's package of the release, as measured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    pub platform: Platform,
    /// The archive's SHA-256, 64 lower-case hex digits.
    pub digest: String,
    pub bytes: u64,
    /// The SHA-256 of the archive's `.sigstore.json` bundle.
    pub bundle_digest: String,
    pub bundle_bytes: u64,
    /// The `min_hostkeeper` of the archive's manifest.
    pub min_hostkeeper: String,
}

/// Where a release's files are: the repository (`owner/name`), the tag and the packages'
/// version.
#[derive(Clone, Copy, Debug)]
pub struct Release<'a> {
    pub repo: &'a str,
    pub tag: &'a str,
    pub version: &'a str,
}

/// The two locations of a file: its path in the release channel, then its release asset.
fn locations(release: Release<'_>, channel_path: &str, name: &str) -> Value {
    json!([
        { "store": CHANNEL_STORE, "locator": channel_path },
        {
            "store": GITHUB_STORE,
            "locator": format!("{}/releases/download/{}/{name}", release.repo, release.tag),
        },
    ])
}

/// The `hostkeeper-archive` entry of `archive`.
fn archive_entry(release: Release<'_>, archive: &Archive) -> Result<Value, Error> {
    let service = service()?;
    let version = host_version(release.version)?;
    let platform = host_platform(archive.platform);
    let name = file_name(release.version, archive.platform);
    let bundle = bundles::name_of(&name);
    Ok(json!({
        "installer": ARCHIVE_INSTALLER,
        "locations": locations(release, &package_path(&service, &version, platform), &name),
        "digest": archive.digest,
        "bytes": archive.bytes.to_string(),
        "attestation": {
            "digest": archive.bundle_digest,
            "bytes": archive.bundle_bytes.to_string(),
            "locations": locations(release, &attestation_path(&service, &version, platform), &bundle),
        },
        "manifest": { "minHostkeeper": archive.min_hostkeeper },
    }))
}

/// The targets of `release` whose packages are `archives`, as one line of JSON keyed by platform.
///
/// # Errors
/// The packages' version is not one hostkeeper takes (never for a version the index was read
/// with).
pub fn targets(release: Release<'_>, archives: &[Archive]) -> Result<String, Error> {
    let mut lists = BTreeMap::new();
    for archive in archives {
        // One entry today; on Linux the apt entry goes before it once hostkeeper's apt
        // installer ships (the module's comment).
        let entries = vec![archive_entry(release, archive)?];
        lists.insert(archive.platform.name(), json!({ "entries": entries }));
    }
    Ok(json!(lists).to_string())
}

#[cfg(test)]
mod tests;
