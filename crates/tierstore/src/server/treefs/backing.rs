//! The backing ids an earlier release left on the connection (`docs/design/virtual-build-fs.md`
//! section 7.1). This release never asks the kernel for passthrough and registers no backing id:
//! every open is a cached open. A release before it did, on a connection whose `INIT` it answered
//! with `FUSE_PASSTHROUGH`, and kept for each blob with a live id a line `<sha256> <id> <opens>`
//! in `<runtime dir>/backing` (a fourth word `exact` on a count it knew exactly).
//!
//! The descriptor of the connection outlives the process (the service manager keeps it), so a
//! process of this release that takes such a connection over finds those ids still registered,
//! and inodes the kernel still serves from them. The kernel fails with `EIO` a cached open of an
//! inode that has a passthrough open, so an open of one of those blobs is answered with its id, as
//! the release before would have: [`Adopted`] holds them, read once at the take-over and never
//! changed, and never closes one (the kernel closes them all when the connection ends). An open of
//! any other blob is a cached open.
//!
//! The file is read ([`saved`]) at the take-over, and written once more if a line of it is marked
//! `exact`, with no line marked: an earlier release that takes the connection over from this one keeps every id for as
//! long as the connection lives, since the opens of them that this process made are counted
//! nowhere. A fresh mount removes the file (`start.rs`). The fixtures are `fs_backing_table.txt`
//! and `fs_backing_table_newer.txt`.

use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::tree_manifest::TreeDigest;

/// The name of the file in the runtime directory.
pub(super) const FILE: &str = "backing";

/// The frontend's handle on a backing id: what an open of the blob is answered with.
pub(super) type Guard = Arc<dyn Any + Send + Sync>;

/// A backing id an earlier release registered on the connection, as the guard the FUSE frontend
/// answers an open with holds it: the id alone, since the kernel closes it when the connection
/// ends and this process never does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BackingId(pub u32);

/// A line of the file: a blob with a live id in an earlier process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Saved {
    /// The blob.
    pub blob: TreeDigest,
    /// Its backing id.
    pub id: u32,
    /// The opens, if the line says that the count is exact (otherwise at least this many).
    pub opens: u32,
    /// Whether the line says the count is exact.
    pub exact: bool,
}

/// The lines of the file at `path`: none for a file that is not there. A line that is not
/// `<sha256> <id> <opens>` is said and left out.
///
/// # Errors
/// What the system said when the file exists and cannot be read.
pub(super) fn saved(path: &Path) -> io::Result<Vec<Saved>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(why) => return Err(why),
    };
    let mut lines = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split(' ').collect();
        let parsed = match words[..] {
            [blob, id, opens, ref rest @ ..] => TreeDigest::from_hex(blob)
                .zip(id.parse().ok())
                .zip(opens.parse().ok())
                .map(|((blob, id), opens)| Saved {
                    blob,
                    id,
                    opens,
                    exact: rest.first() == Some(&"exact"),
                }),
            _ => None,
        };
        match parsed {
            Some(saved) => lines.push(saved),
            None => tracing::warn!("tierstore fs: a line of the backing table is not read: {line}"),
        }
    }
    Ok(lines)
}

/// The backing ids of the connection, by blob: fixed at the take-over, read with no lock.
pub(super) struct Adopted {
    ids: HashMap<TreeDigest, Guard>,
}

impl Adopted {
    /// The ids of `saved`, the lines of the file at `path`; `wrap` makes the frontend's handle on
    /// an id. The file is written again with no line marked `exact`, if one is. Each handle is kept for the
    /// life of the process and one reference to it is given up, so that it is never dropped: the
    /// ids are the connection's, and a process that takes over from this one finds them too.
    pub(super) fn new(path: &Path, saved: &[Saved], wrap: &dyn Fn(u32) -> Guard) -> Self {
        let ids = saved
            .iter()
            .map(|line| {
                let guard = wrap(line.id);
                std::mem::forget(Arc::clone(&guard));
                (line.blob, guard)
            })
            .collect();
        if saved.iter().any(|line| line.exact) {
            unmark(path, saved);
        }
        Self { ids }
    }

    /// The handle on the id of `blob`, if an earlier release registered one.
    pub(super) fn get(&self, blob: &TreeDigest) -> Option<&Guard> {
        self.ids.get(blob)
    }

    /// The blobs with an id.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Writes the file at `path` as `saved`, with no count marked exact; a file that cannot be written
/// is said, and the ids are served as they are.
fn unmark(path: &Path, saved: &[Saved]) {
    let mut lines: Vec<String> = saved
        .iter()
        .map(|line| format!("{} {} {}\n", line.blob, line.id, line.opens))
        .collect();
    // The hex of the blob leads each line, and no two lines have one blob.
    lines.sort_unstable();
    let mut new = path.as_os_str().to_owned();
    new.push(".new");
    let written = std::fs::write(&new, lines.concat()).and_then(|()| std::fs::rename(&new, path));
    if let Err(why) = written {
        let file = path.display();
        tracing::warn!("tierstore fs: cannot write the backing table {file}: {why}");
    }
}

#[cfg(test)]
#[path = "backing_tests.rs"]
mod tests;
