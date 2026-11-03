//! `release set-version`: gives the `tierstore` crate the version of the release before it is
//! built, so the binary reports the version of its `Cargo.toml`.
//!
//! `build.rs` embeds the crate's version and the commit of the checkout; it reads no
//! environment variable for either. The release build jobs run this verb with the version
//! `release version` computed: it rewrites `version` in the `[package]` table of
//! `crates/tierstore/Cargo.toml` and the `tierstore` entry of `Cargo.lock` (so the `--locked`
//! build accepts it), and touches nothing else.

use std::path::Path;

use super::publish::is_release_tag;
use crate::Error;

/// The manifest of the crate, under the repository root.
const MANIFEST: &str = "crates/tierstore/Cargo.toml";

/// The lock file, under the repository root.
const LOCK: &str = "Cargo.lock";

/// `text` with the value of the first `version = "..."` line after line `from` replaced by
/// `version`; none when `stop` holds of a line first, or there is no such line.
fn replace_version(
    text: &str,
    from: usize,
    stop: impl Fn(&str) -> bool,
    version: &str,
) -> Option<String> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    for line in lines.iter_mut().skip(from + 1) {
        if stop(line) {
            return None;
        }
        if line.starts_with("version = \"") {
            *line = format!("version = \"{version}\"");
            return Some(lines.join("\n"));
        }
    }
    None
}

/// The manifest text with the `[package]` version set to `version`.
pub fn set_manifest(text: &str, version: &str) -> Result<String, Error> {
    let start = text
        .split('\n')
        .position(|l| l.trim_end() == "[package]")
        .ok_or_else(|| Error::Failed(format!("{MANIFEST}: no [package] table")))?;
    replace_version(text, start, |l| l.starts_with('['), version)
        .ok_or_else(|| Error::Failed(format!("{MANIFEST}: [package] has no version line")))
}

/// The lock text with the version of the `tierstore` package set to `version`.
pub fn set_lock(text: &str, version: &str) -> Result<String, Error> {
    let start = text
        .split('\n')
        .position(|l| l == "name = \"tierstore\"")
        .ok_or_else(|| Error::Failed(format!("{LOCK}: no tierstore package")))?;
    replace_version(text, start, str::is_empty, version)
        .ok_or_else(|| Error::Failed(format!("{LOCK}: tierstore has no version line")))
}

/// The `release set-version` verb: sets the version in both files under `root`; the line to
/// print says which.
pub fn set_version(root: &Path, version: &str) -> Result<Vec<String>, Error> {
    if !is_release_tag(&format!("v{version}")) {
        return Err(Error::Failed(format!(
            "{version:?} is not <major>.<minor>.<patch>, decimal numbers without a leading zero"
        )));
    }
    type Edit = fn(&str, &str) -> Result<String, Error>;
    let edits: [(&str, Edit); 2] = [(MANIFEST, set_manifest), (LOCK, set_lock)];
    // Both are computed before either is written, so a refusal leaves both as they were.
    let mut written = Vec::new();
    for (name, edit) in edits {
        let path = root.join(name);
        let text = std::fs::read_to_string(&path)
            .map_err(Error::spawn(format!("read {}", path.display())))?;
        written.push((path, edit(&text, version)?));
    }
    for (path, text) in written {
        std::fs::write(&path, text).map_err(Error::spawn(format!("write {}", path.display())))?;
    }
    Ok(vec![format!(
        "{MANIFEST} and {LOCK}: tierstore is {version}"
    )])
}

#[cfg(test)]
mod tests;
