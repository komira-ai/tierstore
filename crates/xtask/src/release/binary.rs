//! `release check-binary`: the linux-x86_64 binary of a release says the tag it is released
//! under and the commit it was built from.
//!
//! `tierstore --version` prints `tierstore <version>+<commit>`. The build gives the binary its
//! version (`TIERSTORE_BUILD_VERSION_OVERRIDE`, from `cargo xtask release version`, the same
//! computation that names the tag), so for a release the text is exactly
//! `tierstore <tag without v>+<first 12 digits of the commit>`. A release whose binary says
//! anything else (a crate version nobody bumped, another commit, no commit) is refused before
//! it is published. The binary is run, so this is made on a runner of its architecture:
//! linux-x86_64, where the release workflow runs. The binaries of the other platforms, the
//! macos-arm64 one included, cannot run there; each is run and checked the same way
//! (`build-commit --version`, [`check_built`]) on its own runner by the job of
//! `artifacts.yml` that builds it, before it is packaged.

use std::path::Path;

use super::BINARY;
use super::publish::is_release_tag;
use crate::Error;
use crate::build_commit::{Said, full_commit, is_commit};
use crate::fetch::{self, Sha256, Unpack};
use crate::package::{Platform, asset_stem};

/// Checks the text `tierstore --version` printed against the release `version`
/// (`<major>.<minor>.<patch>`) of `commit`.
pub fn check_said(said: &str, version: &str, commit: &str) -> Result<(), Error> {
    if !is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let expected = format!("{BINARY} {version}+{}", &commit[..12]);
    if said.trim_end() == expected {
        return Ok(());
    }
    Err(Error::Failed(format!(
        "the binary says {said:?}, not {expected:?}"
    )))
}

/// Runs `binary --version`: its stdout, or why it failed.
fn said(binary: &Path, run: fn(&Path) -> Result<Said, Error>) -> Result<String, Error> {
    let said = run(binary)?;
    if !said.success {
        return Err(Error::Failed(format!(
            "{} --version failed: {}",
            binary.display(),
            said.stderr.trim_end()
        )));
    }
    Ok(said.stdout)
}

/// The `release check-binary` verb: unpacks the linux-x86_64 tarball of `commit` in `dir`
/// into `scratch`, runs it with `--version`, and refuses it unless it says `tag` and `commit`.
pub fn check_binary(
    dir: &Path,
    tag: &str,
    commit: &str,
    scratch: &Path,
    run: fn(&Path) -> Result<Said, Error>,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !is_release_tag(tag) {
        return Err(Error::Usage(format!(
            "--tag {tag:?} is not v<major>.<minor>.<patch>"
        )));
    }
    let name = format!(
        "{}.tar.gz",
        asset_stem(BINARY, commit, Platform::LinuxX86_64)
    );
    let path = dir.join(&name);
    let bytes = std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
    std::fs::create_dir_all(scratch)
        .map_err(Error::spawn(format!("create {}", scratch.display())))?;
    let binary = scratch.join(BINARY);
    let unpack = Unpack::Tar {
        xz: false,
        member: BINARY.into(),
    };
    fetch::install(&bytes, &Sha256::of(&bytes), &unpack, &binary)?;
    let text = said(&binary, run)?;
    check_said(&text, &tag[1..], commit)?;
    Ok(vec![format!(
        "release check-binary: {name} says {}, the release {tag} of {commit}",
        text.trim_end()
    )])
}

/// `build-commit --version`: the binary at `binary` says `version` and `commit`.
pub fn check_built(
    binary: &Path,
    version: &str,
    commit: &str,
    run: fn(&Path) -> Result<Said, Error>,
) -> Result<String, Error> {
    if !is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let text = said(binary, run)?;
    check_said(&text, version, commit)?;
    Ok(format!(
        "{}: {} says version {version}",
        binary.display(),
        text.trim_end()
    ))
}

#[cfg(test)]
mod tests;
