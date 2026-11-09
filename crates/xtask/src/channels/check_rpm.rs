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
//! signatures are OK, the binary says `tierstore <version>+<commit>`, the unit, the `sysusers.d`
//! entry and the conffile are there (the conffile registered as one with `rpm -qc`), and the
//! service is not enabled.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::aws::Answer;
use super::check::{BINARY, CONFFILE, Host, SYSUSERS, UNIT, must, says, version_flag};
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
        ("the unit", UNIT),
        ("the sysusers.d entry", SYSUSERS),
        ("the conffile", CONFFILE),
    ] {
        let args = ["-c", "test -f \"$1\"", "sh", &format!("/{path}")].map(str::to_owned);
        if !host.run(program("sh"), &args)?.ok {
            problems.push(format!("{what} /{path} is not installed"));
        }
    }
    let conffiles = must(host, program("rpm"), &["-qc", PACKAGE])?;
    if !conffiles.lines().any(|l| l == format!("/{CONFFILE}")) {
        problems.push(format!(
            "/{CONFFILE} is not registered as a configuration file of the package"
        ));
    }
    let enabled = must(
        host,
        program("sh"),
        &[
            "-c",
            "ls /etc/systemd/system/*.wants/tierstore.service /etc/systemd/system/tierstore.service 2>/dev/null; true",
        ],
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
        "check-rpm: {PACKAGE} {} installed from the suite testing on {}; rpm -K says OK; it says {said:?}; the unit, the sysusers.d entry and the conffile are there; the service is not enabled",
        r.version, r.arch
    )])
}

#[cfg(test)]
mod tests;
