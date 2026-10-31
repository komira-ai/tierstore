//! `build-commit check <binary> <commit>`: a built binary names the commit it was built from.
//!
//! `tierstore --version` prints `tierstore <version>+<commit>`, where `<commit>` is the 12
//! lowercase hex digits `build.rs` embeds (`unknown` where the build had no git checkout). The
//! check runs the binary, so it is made on a runner of the binary's own architecture, and
//! refuses a binary that says another commit, `unknown`, or nothing it can read. A control
//! follows: the same output must not pass for a commit whose twelfth digit differs, so a check that
//! accepts any text fails instead of passing.

use std::path::Path;
use std::process::Command;

use crate::Error;

/// A commit as git writes it: 12 to 40 lowercase hex digits.
pub fn is_commit(text: &str) -> bool {
    (12..=40).contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A full commit SHA (40 lowercase hex digits), or a usage error naming the flag.
pub fn full_commit(flag: &str, commit: &str) -> Result<(), Error> {
    if commit.len() == 40 && is_commit(commit) {
        return Ok(());
    }
    Err(Error::Usage(format!(
        "{flag} {commit:?} is not a full commit SHA (40 lowercase hex digits)"
    )))
}

/// `commit` with its last hex digit changed: a commit the binary was not built from.
pub fn other_commit(commit: &str) -> String {
    let (head, last) = commit.split_at(commit.len().saturating_sub(1));
    format!("{head}{}", if last == "0" { "1" } else { "0" })
}

/// `commit` with the last of its first 12 hex digits changed: the digits `--version` carries,
/// so a commit the binary names differently. (`other_commit` changes the 40th digit, which
/// `--version` does not carry, so it cannot serve as a control for this check.)
pub fn other_short_commit(commit: &str) -> String {
    let mut digits = commit.to_owned();
    let last = if &commit[11..12] == "0" { "1" } else { "0" };
    digits.replace_range(11..12, last);
    digits
}

/// Whether the `--version` text `said` ends with `+<first 12 digits of commit>`.
pub fn names_commit(said: &str, commit: &str) -> bool {
    let short = &commit[..12];
    said.trim_end()
        .rsplit_once('+')
        .is_some_and(|(_, tail)| tail == short)
}

/// What `<binary> --version` did: whether it succeeded, its stdout and its stderr.
pub struct Said {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `<binary> --version`.
pub fn run_version(binary: &Path) -> Result<Said, Error> {
    let out = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(Error::spawn(binary.display().to_string()))?;
    Ok(Said {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The `check` verb, with the way to run the binary (tests pass a stand-in): the line to print.
pub fn check_with(
    binary: &Path,
    commit: &str,
    run: fn(&Path) -> Result<Said, Error>,
) -> Result<String, Error> {
    check_naming(binary, commit, run, names_commit)
}

/// [`check_with`] with the way the version text is read (tests pass a reader that accepts any
/// text, which the control must catch).
pub fn check_naming(
    binary: &Path,
    commit: &str,
    run: fn(&Path) -> Result<Said, Error>,
    names: fn(&str, &str) -> bool,
) -> Result<String, Error> {
    if !is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let said = run(binary)?;
    if !said.success {
        return Err(Error::Failed(format!(
            "{} --version failed: {}",
            binary.display(),
            said.stderr.trim_end()
        )));
    }
    let text = said.stdout.trim_end().to_owned();
    if !names(&text, commit) {
        return Err(Error::WrongCommit {
            version: text,
            commit: commit.to_owned(),
        });
    }
    let other = other_short_commit(commit);
    if names(&text, &other) {
        return Err(Error::ControlPassed {
            version: text,
            other,
        });
    }
    Ok(format!(
        "{}: {text} names commit {}",
        binary.display(),
        &commit[..12]
    ))
}

/// The `check` verb: the line to print.
pub fn check(binary: &Path, commit: &str) -> Result<String, Error> {
    check_with(binary, commit, run_version)
}

#[cfg(test)]
mod tests;
