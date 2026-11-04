//! `licenses --metadata <file> --package <name> --out <file>`: the third-party licence notice of a
//! released binary.
//!
//! `--metadata` is the output of `cargo metadata --format-version 1` for the build of the
//! binary (the build jobs of `artifacts.yml` pass `--filter-platform` with the build's target and
//! the crate's `server` feature, so the resolution is that of the binary). The notice lists
//! every crate that is not of this workspace and that `--package` reaches through normal
//! dependencies (not dev or build dependencies, which are not linked into the binary, and not
//! procedural macros, which run in the compiler, nor what only they need), once per
//! name and version, in name and version order: its licence expression as its manifest gives it,
//! its repository, and the text of each of its licence files (a file of the crate's directory
//! whose name starts with `LICENSE`, `LICENCE`, `COPYING`, `NOTICE` or `UNLICENSE`, in any case,
//! and the file the manifest names as its `license_file`, when it is there). Files in
//! subdirectories are not searched.
//! A crate that ships no licence file is listed with its declared licence and a line saying so.
//! A crate that declares no licence (no `license` and no `license_file`) is refused, with every
//! other such crate named, and nothing is written. `package --licenses` puts the file into the
//! tarball.
//!
//! The output file may not exist already.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::Value;

use crate::{Error, package};

/// The upper-case beginnings of the names of the files that hold a licence text or a notice.
const LICENCE_FILES: [&str; 5] = ["LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE"];

/// What `get` takes from the member `key` of `v`; `whose` names `v` in the error.
fn field<'a, T>(
    v: &'a Value,
    key: &str,
    whose: &str,
    get: fn(&'a Value) -> Option<T>,
) -> Result<T, Error> {
    v.get(key).and_then(get).ok_or_else(|| {
        Error::Failed(format!(
            "the metadata: {whose} has no `{key}` of the expected type"
        ))
    })
}

/// Whether the package `p` is a procedural macro: it runs in the compiler and is not linked into
/// the binary, nor are its own dependencies (unless something else needs them).
fn is_proc_macro(p: &Value) -> bool {
    p["targets"].as_array().is_some_and(|targets| {
        targets.iter().any(|t| {
            t["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("proc-macro")))
        })
    })
}

/// The packages `root` (a workspace package's name) reaches through normal dependencies, other
/// than workspace packages, by name and version.
fn third_party<'a>(meta: &'a Value, root: &str) -> Result<Vec<&'a Value>, Error> {
    let packages = field(meta, "packages", "the metadata", Value::as_array)?;
    let resolve = field(meta, "resolve", "the metadata", Some)?;
    let nodes = field(resolve, "nodes", "resolve", Value::as_array)?;
    let mut by_id = HashMap::new();
    for p in packages {
        by_id.insert(field(p, "id", "a package", Value::as_str)?, p);
    }
    let mut node_of = HashMap::new();
    for n in nodes {
        node_of.insert(field(n, "id", "a resolve node", Value::as_str)?, n);
    }
    let start = packages
        .iter()
        .find(|p| p["name"] == root && p["source"].is_null())
        .ok_or_else(|| {
            Error::Failed(format!(
                "the metadata has no workspace package named {root}"
            ))
        })?;
    let start_id = field(start, "id", "the package", Value::as_str)?;
    let mut seen = BTreeSet::new();
    let mut todo = vec![start_id];
    while let Some(id) = todo.pop() {
        if by_id.get(id).is_some_and(|p| is_proc_macro(p)) || !seen.insert(id) {
            continue;
        }
        let node = node_of
            .get(id)
            .ok_or_else(|| Error::Failed(format!("the metadata: no resolve node for {id}")))?;
        for dep in field(node, "deps", id, Value::as_array)? {
            let kinds = field(dep, "dep_kinds", "a dependency", Value::as_array)?;
            if kinds.iter().any(|k| k["kind"].is_null()) {
                todo.push(field(dep, "pkg", "a dependency", Value::as_str)?);
            }
        }
    }
    let mut found = BTreeMap::new();
    for id in seen.into_iter().filter(|id| *id != start_id) {
        let p = by_id
            .get(id)
            .ok_or_else(|| Error::Failed(format!("the metadata: no package {id}")))?;
        if p["source"].is_null() {
            continue;
        }
        let name = field(p, "name", id, Value::as_str)?;
        let version = field(p, "version", id, Value::as_str)?;
        found.insert((name, version), *p);
    }
    Ok(found.into_values().collect())
}

/// Whether a file of this name holds a licence text or a notice.
fn is_licence_file(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    LICENCE_FILES.iter().any(|p| upper.starts_with(p))
}

/// The notice of the crate `p`: its section of the file. A crate that declares no licence is added
/// to `problems`.
fn section(p: &Value, problems: &mut Vec<String>) -> Result<String, Error> {
    let name = field(p, "name", "a package", Value::as_str)?;
    let version = field(p, "version", "a package", Value::as_str)?;
    let label = format!("{name} {version}");
    let manifest = field(p, "manifest_path", &label, Value::as_str)?;
    let dir = Path::new(manifest).with_file_name("");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map_err(Error::spawn(format!("read {}", dir.display())))?
        .flatten()
        .filter(|e| e.path().is_file() && is_licence_file(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    if let Some(named) = p["license_file"].as_str() {
        let path = dir.join(named);
        if path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    files.sort();
    let declared = p["license"].as_str().or_else(|| p["license_file"].as_str());
    if declared.is_none() {
        problems.push(format!("{label}: declares no licence"));
    }
    let mut text = format!(
        "{label}\nlicence: {}\n",
        declared.unwrap_or("none declared")
    );
    if let Some(repository) = p["repository"].as_str() {
        text.push_str(&format!("source: {repository}\n"));
    }
    if files.is_empty() {
        text.push_str(
            "\nThis crate ships no licence file; its licence is the one declared above.\n",
        );
    }
    for file in files {
        let bytes = package::read(&file)?;
        text.push_str(&format!(
            "\n--- {} ---\n{}\n",
            file.strip_prefix(&dir).unwrap_or(&file).display(),
            String::from_utf8_lossy(&bytes).trim_end()
        ));
    }
    Ok(text)
}

/// The notice of everything `root` is built from, as the text of the file.
///
/// # Errors
/// The metadata is not what `cargo metadata` writes, or a crate declares no licence.
pub fn notice(meta: &Value, root: &str) -> Result<String, Error> {
    let crates = third_party(meta, root)?;
    let mut problems = Vec::new();
    let mut text = format!(
        "Third-party licences of {root}\n\n\
         The crates below are linked into the binary. Each is used under the licence it declares;\n\
         the licence texts it ships follow it.\n"
    );
    for p in &crates {
        text.push_str("\n================================================================\n\n");
        text.push_str(&section(p, &mut problems)?);
    }
    if problems.is_empty() {
        Ok(text)
    } else {
        Err(Error::Refused(problems))
    }
}

/// The `licenses` verb: the lines to print.
pub fn licenses(metadata: &Path, root: &str, out: &Path) -> Result<Vec<String>, Error> {
    let bytes = package::read(metadata)?;
    let meta: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Failed(format!("{} is not JSON: {e}", metadata.display())))?;
    let text = notice(&meta, root)?;
    package::write_new(out, text.as_bytes())?;
    Ok(vec![format!("wrote {}", out.display())])
}

#[cfg(test)]
mod tests;
