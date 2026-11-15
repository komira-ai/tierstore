//! The layout `extents-v1` of the cache directory, and the scan that reads it back at start.
//!
//! ```text
//! <path>/extents-v1/<aa>/<bb>/<hex>        a sparse file of the object's full size, mode 0644
//! <path>/extents-v1/<aa>/<bb>/<hex>.ext    JSON: the key, the size, the tag, the filled extents
//! ```
//!
//! `<hex>` is the name `digest-v1` gives the key (`layout::name_of`). The sidecar lists the
//! `[offset, length]` ranges whose bytes are in the file and were checked against the SHA-256
//! the caller gave; the bytes outside them are holes or unchecked and are never served. The
//! data file is written before the sidecar says its ranges are filled, so a crash of the
//! process leaves a sidecar that is behind the file, never ahead of it. An entry whose file has
//! not the size its sidecar says (a file cut short) is dropped at start.
//!
//! `digest-v1` (`obj/`) holds whole objects only, as every release has read it; a release that
//! does not know this directory does not read it.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::layout::{is_digest, make_dirs};
use super::map::ExtentMap;

/// The directory of the entries by extents.
const EXTENTS: &str = "extents-v1";

/// The ending of a sidecar's name.
const SIDECAR: &str = ".ext";

/// What the sidecar of an entry by extents holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::server::local_cache) struct ExtentSidecar {
    /// The key of the object the ranges are of.
    pub key: String,
    /// The size of the object, which is the size of the data file.
    pub size: u64,
    /// The content tag the upstream gave the object.
    pub tag: String,
    /// The time the upstream gave the object, in milliseconds.
    pub modified: u64,
    /// The filled ranges, as `[offset, length]`, in order, apart from one another.
    pub extents: Vec<[u64; 2]>,
}

/// The directory that holds the files of `hex` under `root`.
pub(in crate::server::local_cache) fn dir_of(root: &Path, hex: &str) -> PathBuf {
    let (first, rest) = hex.split_at(2);
    let (second, _) = rest.split_at(2);
    root.join(EXTENTS).join(first).join(second)
}

/// The path of the data file of `hex` under `root`.
pub(in crate::server::local_cache) fn data_of(root: &Path, hex: &str) -> PathBuf {
    dir_of(root, hex).join(hex)
}

/// The path of the sidecar of `hex` under `root`.
pub(in crate::server::local_cache) fn meta_of(root: &Path, hex: &str) -> PathBuf {
    dir_of(root, hex).join(format!("{hex}{SIDECAR}"))
}

/// The name a sidecar is written under before it is renamed into place.
pub(in crate::server::local_cache) fn meta_new_of(root: &Path, hex: &str) -> PathBuf {
    dir_of(root, hex).join(format!("{hex}{SIDECAR}.new"))
}

/// Makes the directory of `hex` (with the mode 0755 the files of `digest-v1` have).
///
/// # Errors
/// What the system said.
pub(in crate::server::local_cache) fn make(root: &Path, hex: &str) -> io::Result<()> {
    make_dirs(root, &dir_of(root, hex))
}

/// Removes the files of `hex`; what is not there is passed over.
pub(in crate::server::local_cache) fn remove(root: &Path, hex: &str) {
    let _ = std::fs::remove_file(data_of(root, hex));
    let _ = std::fs::remove_file(meta_of(root, hex));
    let _ = std::fs::remove_file(meta_new_of(root, hex));
}

/// An entry the scan found whole: a data file of the size its sidecar says, with ranges.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::server::local_cache) struct Found {
    pub hex: String,
    pub sidecar: ExtentSidecar,
    pub map: ExtentMap,
    /// The time of last change of the data file, in milliseconds: the seed of its recency.
    pub used: u64,
}

/// What the scan found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::server::local_cache) struct Scanned {
    pub found: Vec<Found>,
    /// The files removed because they were not part of a usable entry.
    pub discarded: u64,
}

/// The entries under `root`, and the removal of every file that is not part of a usable one: a
/// data file with no readable sidecar, of another size than the sidecar says, with no range or
/// ranges that are out of order or past the end, a sidecar with no data file, an entry the
/// `whole` set of names also holds whole (the entry in `obj/` is the one to keep). A directory
/// that is not there has no entries.
pub(in crate::server::local_cache) fn scan(root: &Path, whole: &HashSet<&str>) -> Scanned {
    let mut scanned = Scanned::default();
    for first in children(&root.join(EXTENTS)) {
        for second in children(&first) {
            for path in children(&second) {
                let Some(hex) = data_name(&path) else {
                    continue;
                };
                if data_of(root, &hex) != path {
                    // A file of a digest's name in a place that is not its own.
                    scanned.discarded += u64::from(std::fs::remove_file(&path).is_ok());
                    continue;
                }
                let kept = read(root, &hex, whole.contains(hex.as_str()));
                match kept {
                    Some(found) => scanned.found.push(found),
                    None => scanned.discarded += removed(root, &hex),
                }
            }
            // A sidecar whose data file is not there, and what a write of a sidecar left.
            for path in children(&second) {
                let orphan = match path.extension().and_then(|ext| ext.to_str()) {
                    Some("ext") => !path.with_extension("").is_file(),
                    Some("new") => true,
                    _ => false,
                };
                if orphan {
                    scanned.discarded += u64::from(std::fs::remove_file(&path).is_ok());
                }
            }
        }
    }
    scanned
}

/// The paths in `dir`; none if it cannot be read.
fn children(dir: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    entries.map(|entry| entry.path()).collect()
}

/// The name of the entry whose data file `path` is: a digest. A file that is not one is no data
/// file (the sidecar, or what is not ours).
fn data_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    is_digest(name).then(|| name.to_owned())
}

/// Removes the files of `hex`; gives 1 if the data file was there.
fn removed(root: &Path, hex: &str) -> u64 {
    let gone = std::fs::remove_file(data_of(root, hex)).is_ok();
    let _ = std::fs::remove_file(meta_of(root, hex));
    let _ = std::fs::remove_file(meta_new_of(root, hex));
    u64::from(gone)
}

/// The entry `hex` if it is usable and not `unwanted`.
fn read(root: &Path, hex: &str, unwanted: bool) -> Option<Found> {
    if unwanted {
        return None;
    }
    let data = std::fs::metadata(data_of(root, hex)).ok()?;
    let text = std::fs::read(meta_of(root, hex)).ok()?;
    let sidecar: ExtentSidecar = serde_json::from_slice(&text).ok()?;
    let map = ExtentMap::from_pairs(&sidecar.extents, sidecar.size)?;
    let used = data
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    let usable = data.len() == sidecar.size && !map.is_empty();
    usable.then_some(Found {
        hex: hex.to_owned(),
        sidecar,
        map,
        used,
    })
}
