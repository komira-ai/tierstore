//! The core in a process that took a connection over (`docs/design/virtual-build-fs.md` section
//! 7.1): the backing ids an earlier release registered on the connection, and the requests that
//! name file handles the process before gave out.
//!
//! The kernel keeps a file handle for as long as an open file lives, whichever process gave it. A
//! read or a release then arrives for a handle this core never made. Its number says so
//! (`handle`), and its inode says which blob it was an open of (`ino.rs`), so a cached open is
//! made again on the first read that names it, and a release of one that no read made again has
//! nothing to end.

use super::super::backing::{Adopted, Guard, Saved};
use super::super::handle;
use super::super::ino;
use super::{Core, Errno, Handle};
use crate::tree_manifest::EntryKind;
use std::path::Path;

impl Core {
    /// Answers the opens of the blobs of `saved` (the lines of the file at `path`, which the
    /// process before kept) with the backing ids an earlier release registered for them, from now
    /// on; `wrap` makes the frontend's handle on an id. For a connection whose `INIT` an earlier
    /// release answered with passthrough; called once, before the first request is read.
    pub(in crate::server::treefs) fn adopt_backing(
        &self,
        path: &Path,
        saved: &[Saved],
        wrap: &dyn Fn(u32) -> Guard,
    ) {
        // Once only: a second call (a hand-over that did not complete) finds the ids it has.
        let _ = self.adopted.get_or_init(|| Adopted::new(path, saved, wrap));
    }

    /// The blobs answered with an id of an earlier release.
    #[cfg(test)]
    pub(in crate::server::treefs) fn adopted_ids(&self) -> usize {
        self.adopted.get().map_or(0, Adopted::len)
    }

    /// Whether `fh` was given out by a process before this one.
    pub(super) fn foreign(&self, fh: u64) -> bool {
        handle::generation_of(fh) != self.settings.generation
    }

    /// [`Core::read`] of the open `fh` of the file `ino`. A handle that an earlier process gave
    /// out is made again here, as a cached open, on the first read: the page cache of the kernel
    /// is kept, and a read it misses reaches this process.
    ///
    /// # Errors
    /// As [`Core::read`]; and those of an open for the handle that is made again, but that a read
    /// is never answered `ENOENT`: a handle whose tree is gone (its placement ended while the
    /// process was replaced) is `EIO`, said on stderr, as a read of an open file that fails.
    pub(in crate::server::treefs) fn read_open(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        if !self.handles.contains(fh) && self.foreign(fh) {
            let handle = self.reopen(ino).map_err(|error| match error {
                Errno::NoEnt => {
                    let why = "the file was opened by the process before this one, and its tree \
                               is gone";
                    self.unreadable(ino, offset, why)
                }
                error => error,
            })?;
            self.handles.insert_if_absent(fh, handle);
        }
        self.read(fh, offset, size)
    }

    /// The cached open of the file `ino`, as an open of it would make it.
    fn reopen(&self, ino: u64) -> Result<Handle, Errno> {
        let (number, index) = ino::split(ino);
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        match &entry.kind {
            EntryKind::File { size: 0, .. } => Ok(Handle::Empty),
            EntryKind::File {
                digest,
                size,
                source,
                ..
            } => Ok(Handle::Cached {
                bytes: self.blob(
                    (&loaded, number),
                    &entry,
                    (digest, *size, source.as_ref()),
                    self.now(),
                )?,
                ino,
            }),
            _ => Err(Errno::Inval),
        }
    }
}

#[cfg(test)]
#[path = "adopt_tests.rs"]
mod tests;
