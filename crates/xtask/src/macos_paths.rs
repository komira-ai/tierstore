//! Whether a pull request needs the macOS job of `macos.yml`, from the paths it changes.
//!
//! The macOS job builds and tests the workspace on macOS. A pull request whose every changed path
//! is one that job builds nothing from ([`skippable`]) does not need it: documentation, the lint
//! configuration, the issue and pull request templates, and the workflows that put nothing on
//! macOS. A test that reads such a file reads it as text, the same on every system, and `ci.yml`
//! runs every test on Linux for every pull request.
//!
//! What cannot be judged runs the job: a path not in the lists below, a path that is not UTF-8,
//! a list that names no path, and a list that cannot be read. `macos.yml` writes the list with
//! `git diff --no-renames --name-only -z` (a rename is its old and its new path, each judged).

use std::path::Path;

use crate::Error;

/// The directories whose every file a pull request may change without the macOS job.
pub const SKIPPABLE_DIRS: [&str; 2] = ["docs/", ".github/ISSUE_TEMPLATE/"];

/// The files a pull request may change without the macOS job, besides a top-level `*.md` file.
/// The workflows listed put nothing on macOS for a pull request; `macos.yml` and `artifacts.yml`
/// (whose macOS job `macos.yml` repeats) are not listed, nor is a workflow added later.
pub const SKIPPABLE_FILES: [&str; 10] = [
    "LICENSE",
    "clippy.toml",
    "deny.toml",
    "rustfmt.toml",
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/workflows/advisories.yml",
    ".github/workflows/ci.yml",
    ".github/workflows/hostkeeper-desired.yml",
    ".github/workflows/tierstore-site-publish.yml",
    ".github/workflows/tierstore-site.yml",
];

/// Whether `path` (relative to the repository root, `/`-separated, as git names it) is one the
/// macOS job builds nothing from: under [`SKIPPABLE_DIRS`], one of [`SKIPPABLE_FILES`], or a
/// `*.md` file at the top of the repository.
pub fn skippable(path: &str) -> bool {
    SKIPPABLE_DIRS
        .iter()
        .any(|dir| path.len() > dir.len() && path.starts_with(dir))
        || SKIPPABLE_FILES.contains(&path)
        || (!path.contains('/') && path.len() > ".md".len() && path.ends_with(".md"))
}

/// Whether the macOS job runs for a change of the paths in `listed` (NUL-separated, as
/// `git diff -z` writes them), and the line that says why.
pub fn decide(listed: &[u8]) -> (bool, String) {
    let paths: Vec<&[u8]> = listed
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    if paths.is_empty() {
        return (
            true,
            "the macOS job runs: the list of changed paths names none".to_owned(),
        );
    }
    for path in &paths {
        match std::str::from_utf8(path) {
            Err(_) => {
                return (
                    true,
                    format!(
                        "the macOS job runs: the changed path {:?} is not UTF-8",
                        String::from_utf8_lossy(path)
                    ),
                );
            }
            Ok(path) if !skippable(path) => {
                return (
                    true,
                    format!("the macOS job runs: {path} is not a path it may skip"),
                );
            }
            Ok(_) => {}
        }
    }
    (
        false,
        format!(
            "the macOS job is not needed: each of the {} changed paths is one it builds nothing from",
            paths.len()
        ),
    )
}

/// `cargo xtask macos-paths`: judges the list `changed` ([`decide`]; a list that cannot be read
/// runs the job), appends `macos=true` or `macos=false` to `output` if given, and returns the
/// lines to print.
pub fn macos_paths(changed: &Path, output: Option<&Path>) -> Result<Vec<String>, Error> {
    let (run, why) = match std::fs::read(changed) {
        Ok(listed) => decide(&listed),
        Err(e) => (
            true,
            format!(
                "the macOS job runs: the list {} cannot be read: {e}",
                changed.display()
            ),
        ),
    };
    let line = format!("macos={run}");
    if let Some(output) = output {
        crate::release::append_output(output, std::slice::from_ref(&line))?;
    }
    Ok(vec![why, line])
}

#[cfg(test)]
mod tests;
