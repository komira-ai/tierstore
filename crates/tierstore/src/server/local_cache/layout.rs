//! The layout `digest-v1` of the cache directory, and the scan that reads it back at start.
//!
//! ```text
//! <path>/obj/<aa>/<bb>/<hex>        the raw bytes of an object, mode 0444
//! <path>/obj/<aa>/<bb>/<hex>.meta   JSON: the key, the size, the SHA-256 of the bytes, the tag
//! <path>/.stage/                    fills in progress; emptied at start, never served
//! ```
//!
//! The sidecar's `origin` says whether the bytes are the upstream object of the key (the default,
//! left out of the file) or a `span` cut out of a pack; the fill socket's blobs are entries named
//! by their digest like any `cas/l/<sha256>` object.
//!
//! `<hex>` is the SHA-256 of the key's name, except for a key `cas/l/<sha256>`, which is the
//! content's own SHA-256 (the name says what the bytes must hash to); `<aa>` and `<bb>` are its
//! first two pairs of digits. The data file is created last, by a create-only link, so a name
//! that has a data file has its `.meta`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The directory of the objects.
const OBJECTS: &str = "obj";

/// The directory of the fills in progress.
pub(super) const STAGE: &str = ".stage";

/// The ending of a sidecar's name.
const SIDECAR: &str = ".meta";

/// The prefix of the keys that are named by the SHA-256 of their bytes.
const WHOLE_BLOBS: &str = "cas/l/";

/// The prefix of the keys that the previous release kept the fill socket's blobs under.
const OLD_FILL_PREFIX: &str = "_tiering/fill/";

/// What a key is called on disk, and what its bytes must hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Name {
    /// Sixty-four lower-case hex digits.
    pub hex: String,
    /// For a key named by its content, the SHA-256 the bytes must have; else none.
    pub content: Option<String>,
}

/// Whether `text` is the lower-case hex of a SHA-256.
pub(in crate::server) fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The on-disk name of `key`.
pub(super) fn name_of(key: &str) -> Name {
    let content = key.strip_prefix(WHOLE_BLOBS).filter(|rest| is_digest(rest));
    match content {
        Some(digest) => Name {
            hex: digest.to_owned(),
            content: Some(digest.to_owned()),
        },
        None => Name {
            hex: hex::encode(Sha256::digest(key.as_bytes())),
            content: None,
        },
    }
}

/// The directory that holds the files of `hex` under `root`.
pub(super) fn object_dir(root: &Path, hex: &str) -> PathBuf {
    let (first, rest) = hex.split_at(2);
    let (second, _) = rest.split_at(2);
    root.join(OBJECTS).join(first).join(second)
}

/// The path of the data file of `hex` under `root`.
pub(super) fn data_path(root: &Path, hex: &str) -> PathBuf {
    object_dir(root, hex).join(hex)
}

/// The path of the sidecar of `hex` under `root`.
pub(super) fn meta_path(root: &Path, hex: &str) -> PathBuf {
    let mut path = data_path(root, hex).into_os_string();
    path.push(SIDECAR);
    PathBuf::from(path)
}

/// The directory of the fills in progress.
pub(super) fn stage_dir(root: &Path) -> PathBuf {
    root.join(STAGE)
}

/// The directory of the objects under `root`.
pub(super) fn objects_dir(root: &Path) -> PathBuf {
    root.join(OBJECTS)
}

/// Makes `dir`, which is under `root`, and the directories between them, with mode 0755 for
/// each however the umask is set: the files in them are read by other users (the jobs that
/// hard-link them), and no other user may change a name in them.
///
/// # Errors
/// What the system said.
pub(super) fn make_dirs(root: &Path, dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let depth = dir
        .strip_prefix(root)
        .map_or(0, |below| below.components().count());
    for made in dir.ancestors().take(depth) {
        let mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(made, mode)?;
    }
    Ok(())
}

/// What the sidecar of an object holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Sidecar {
    /// The key the object was read under.
    pub key: String,
    /// The bytes in the data file.
    pub size: u64,
    /// The SHA-256 of the bytes, as the fill hashed them.
    pub sha256: String,
    /// The content tag the upstream gave the object.
    pub tag: String,
    /// The time the upstream gave the object, in milliseconds.
    pub modified: u64,
    /// Where the bytes came from. A sidecar of the previous release has no such field, and
    /// reads as [`Origin::Object`]; a release that does not know the field ignores it.
    #[serde(default, skip_serializing_if = "Origin::is_object")]
    pub origin: Origin,
}

/// Where the bytes of an entry named by a digest came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Origin {
    /// The bytes are the upstream object `cas/l/<sha256>`, whole.
    #[default]
    Object,
    /// The bytes were cut out of a larger object (a pack): no upstream object has the key
    /// `cas/l/<sha256>`, so the S3 front does not serve the entry as one.
    Span,
}

impl Origin {
    /// Whether this is [`Origin::Object`], which a sidecar leaves out.
    fn is_object(&self) -> bool {
        matches!(self, Origin::Object)
    }
}

/// An object that the scan found whole.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Found {
    pub hex: String,
    pub sidecar: Sidecar,
    /// The time of last change of the data file, in milliseconds: the seed of its recency.
    pub used: u64,
}

/// What the scan found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Scanned {
    /// The whole objects.
    pub found: Vec<Found>,
    /// The files that were not part of a whole object and were deleted.
    pub discarded: u64,
    /// The entries of the previous release's fill keys that were given the name of their digest:
    /// the old name, and the new one (a pins file of that release names the old).
    pub renamed: HashMap<String, String>,
}

/// Empties the stage directory (making it if it is not there): a fill that was running when the
/// process ended is not kept. Gives the number of entries removed; one that cannot be removed is
/// left, which costs the disk some bytes and nothing else.
///
/// # Errors
/// What the system said about making the directory.
pub(super) fn clear_stage(root: &Path) -> io::Result<u64> {
    let stage = stage_dir(root);
    make_dirs(root, &stage)?;
    let mut removed = 0;
    for path in children(&stage, |_| true) {
        let gone = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed += u64::from(gone.is_ok());
    }
    Ok(removed)
}

/// The paths in `dir` that `wanted` says yes to; none if `dir` cannot be read, and an entry that
/// cannot be read is left out.
fn children(dir: &Path, wanted: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    entries
        .map(|entry| entry.path())
        .filter(|path| wanted(path))
        .collect()
}

/// The whole object whose data file is `path` under `root`, if it is one: the name is a digest
/// in the directories of its first digits, the sidecar reads, and it says the size the file has.
fn whole(root: &Path, path: &Path) -> Option<Found> {
    let hex = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !is_digest(hex) || data_path(root, hex) != path {
        return None;
    }
    let data = std::fs::metadata(path).ok();
    let text = std::fs::read(meta_path(root, hex)).ok()?;
    let sidecar: Sidecar = serde_json::from_slice(&text).ok()?;
    let used = data
        .as_ref()
        .and_then(|data| data.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    (Some(sidecar.size) == data.map(|data| data.len())).then(|| Found {
        hex: hex.to_owned(),
        sidecar,
        used,
    })
}

/// Whether `path` is the name of a sidecar.
fn is_sidecar(path: &Path) -> bool {
    path.to_str().is_some_and(|text| text.ends_with(SIDECAR))
}

/// Reads the object directory under `root`: the objects that are whole, and the number of files
/// that are not part of one (a data file whose sidecar is missing, unreadable or says another
/// size, a sidecar with no data file, a file with a name that is not a digest in its place),
/// which are deleted (one that cannot be is not counted and is left).
///
/// # Errors
/// What the system said about making the directory.
pub(super) fn scan(root: &Path) -> io::Result<Scanned> {
    make_dirs(root, &objects_dir(root))?;
    let mut scanned = walk(root, true);
    rename_fill_entries(root, &mut scanned);
    Ok(scanned)
}

/// The digest of an entry that the previous release kept under the key `_tiering/fill/<sha256>`
/// (the name of such an entry is the hash of that key).
fn old_fill_digest(key: &str) -> Option<String> {
    let digest = key.strip_prefix(OLD_FILL_PREFIX)?;
    is_digest(digest).then(|| digest.to_owned())
}

/// Gives each entry of the previous release's fill keys the name of its digest, which is the
/// name every blob has now. The entry keeps its recency (the data file is renamed, so its time
/// of change stays). If the digest name is taken the old entry is removed: it holds the same
/// bytes. An entry that cannot be renamed is removed too, which costs a refill. Each step leaves
/// a state the scan reads back whole or deletes, so a crash in the middle loses at most the entry.
fn rename_fill_entries(root: &Path, scanned: &mut Scanned) {
    let found = std::mem::take(&mut scanned.found);
    let mut taken: HashSet<String> = found.iter().map(|entry| entry.hex.clone()).collect();
    for entry in found {
        let Some(digest) = old_fill_digest(&entry.sidecar.key) else {
            scanned.found.push(entry);
            continue;
        };
        let moved = if taken.contains(&digest) {
            // The digest entry holds the same bytes: the pins of the old name go to it.
            scanned.renamed.insert(entry.hex.clone(), digest.clone());
            None
        } else {
            rename_entry(root, &entry, &digest)
        };
        match moved {
            Some(moved) => {
                taken.insert(digest.clone());
                scanned.renamed.insert(entry.hex, digest);
                scanned.found.push(moved);
            }
            None => {
                let gone = std::fs::remove_file(data_path(root, &entry.hex));
                let _ = std::fs::remove_file(meta_path(root, &entry.hex));
                scanned.discarded += u64::from(gone.is_ok());
            }
        }
    }
}

/// Moves `entry` to the name `digest`, as an entry of origin `span` under the key
/// `cas/l/<digest>`: the new sidecar first, then the data file, then the old sidecar.
fn rename_entry(root: &Path, entry: &Found, digest: &str) -> Option<Found> {
    let sidecar = Sidecar {
        key: format!("{WHOLE_BLOBS}{digest}"),
        origin: Origin::Span,
        ..entry.sidecar.clone()
    };
    let text = serde_json::to_vec(&sidecar).ok()?;
    make_dirs(root, &object_dir(root, digest)).ok()?;
    let mut temporary = meta_path(root, digest).into_os_string();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    let meta = meta_path(root, digest);
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644)))
        .and_then(|()| std::fs::rename(&temporary, &meta))
        .ok()?;
    if std::fs::rename(data_path(root, &entry.hex), data_path(root, digest)).is_err() {
        let _ = std::fs::remove_file(&meta);
        return None;
    }
    let _ = std::fs::remove_file(meta_path(root, &entry.hex));
    Some(Found {
        hex: digest.to_owned(),
        sidecar,
        used: entry.used,
    })
}

/// Reads the object directory as [`scan`] does and changes nothing in it: the directory is not
/// made, and a file that is not part of a whole object is left where it is and not counted. It
/// is what a cache that does not hold the lock of its directory may do: the file may be the
/// middle of a publish by the one that does. A directory that is not there has no objects.
pub(super) fn read_only_scan(root: &Path) -> Scanned {
    walk(root, false)
}

/// Removes `path` if `delete`, and says whether it did.
fn discard(path: &Path, delete: bool) -> u64 {
    if delete {
        u64::from(std::fs::remove_file(path).is_ok())
    } else {
        0
    }
}

/// The walk of [`scan`] and [`read_only_scan`]: `delete` says whether what is not whole goes.
fn walk(root: &Path, delete: bool) -> Scanned {
    let mut scanned = Scanned::default();
    let objects = objects_dir(root);
    for first in children(&objects, Path::is_dir) {
        for second in children(&first, Path::is_dir) {
            let (sidecars, data): (Vec<PathBuf>, Vec<PathBuf>) = children(&second, Path::is_file)
                .into_iter()
                .partition(|p| is_sidecar(p));
            for path in data {
                match whole(root, &path) {
                    Some(found) => scanned.found.push(found),
                    None => scanned.discarded += discard(&path, delete),
                }
            }
            // After the data files: a sidecar is kept only by a data file that is still there.
            for path in sidecars {
                if !path.with_extension("").is_file() {
                    scanned.discarded += discard(&path, delete);
                }
            }
        }
    }
    scanned
}
