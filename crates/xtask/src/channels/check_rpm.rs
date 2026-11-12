//! `channels check-rpm`: the release installed the way a user installs it with `dnf`, from what was
//! published to the suite `testing`, before anything public changes
//! (`docs/design/distribution.md`, section 5.4). `.github/workflows/release.yml` runs this verb in
//! the job `check-rpm`, on a hosted runner of each architecture and in a clean container of each of
//! three distributions (Fedora, AlmaLinux 9 and Amazon Linux 2023, pinned by digest).
//!
//! The verb runs on the runner and the system it checks is the container: every program runs in it
//! with `docker exec <container>` ([`Container`]). (The `xtask` binary of the runner is built against
//! a newer C library than these distributions have, so it cannot run in them.) A test hands
//! [`super::check::Host`] a stand-in instead.
//!
//! In the container it writes the repository file of the suite `testing` (`gpgcheck=1` and
//! `repo_gpgcheck=1`, the key at `<url>/apt/komira-archive-keyring.asc`, the same file `komira.repo`
//! has for its testing section, with that one enabled), installs `tierstore-<version>` with `dnf`
//! (which verifies the signature of the repository metadata and of the package, and imports the
//! key), and then holds the installed system to what the package promises: the package is at the
//! version and release, `rpm -K` of the package downloaded from the pool says its digests and
//! signatures are OK, the binary says `tierstore <version>+<commit>`, the unit template, the
//! `sysusers.d` entry and the example configuration are there, `rpm -qc` lists no configuration
//! file (nothing in `/etc` is the package's), `/etc/tierstore` is a directory of mode 0750 with no
//! `config.toml` in it (the live configuration is the host's), no start marker of an instance was
//! written, and no instance is enabled.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::aws::Answer;
use super::check::{
    BINARY, CONFIG, CONFIG_DIR, EXAMPLE, Host, MARKER_DIR, SYSUSERS, UNIT, UNITS, must, says,
    version_flag,
};
use crate::Error;

/// The package.
const PACKAGE: &str = "tierstore";

/// The file of the repository in the container.
pub const REPO_FILE: &str = "/etc/yum.repos.d/komira-testing.repo";

/// The key's path on the repository host (the same key signs the apt and the rpm tree).
pub const KEY_PATH: &str = "apt/komira-archive-keyring.asc";

/// A container: every program runs in it.
pub struct Container {
    pub docker: PathBuf,
    pub name: String,
}

impl Host for Container {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(&self.docker)
            .args(["exec", &self.name])
            .arg(program)
            .args(args)
            .output()
            .map_err(Error::spawn(self.docker.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// What `check-rpm` is asked.
pub struct Rpm<'a> {
    /// The RPM architecture of the machine, `x86_64` or `aarch64`.
    pub arch: &'a str,
    pub version: &'a str,
    pub commit: &'a str,
    /// The repository host, `https://packages.komira.ai`.
    pub url: &'a str,
    /// A directory of the container to download the package to.
    pub work: &'a str,
}

/// The repository file of the suite `testing`: signed metadata and signed packages, no other.
#[must_use]
pub fn repo_file(url: &str) -> String {
    format!(
        "[komira-testing]\nname=komira-ai packages (testing)\nbaseurl={url}/rpm/testing/$basearch\nenabled=1\ngpgcheck=1\nrepo_gpgcheck=1\ngpgkey={url}/{KEY_PATH}\n"
    )
}

/// The programs are found on the container's `PATH`.
fn program(name: &str) -> &Path {
    Path::new(name)
}

/// The file name of the package of the release.
#[must_use]
pub fn file_name(version: &str, arch: &str) -> String {
    crate::release::rpm::file_name(version, arch)
}

/// The `check-rpm` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a program that fails (naming it and what it said), a machine of another
/// architecture, a package whose signature `rpm -K` does not accept, a binary that does not say
/// the release, or one problem per way the installed system is not what the package promises.
pub fn check_rpm(host: &dyn Host, r: &Rpm) -> Result<Vec<String>, Error> {
    version_flag(r.version)?;
    if !["x86_64", "aarch64"].contains(&r.arch) {
        return Err(Error::Usage(format!(
            "--arch {:?} is not x86_64 or aarch64",
            r.arch
        )));
    }
    if !r.url.starts_with("https://") || r.url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--url {:?} is not an https URL without a trailing /",
            r.url
        )));
    }
    if !r.work.starts_with('/') || r.work.ends_with('/') {
        return Err(Error::Usage(format!(
            "--work {:?} is not an absolute path without a trailing /",
            r.work
        )));
    }
    let machine = must(host, program("uname"), &["-m"])?;
    if machine.trim() != r.arch {
        return Err(Error::Failed(format!(
            "this machine is {}, not {}: the check would install another package",
            machine.trim(),
            r.arch
        )));
    }
    must(
        host,
        program("sh"),
        &[
            "-c",
            "printf '%s' \"$1\" > \"$2\"",
            "sh",
            &repo_file(r.url),
            REPO_FILE,
        ],
    )?;
    let target = format!("{PACKAGE}-{}", r.version);
    must(host, program("dnf"), &["install", "-y", &target])?;
    let installed = must(
        host,
        program("rpm"),
        &["-q", "--qf", "%{VERSION}-%{RELEASE}.%{ARCH}", PACKAGE],
    )?;
    let file = file_name(r.version, r.arch);
    let wanted = format!("{}-{}.{}", r.version, crate::release::rpm::RELEASE, r.arch);
    let mut problems = Vec::new();
    if installed != wanted {
        problems.push(format!(
            "rpm says {PACKAGE} {installed:?} is installed, not {wanted:?}"
        ));
    }
    let key = format!("{}/{KEY_PATH}", r.url);
    must(host, program("rpm"), &["--import", &key])?;
    let local = format!("{}/{file}", r.work);
    must(
        host,
        program("curl"),
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--output",
            &local,
            &format!("{}/rpm/{}/{file}", r.url, super::rpm::state::POOL_DIR),
        ],
    )?;
    let checked = must(host, program("rpm"), &["-K", &local])?;
    if !checked.contains("signatures OK") || checked.contains("NOT OK") {
        problems.push(format!(
            "rpm -K says {:?}, not that the digests and signatures are OK",
            checked.trim()
        ));
    }
    let said = says(host, Path::new(&format!("/{BINARY}")), r.version, r.commit)?;
    for (what, path) in [
        ("the unit template", UNIT),
        ("the sysusers.d entry", SYSUSERS),
        ("the example configuration", EXAMPLE),
    ] {
        let args = ["-c", "test -f \"$1\"", "sh", &format!("/{path}")].map(str::to_owned);
        if !host.run(program("sh"), &args)?.ok {
            problems.push(format!("{what} /{path} is not installed"));
        }
    }
    // `rpm -qc` says `(contains no files)` for a package with no configuration file.
    let conffiles = must(host, program("rpm"), &["-qc", PACKAGE])?;
    for file in conffiles.lines().filter(|l| l.starts_with('/')) {
        problems.push(format!(
            "{file} is a configuration file of the package, and nothing in /etc is the package's"
        ));
    }
    let mode = must(
        host,
        program("sh"),
        &[
            "-c",
            "if [ -d \"$1\" ] && [ ! -L \"$1\" ]; then stat -c %a \"$1\"; fi",
            "sh",
            &format!("/{CONFIG_DIR}"),
        ],
    )?;
    match mode.trim() {
        "750" => {}
        "" => problems.push(format!("/{CONFIG_DIR} is not a directory the package made")),
        other => problems.push(format!("/{CONFIG_DIR} has mode {other}, not 750")),
    }
    let left = must(
        host,
        program("sh"),
        &[
            "-c",
            "ls -d \"$1\" 2>/dev/null; ls -A \"$2\" 2>/dev/null; true",
            "sh",
            &format!("/{CONFIG}"),
            &format!("/{MARKER_DIR}"),
        ],
    )?;
    for line in left.lines().filter(|l| !l.is_empty()) {
        if line == format!("/{CONFIG}") {
            problems.push(format!(
                "/{CONFIG} is there, and the package never writes the host's configuration"
            ));
        } else {
            problems.push(format!(
                "/{MARKER_DIR}/{line} is there, and the package writes no start marker"
            ));
        }
    }
    let listing = UNITS
        .iter()
        .map(|u| format!("/etc/systemd/system/*.wants/{u} /etc/systemd/system/{u}"))
        .collect::<Vec<_>>()
        .join(" ");
    let enabled = must(
        host,
        program("sh"),
        &["-c", &format!("ls -d {listing} 2>/dev/null; true")],
    )?;
    for link in enabled.lines() {
        problems.push(format!(
            "the service is enabled: {link} exists, and the package enables nothing"
        ));
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(vec![format!(
        "check-rpm: {PACKAGE} {} installed from the suite testing on {}; rpm -K says OK; it says {said:?}; the unit template, the sysusers.d entry and the example configuration are there; no configuration file is the package's; /{CONFIG_DIR} is 750 with no config.toml and no start marker was written; no instance is enabled",
        r.version, r.arch
    )])
}

#[cfg(test)]
mod tests;
