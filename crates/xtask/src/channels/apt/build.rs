//! `channels apt-index --current <dir> --out <dir> --debs <dir> --version <V> --origin <t> --label
//! <t> --description <t> --date <epoch> [--keep <n>]`: the packages merged into the index
//! `apt-read` copied to `<current>`.
//!
//! The index is shared with other products. Their entries are kept as they are, the architectures
//! they list are kept (an index written without one would drop its packages), and the `Origin`,
//! `Label` and `Description` of the `Release` that exists stay: the three flags apply to the
//! first `Release` only. A version older than the newest listed for tierstore is refused, and so
//! is the same version with other bytes. Everything is computed before anything is written; the
//! new unsigned index goes to `<out>/dists/stable/` and the ETags it was built from to
//! `<out>/etags.txt`. The `Date` is `--date`, seconds since the epoch.

use std::path::Path;

use super::control;
use super::index::{self, ReleaseInfo, Stanza};
use super::package_files;
use super::state::{self, ETAGS_FILE, POOL_DIR};
use crate::Error;

/// How many versions of a package and architecture the index keeps, by default.
pub const KEEP: usize = 25;

/// What `apt-index` is asked.
pub struct Request<'a> {
    pub current: &'a Path,
    pub out: &'a Path,
    pub debs: &'a Path,
    pub version: &'a str,
    pub origin: &'a str,
    pub label: &'a str,
    pub description: &'a str,
    pub date: u64,
    pub keep: usize,
}

/// The directories `binary-<arch>` below `main` in `dists`, as architectures.
fn listed_archs(main: &Path) -> Vec<String> {
    std::fs::read_dir(main)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("binary-").map(str::to_owned)
        })
        .collect()
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let what = || format!("write {}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(Error::spawn(what()))?;
    std::fs::write(path, bytes).map_err(Error::spawn(what()))
}

/// The `apt-index` verb: the lines to print.
///
/// # Errors
/// No `etags.txt` in `current`, a package that is missing or that says other than its name says,
/// an index that cannot be read or merged, or a file that cannot be written.
pub fn build(r: &Request) -> Result<Vec<String>, Error> {
    let etags_in = r.current.join(ETAGS_FILE);
    let etags = std::fs::read_to_string(&etags_in).map_err(|_| {
        Error::Failed(format!(
            "{} is missing: run `apt read` first",
            etags_in.display()
        ))
    })?;
    let mut added: Vec<Stanza> = Vec::new();
    for (name, path) in package_files(r.debs, r.version)? {
        let bytes =
            std::fs::read(&path).map_err(Error::spawn(format!("read {}", path.display())))?;
        let stanza = index::stanza_of(&bytes).map_err(|e| Error::Failed(format!("{name}: {e}")))?;
        // The package's own control file decides the entry's file name; it must be this file.
        let fields = control::parse(&stanza.text)?;
        let filename = control::get(&fields, "Filename").unwrap_or_default();
        if filename != format!("{POOL_DIR}/{name}") {
            return Err(Error::Failed(format!(
                "{name} is the package {filename}: its control file does not say the name it has"
            )));
        }
        added.push(stanza);
    }
    let main = r.current.join("dists").join("stable").join("main");
    let mut archs: Vec<String> = state::DEFAULT_ARCHS
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    archs.extend(listed_archs(&main));
    archs.sort();
    archs.dedup();
    let mut existing = Vec::new();
    for arch in &archs {
        let path = main.join(format!("binary-{arch}")).join("Packages");
        if let Ok(text) = std::fs::read_to_string(&path) {
            existing.extend(
                index::parse_packages(&text)
                    .map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?,
            );
        }
    }
    let merged = index::merge(&existing, &added, r.keep)?;
    let release_path = r.current.join("dists").join("stable").join("Release");
    let release = std::fs::read_to_string(&release_path).ok();
    let info = index::keep_shared_fields(
        release.as_deref(),
        ReleaseInfo {
            origin: Some(r.origin.to_owned()),
            label: Some(r.label.to_owned()),
            description: Some(r.description.to_owned()),
            date: r.date,
        },
    )?;
    let tree = index::tree(&info, &merged.stanzas, &archs)?;
    // Everything is computed; only now is anything written.
    for file in &tree {
        write(&r.out.join(&file.path), &file.data)?;
    }
    // The new index was built from exactly the objects whose ETags these are.
    write(&r.out.join(ETAGS_FILE), etags.as_bytes())?;
    Ok(vec![format!(
        "apt-index: added {}, dropped {}, listed {}",
        merged.added,
        merged.dropped,
        merged.stanzas.len()
    )])
}

#[cfg(test)]
mod tests;
