//! `channels check-apt|check-brew|check-install-sh`: the release installed the way a user installs
//! it, from what was published to testing, before anything public changes
//! (`docs/design/distribution.md`, section 5.2). `.github/workflows/release.yml` runs these verbs in
//! its install-check jobs, on hosted runners and at release time only.
//!
//! - `check-apt` runs in a clean Ubuntu container (the image is pinned by digest by the job): it
//!   brings the image's packages up to date (the pinned image is older than the archive), adds
//!   `packages.komira.ai` with the published keyring (a binary `.gpg`, referred to by a
//!   deb822 `Signed-By`) and the suite `testing`, removes the image's dpkg drop-in that leaves out
//!   `/usr/share/doc` ([`DPKG_EXCLUDES`]) just before it installs `tierstore=<version>`, and then holds
//!   the installed system to what the package promises: the package is at the version, the binary
//!   says `tierstore <version>+<commit>`, the unit template, the `sysusers.d` entry and the example
//!   configuration are there, `/etc/tierstore` is a directory of mode 0750, the package wrote no
//!   `/etc/tierstore/config.toml` (the live configuration is the host's) and no start marker of an
//!   instance, and no instance is enabled or running.
//! - `check-brew` installs the rendered formula from a local tap (`brew` refuses a bare path) and
//!   runs the binary it installed.
//! - `check-install-sh` downloads `install.sh` from the site, installs the version with it into a
//!   scratch prefix and runs the binary it installed.
//!
//! The programs (`apt-get`, `dpkg-query`, `curl`, `brew`, `sh`, the installed binary) run behind
//! [`Host`], so a test answers in place of them; `--root` is where the checked system is (`/`),
//! so a test points it at a scratch directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::aws::Answer;
use crate::Error;
use crate::release::check_said;
use crate::release::deb;
use crate::release::hostkeeper_package::is_plain_version;

/// The package and the program it installs.
const PACKAGE: &str = "tierstore";

/// Where the package puts the binary (below the root).
pub const BINARY: &str = deb::BINARY_PATH;

/// The unit template, the `sysusers.d` entry and the example configuration of the package (below
/// the root).
pub const UNIT: &str = deb::UNIT_PATH;
pub const SYSUSERS: &str = "usr/lib/sysusers.d/tierstore.conf";
pub const EXAMPLE: &str = deb::EXAMPLE_PATH;

/// The directory of the host's configuration, which the scripts make (below the root).
pub const CONFIG_DIR: &str = "etc/tierstore";

/// The host's configuration file (below the root), which the package must not write.
pub const CONFIG: &str = "etc/tierstore/config.toml";

/// The directory of the start markers of the instances (below the root), which the package must
/// not write a marker into.
pub const MARKER_DIR: &str = "var/lib/tierstore";

/// The units of the package: the template and its two instances.
pub const UNITS: [&str; 3] = [
    "tierstore@.service",
    "tierstore@a.service",
    "tierstore@b.service",
];

/// The dpkg drop-in of a minimized Ubuntu image (below the root), the one the job's
/// `ubuntu:24.04` has: its `path-exclude=/usr/share/doc/*` keeps dpkg from unpacking everything
/// below `/usr/share/doc` but the copyright files, the package's example configuration among it.
/// A standard Ubuntu install has no such file, so `check-apt` removes it before it installs the
/// package, and the files it then holds the install to are the ones a standard install gets.
pub const DPKG_EXCLUDES: &str = "etc/dpkg/dpkg.cfg.d/excludes";

/// Where the keyring is put (below the root) and the name the source file refers to it by.
pub const KEYRING: &str = "usr/share/keyrings/komira-archive-keyring.gpg";
const KEYRING_PATH: &str = "/usr/share/keyrings/komira-archive-keyring.gpg";

/// The source file (below the root).
pub const SOURCES: &str = "etc/apt/sources.list.d/komira.sources";

/// The keyring's file name in the repository.
pub const KEYRING_NAME: &str = "komira-archive-keyring.gpg";

/// The programs outside, behind a trait so a test answers in place of them.
pub trait Host {
    /// `program <args>`. A refusal is an [`Answer`] that is not `ok`.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error>;
}

/// The real programs.
pub struct RealHost;

impl Host for RealHost {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(program)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .map_err(Error::spawn(program.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Runs `program <args>`; its standard output, or why it failed.
pub(super) fn must(host: &dyn Host, program: &Path, args: &[&str]) -> Result<String, Error> {
    let answer = host.run(program, &strings(args))?;
    if answer.ok {
        return Ok(answer.stdout);
    }
    Err(Error::Failed(format!(
        "{} {} failed: {}",
        program.display(),
        args.join(" "),
        answer.stderr.trim_end()
    )))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let what = || format!("write {}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(Error::spawn(what()))?;
    std::fs::write(path, bytes).map_err(Error::spawn(what()))
}

pub(super) fn version_flag(version: &str) -> Result<(), Error> {
    if is_plain_version(version) {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )))
    }
}

/// The binary at `binary` says the release it is: `tierstore <version>+<12 digits of commit>`.
pub(super) fn says(
    host: &dyn Host,
    binary: &Path,
    version: &str,
    commit: &str,
) -> Result<String, Error> {
    let said = must(host, binary, &["--version"])?;
    check_said(&said, version, commit)?;
    Ok(said.trim_end().to_owned())
}

/// What `check-apt` is asked.
pub struct Apt<'a> {
    /// The system to check: `/`, or a scratch directory in a test.
    pub root: &'a Path,
    /// The Debian architecture of the machine, `amd64` or `arm64`.
    pub arch: &'a str,
    pub version: &'a str,
    pub commit: &'a str,
    /// The repository, `https://packages.komira.ai/apt`.
    pub apt_url: &'a str,
    pub apt_get: &'a Path,
    pub dpkg_query: &'a Path,
    pub dpkg: &'a Path,
    pub curl: &'a Path,
}

/// The deb822 source of the suite `testing`, signed by the keyring alone.
#[must_use]
pub fn source(apt_url: &str, arch: &str) -> String {
    format!(
        "Types: deb\nURIs: {apt_url}\nSuites: testing\nComponents: main\nArchitectures: {arch}\nSigned-By: {KEYRING_PATH}\n"
    )
}

/// The service entries of systemd's `wants` or `requires` directories, and the units of the
/// administrator, for the package's template or one of its instances, below `etc/systemd/system`.
pub(super) fn enabled_links(root: &Path) -> Vec<PathBuf> {
    let base = root.join("etc/systemd/system");
    let mut found = Vec::new();
    for unit in UNITS {
        let unit = base.join(unit);
        if unit.symlink_metadata().is_ok() {
            found.push(unit);
        }
    }
    for dir in std::fs::read_dir(&base).into_iter().flatten().flatten() {
        for unit in UNITS {
            let link = dir.path().join(unit);
            if dir.path().is_dir() && link.symlink_metadata().is_ok() {
                found.push(link);
            }
        }
    }
    found.sort();
    found
}

/// The processes (by `comm`) named `tierstore` below `root/proc`.
fn running(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.join("proc"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim_end() == PACKAGE {
            found.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    found
}

/// The checks of an installed system, as problems.
fn installed_problems(r: &Apt, installed: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if installed != r.version {
        problems.push(format!(
            "dpkg says {PACKAGE} {installed:?} is installed, not {:?}",
            r.version
        ));
    }
    for (what, rel) in [
        ("the unit template", UNIT),
        ("the sysusers.d entry", SYSUSERS),
        ("the example configuration", EXAMPLE),
    ] {
        if !r.root.join(rel).is_file() {
            problems.push(format!("{what} /{rel} is not installed"));
        }
    }
    problems.extend(config_problems(r.root));
    for link in enabled_links(r.root) {
        problems.push(format!(
            "the service is enabled: {} exists, and the package enables nothing",
            link.display()
        ));
    }
    for pid in running(r.root) {
        problems.push(format!(
            "the service is running (process {pid}), and the package starts nothing"
        ));
    }
    problems
}

/// What the scripts and the package left of the host's configuration and the markers below `root`,
/// as problems: `/etc/tierstore` is a directory (not a link) of mode 0750, and neither
/// `/etc/tierstore/config.toml` nor anything in `/var/lib/tierstore` is there.
pub(super) fn config_problems(root: &Path) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut problems = Vec::new();
    match root.join(CONFIG_DIR).symlink_metadata() {
        Ok(meta) if meta.is_dir() && meta.permissions().mode() & 0o7777 == 0o750 => {}
        Ok(meta) if meta.is_dir() => problems.push(format!(
            "/{CONFIG_DIR} has mode {:04o}, not 0750",
            meta.permissions().mode() & 0o7777
        )),
        Ok(_) => problems.push(format!("/{CONFIG_DIR} is not a directory")),
        Err(_) => problems.push(format!("/{CONFIG_DIR} was not made")),
    }
    if root.join(CONFIG).symlink_metadata().is_ok() {
        problems.push(format!(
            "/{CONFIG} is there, and the package never writes the host's configuration"
        ));
    }
    for entry in std::fs::read_dir(root.join(MARKER_DIR))
        .into_iter()
        .flatten()
        .flatten()
    {
        problems.push(format!(
            "{} is there, and the package writes no start marker",
            Path::new("/")
                .join(MARKER_DIR)
                .join(entry.file_name())
                .display()
        ));
    }
    problems
}

/// The `check-apt` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a program that fails (naming it and what it said), a keyring that is
/// not a binary keyring, a machine of another architecture, a binary that does not say the
/// release, or one problem per way the installed system is not what the package promises.
pub fn check_apt(host: &dyn Host, r: &Apt) -> Result<Vec<String>, Error> {
    version_flag(r.version)?;
    if !["amd64", "arm64"].contains(&r.arch) {
        return Err(Error::Usage(format!(
            "--arch {:?} is not amd64 or arm64",
            r.arch
        )));
    }
    if !r.apt_url.starts_with("https://") || r.apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {:?} is not an https URL without a trailing /",
            r.apt_url
        )));
    }
    let machine = must(host, r.dpkg, &["--print-architecture"])?;
    if machine.trim() != r.arch {
        return Err(Error::Failed(format!(
            "this machine is {}, not {}: the check would install another package",
            machine.trim(),
            r.arch
        )));
    }
    // A clean image has no certificates: the first update goes over the mirrors' plain HTTP.
    // The image is pinned by digest, so its packages are as old as the pin: the archive has moved
    // on, and `ca-certificates` and `curl` may need newer versions of packages the image has
    // ("you have held broken packages"). So the installed packages are brought up to date before
    // anything is installed. `dist-upgrade` may install or remove packages to do so, which
    // `upgrade` never does; `--force-confold` keeps the image's configuration files, as there
    // is nobody to answer dpkg's question.
    must(host, r.apt_get, &["update"])?;
    must(
        host,
        r.apt_get,
        &[
            "dist-upgrade",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confold",
        ],
    )?;
    must(
        host,
        r.apt_get,
        &[
            "install",
            "-y",
            "--no-install-recommends",
            "ca-certificates",
            "curl",
        ],
    )?;
    let keyring = r.root.join(KEYRING);
    let keyring_arg = keyring.display().to_string();
    must(
        host,
        r.curl,
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--create-dirs",
            "--output",
            &keyring_arg,
            &format!("{}/{KEYRING_NAME}", r.apt_url),
        ],
    )?;
    let bytes =
        std::fs::read(&keyring).map_err(Error::spawn(format!("read {}", keyring.display())))?;
    // A binary OpenPGP keyring starts with a packet header (bit 7 set); an armored one is text
    // that a deb822 `Signed-By` path does not accept.
    if bytes.first().is_none_or(|b| b & 0x80 == 0) {
        return Err(Error::Failed(format!(
            "{KEYRING_NAME} is not a binary keyring: apt takes a .gpg file in Signed-By, not armor"
        )));
    }
    write(&r.root.join(SOURCES), source(r.apt_url, r.arch).as_bytes())?;
    must(host, r.apt_get, &["update"])?;
    let excludes = r.root.join(DPKG_EXCLUDES);
    match std::fs::remove_file(&excludes) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(Error::spawn(format!("remove {}", excludes.display()))(e));
        }
        _ => {}
    }
    let target = format!("{PACKAGE}={}", r.version);
    must(
        host,
        r.apt_get,
        &["install", "-y", "--no-install-recommends", &target],
    )?;
    let installed = must(
        host,
        r.dpkg_query,
        &["--show", "--showformat=${Version}", PACKAGE],
    )?;
    let binary = r.root.join(BINARY);
    let said = says(host, &binary, r.version, r.commit)?;
    let problems = installed_problems(r, &installed);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(vec![format!(
        "check-apt: {PACKAGE} {} installed from the suite testing on {}; it says {said:?}; the unit template, the sysusers.d entry and the example configuration are there; /{CONFIG_DIR} is 0750 with no config.toml and no start marker was written; no instance is enabled or running",
        r.version, r.arch
    )])
}

/// What `check-brew` is asked.
pub struct Brew<'a> {
    /// The formula `channels formula` rendered.
    pub formula: &'a Path,
    pub version: &'a str,
    pub commit: &'a str,
    /// The local tap, `<owner>/<name>`.
    pub tap: &'a str,
    pub brew: &'a Path,
}

/// The `check-brew` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a formula that is not there, or a program that fails or a binary that
/// does not say the release.
pub fn check_brew(host: &dyn Host, r: &Brew) -> Result<Vec<String>, Error> {
    version_flag(r.version)?;
    if r.tap.split('/').count() != 2 || r.tap.split('/').any(str::is_empty) {
        return Err(Error::Usage(format!(
            "--tap {:?} is not <owner>/<name>",
            r.tap
        )));
    }
    let formula =
        std::fs::read(r.formula).map_err(Error::spawn(format!("read {}", r.formula.display())))?;
    must(host, r.brew, &["tap-new", "--no-git", r.tap])?;
    let repository = must(host, r.brew, &["--repository", r.tap])?;
    write(
        &Path::new(repository.trim())
            .join("Formula")
            .join("tierstore.rb"),
        &formula,
    )?;
    let name = format!("{}/{PACKAGE}", r.tap);
    must(host, r.brew, &["install", "--formula", &name])?;
    let prefix = must(host, r.brew, &["--prefix"])?;
    let said = says(
        host,
        &Path::new(prefix.trim()).join("bin").join(PACKAGE),
        r.version,
        r.commit,
    )?;
    Ok(vec![format!(
        "check-brew: {name} installed from the rendered formula; it says {said:?}"
    )])
}

/// What `check-install-sh` is asked.
pub struct InstallSh<'a> {
    pub version: &'a str,
    pub commit: &'a str,
    /// The site, `https://tierstore.dev`.
    pub url: &'a str,
    /// The scratch directory the script is saved in and installs into.
    pub work: &'a Path,
    pub curl: &'a Path,
    pub sh: &'a Path,
}

/// The `check-install-sh` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a script that cannot be downloaded or fails (what it said), or a
/// binary that is not installed or does not say the release.
pub fn check_install_sh(host: &dyn Host, r: &InstallSh) -> Result<Vec<String>, Error> {
    version_flag(r.version)?;
    if !r.url.starts_with("https://") || r.url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--url {:?} is not an https URL without a trailing /",
            r.url
        )));
    }
    let script = r.work.join("install.sh");
    let prefix = r.work.join("bin");
    let (script_arg, prefix_arg) = (script.display().to_string(), prefix.display().to_string());
    must(
        host,
        r.curl,
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--create-dirs",
            "--output",
            &script_arg,
            &format!("{}/install.sh", r.url),
        ],
    )?;
    must(
        host,
        r.sh,
        &[
            &script_arg,
            "--version",
            r.version,
            "--prefix",
            &prefix_arg,
            "--base-url",
            r.url,
        ],
    )?;
    let said = says(host, &prefix.join(PACKAGE), r.version, r.commit)?;
    Ok(vec![format!(
        "check-install-sh: install.sh from {} installed {}; it says {said:?}",
        r.url, r.version
    )])
}

#[cfg(test)]
mod tests;
