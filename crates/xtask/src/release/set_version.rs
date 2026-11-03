//! `release set-version`: gives the `tierstore` crate the version of the release before it is
//! built, so the binary reports the version of its `Cargo.toml`.
//!
//! `build.rs` embeds the crate's version and the commit of the checkout; it reads no
//! environment variable for either. The release build jobs run this verb with the version
//! `release version` computed: it rewrites `version` in the `[package]` table of
//! `crates/tierstore/Cargo.toml` and the `tierstore` entry of `Cargo.lock` (the local package and
//! the dependency entries that name it, so the `--locked` build accepts it), and touches nothing else.

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

/// The lock text with the version of the local `tierstore` package (the one without a `source`
/// line) set to `version`, and every dependency entry naming that package by its old version
/// (`"tierstore <old>"`, no ` (source)` suffix) changed to match. A second package of the same
/// name from a remote source, and the entries naming it, stay as they were.
pub fn set_lock(text: &str, version: &str) -> Result<String, Error> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let local = (0..lines.len())
        .filter(|&i| lines[i] == "name = \"tierstore\"")
        .find(|&i| {
            let end = package_end(&lines, i);
            !lines[i..end].iter().any(|l| l.starts_with("source = "))
        })
        .ok_or_else(|| Error::Failed(format!("{LOCK}: no tierstore package")))?;
    let end = package_end(&lines, local);
    let at = (local + 1..end)
        .find(|&i| lines[i].starts_with("version = \""))
        .ok_or_else(|| Error::Failed(format!("{LOCK}: tierstore has no version line")))?;
    let old = lines[at]["version = \"".len()..]
        .trim_end_matches('"')
        .to_owned();
    lines[at] = format!("version = \"{version}\"");
    let from = format!("\"tierstore {old}\",");
    let to = format!("\"tierstore {version}\",");
    for line in lines.iter_mut().filter(|l| l.trim() == from) {
        *line = format!("{}{to}", &line[..line.len() - line.trim_start().len()]);
    }
    // Cargo writes same-named entries ordered by version, then local before remote: a lock
    // in another order is rewritten by the next cargo command, which `--locked` refuses.
    for run in lines.chunk_by_mut(|a, b| is_tierstore_ref(a) == is_tierstore_ref(b)) {
        if is_tierstore_ref(&run[0]) {
            run.sort_by_cached_key(|l| (version_key(l), l.contains('(')));
        }
    }
    let joined = lines.join("\n");
    let mut blocks: Vec<String> = joined.split("\n\n").map(str::to_owned).collect();
    let at: Vec<usize> = (0..blocks.len())
        .filter(|&i| blocks[i].starts_with("[[package]]\nname = \"tierstore\"\n"))
        .collect();
    let mut run: Vec<String> = at.iter().map(|&i| blocks[i].clone()).collect();
    run.sort_by_cached_key(|b| (block_key(b), b.contains("\nsource = ")));
    for (&i, block) in at.iter().zip(run) {
        blocks[i] = block;
    }
    Ok(blocks.join("\n\n"))
}

/// Whether `line` is a dependency entry naming a package called `tierstore`.
fn is_tierstore_ref(line: &str) -> bool {
    line.trim_start().starts_with("\"tierstore ")
}

/// The numbers of the version in `line` (a dependency entry or a `version` line), before any
/// `(source)`, for ordering versions the way cargo does.
fn version_key(line: &str) -> Vec<u64> {
    line.split('(')
        .take(1)
        .flat_map(|l| l.split(|c: char| !c.is_ascii_digit()))
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// The version key of the package entry `block`.
fn block_key(block: &str) -> Vec<u64> {
    block
        .lines()
        .filter(|l| l.starts_with("version = \""))
        .flat_map(version_key)
        .collect()
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
