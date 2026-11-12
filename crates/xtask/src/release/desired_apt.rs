//! `release desired-apt --repo <owner/repo> --version <X.Y.Z> --min-hostkeeper <x.y.z> --dir <dir>
//! --apt-url <url> [--output <file>] [--gh <program>] [--curl <program>]`: what `release.yml` sets
//! as the desired version of tierstore on the hostkeeper coordinator for a release that is in the
//! suite `testing`, or why it sets nothing.
//!
//! The lab installs the Debian package of the release from the testing suite through hostkeeper's
//! apt installer (hostkeeper's `docs/design/packages-repositories.md`), so what the coordinator is
//! given is the `targets` of the version: for each architecture one entry of the installer `apt`,
//! naming the host's repository entry `komira-testing` (the file stem of
//! `/etc/hostkeeper/repositories/komira-testing.sources`, placed at host setup), the package
//! version (`<X.Y.Z>`, what `apt-get install tierstore=<X.Y.Z>` takes, which hostkeeper holds to
//! the `.deb`'s control `Version`) and the SHA-256 of the `.deb` as the pin. No `release_index`
//! and no `package_digest` are sent: tierstore ships no archive, and a digest alone cannot be
//! given with `targets`. The suite's `Packages` files are read from the repository as a client
//! reads them (the CDN, `--apt-url`), and the entry of each package is held to the digest the
//! release's `SHA256SUMS` lists for the file:
//!
//! - the release is a published GitHub release of `<owner/repo>` that has `SHA256SUMS`;
//! - `SHA256SUMS` lists `tierstore_<X.Y.Z>_amd64.deb` and `..._arm64.deb`;
//! - the `Packages` of `dists/testing/main/binary-<arch>` lists each, in the pool, with that digest.
//!
//! Every problem found is reported, one line each. When there is none, `--output` gets
//! `package-version=<X.Y.Z>` and `targets=<JSON>`: the proto3 JSON of hostkeeper's `TargetList`
//! per platform (`linux-x86_64` for `amd64`, `linux-arm64` for `arm64`; no `macos-arm64`, which
//! has no package here), as hostkeeper's `cargo xtask release targets` writes it for its own
//! native packages. The manifest of an entry is the update hook, its bounds and params, the run
//! units and the health URL that [`entry`] names; `--min-hostkeeper` is its `minHostkeeper`, the
//! first hostkeeper release whose daemon runs the hook command of an `apt` entry.

use std::path::Path;

use serde_json::{Value, json};

use super::deb::{BINARY_PATH, CONFIG_FILE, architecture, file_name};
use super::hostkeeper_package::is_plain_version;
use super::{Gh, append_output};
use crate::Error;
use crate::channels::apt::index::parse_packages;
use crate::channels::apt::{control, state::POOL_DIR};
use crate::channels::check::Host;
use crate::package::Platform;
use crate::sums::{self, SUMS};

/// The name of the host's repository entry of the testing suite: the file stem of
/// `/etc/hostkeeper/repositories/komira-testing.sources`.
pub const REPOSITORY: &str = "komira-testing";

/// What `release desired-apt` is asked.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub curl: &'a Path,
    pub repo: &'a str,
    pub version: &'a str,
    pub min_hostkeeper: &'a str,
    pub dir: &'a Path,
    pub apt_url: &'a str,
}

/// The `Packages` of the testing suite for an architecture.
fn packages_url(apt_url: &str, arch: &str) -> String {
    format!("{apt_url}/dists/testing/main/binary-{arch}/Packages")
}

/// The entry of the installer `apt` for the `.deb` of `version` whose SHA-256 is `digest`.
///
/// The `.deb` installs the template `tierstore@.service`, and the entry runs it as the archive
/// manifest (`packaging/hostkeeper/manifest.toml.in`) does: the run units are the instances
/// `tierstore@a.service` and `tierstore@b.service`, and the hook is `tierstore update-hook`, the
/// package's binary at its installed path ([`BINARY_PATH`]), which hostkeeper runs from the
/// version's sealed tree. The hook alternates between the instances with a hand-over of the
/// listening sockets, starts the one its record names at boot (`boot = "restore"`), and, in
/// `prepare`, has the staged release (and the previous one) check the host's configuration file,
/// the `config_file` param ([`CONFIG_FILE`]), before anything is installed: a host with no
/// configuration, or one the release refuses, fails `prepare` and starts nothing. The hook's
/// bounds and deadlines, the boot step and the health URL are the archive manifest's (a test holds
/// them equal). The template has a `[Install]` section, but hostkeeper finds no unit file named
/// after an instance in the tree, so it writes no enablement link; it starts the run units at its
/// own start instead, and only the instance with a start marker runs.
pub fn entry(version: &str, digest: &str, min_hostkeeper: &str) -> Value {
    json!({
        "installer": "apt",
        "repository": REPOSITORY,
        "packageVersion": version,
        "digest": digest,
        "manifest": {
            "package": "tierstore",
            "hook": {
                "command": [format!("/{BINARY_PATH}"), "update-hook"],
                "boot": "restore",
                "timeoutSecs": {
                    "prepare": "0", "cutover": "300", "verify": "1000",
                    "rollback": "600", "cleanup": "1300", "uninstall": "300",
                },
                "verifyDeadlineSecs": "1600",
                "promotionCeilingSecs": "3600",
                "cleanupRetries": 3,
            },
            "run": ["tierstore@a.service", "tierstore@b.service"],
            "healthUrl": "http://127.0.0.1:9300/readyz",
            "params": {
                "config_file": {"text": CONFIG_FILE},
                "service_manager": {"text": "systemd"},
            },
            "minHostkeeper": min_hostkeeper,
            "installerBounds": {"stageSecs": "600", "activateSecs": "300", "removeSecs": "300"},
        },
    })
}

/// Refuses a `--min-hostkeeper` that is not `<major>.<minor>.<patch>` or an `--apt-url` that is
/// not an https URL without a trailing `/` (usage errors), before anything is read.
///
/// # Errors
/// As above.
pub fn usage(min_hostkeeper: &str, apt_url: &str) -> Result<(), Error> {
    if !is_plain_version(min_hostkeeper) {
        return Err(Error::Usage(format!(
            "--min-hostkeeper {min_hostkeeper:?} is not <major>.<minor>.<patch>"
        )));
    }
    if !apt_url.starts_with("https://") || apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {apt_url:?} is not an https URL without a trailing /"
        )));
    }
    Ok(())
}

/// What the coordinator is given for a release in the testing suite.
#[derive(Debug)]
pub struct Found {
    /// `package-version=<X.Y.Z>` and `targets=<JSON>`, the lines of `--output`.
    pub lines: [String; 2],
    /// The line that says what was checked.
    pub summary: String,
}

/// The package version and `targets` of `r`'s release, held to the testing suite and the
/// release's `SHA256SUMS`.
///
/// # Errors
/// A `--version` or `--min-hostkeeper` that is not `<major>.<minor>.<patch>` or an `--apt-url`
/// that is not an https URL (usage errors); no published release of that version or one without
/// `SHA256SUMS`; a download that fails; or one line per package the sums do not list or the
/// testing suite does not list with the digest of the sums.
pub fn apt_targets(gh: &dyn Gh, host: &dyn Host, r: &Request) -> Result<Found, Error> {
    let Request {
        curl,
        repo,
        version,
        min_hostkeeper,
        dir,
        apt_url,
    } = *r;
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    usage(min_hostkeeper, apt_url)?;
    let tag = format!("v{version}");
    let (commit, assets) = super::desired::release(gh, repo, &tag)?;
    if !assets.iter().any(|(n, _)| n == SUMS) {
        return Err(Error::Refused(vec![format!(
            "release {tag} has no {SUMS}: there are no digests to hold the testing suite to"
        )]));
    }
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let into = dir.display().to_string();
    gh.run(&[
        "release".to_owned(),
        "download".to_owned(),
        tag.clone(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--dir".to_owned(),
        into,
        "--pattern".to_owned(),
        SUMS.to_owned(),
    ])?;
    let listed = sums::read_sums(&dir.join(SUMS))?;
    let mut problems = Vec::new();
    let mut targets = serde_json::Map::new();
    for (platform, arch) in Platform::ALL
        .into_iter()
        .filter_map(|p| architecture(p).map(|arch| (p, arch)))
    {
        let name = file_name(version, arch);
        let Some(line) = listed.iter().find(|l| l.name == name) else {
            problems.push(format!("{SUMS} of release {tag} lists no {name}"));
            continue;
        };
        let file = dir.join(format!("Packages-{arch}"));
        let file_arg = file.display().to_string();
        let url = packages_url(apt_url, arch);
        let answer = host.run(
            curl,
            &[
                "--proto".to_owned(),
                "=https".to_owned(),
                "--tlsv1.2".to_owned(),
                "--fail".to_owned(),
                "--silent".to_owned(),
                "--show-error".to_owned(),
                "--retry".to_owned(),
                "5".to_owned(),
                "--retry-delay".to_owned(),
                "20".to_owned(),
                "--retry-all-errors".to_owned(),
                "--max-time".to_owned(),
                "60".to_owned(),
                "--output".to_owned(),
                file_arg,
                url.clone(),
            ],
        )?;
        if !answer.ok {
            problems.push(format!(
                "{url} could not be read: {}",
                answer.stderr.trim_end()
            ));
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(Error::spawn(format!("read {}", file.display())))?;
        let stanzas = parse_packages(&text)
            .map_err(|e| Error::Failed(format!("{url} is not a Packages file: {e}")))?;
        let filename = format!("{POOL_DIR}/{name}");
        let entry_of_name = stanzas.iter().find(|s| {
            control::parse(&s.text)
                .is_ok_and(|f| control::get(&f, "Filename") == Some(filename.as_str()))
        });
        match entry_of_name {
            None => problems.push(format!(
                "the testing suite lists no {name}: the release is not published there"
            )),
            Some(s) if s.sha256 != line.sha256.to_string() => problems.push(format!(
                "the testing suite lists {name} with sha256 {}, but {SUMS} lists {}",
                s.sha256, line.sha256
            )),
            Some(_) => {
                targets.insert(
                    platform.name().to_owned(),
                    json!({"entries": [entry(version, &line.sha256.to_string(), min_hostkeeper)]}),
                );
            }
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let count = targets.len();
    Ok(Found {
        lines: [
            format!("package-version={version}"),
            format!("targets={}", Value::Object(targets)),
        ],
        summary: format!(
            "release {tag} ({commit}): the testing suite lists tierstore {version} for each of {count} architectures, each with the digest {SUMS} lists"
        ),
    })
}

/// The `release desired-apt` verb: the lines to print.
///
/// # Errors
/// Those of [`apt_targets`], and `--output` cannot be appended to.
pub fn desired_apt(
    gh: &dyn Gh,
    host: &dyn Host,
    r: &Request,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let found = apt_targets(gh, host, r)?;
    if let Some(output) = output {
        append_output(output, &found.lines)?;
    }
    let mut printed = vec![found.summary];
    printed.extend(found.lines);
    Ok(printed)
}

#[cfg(test)]
mod tests;
