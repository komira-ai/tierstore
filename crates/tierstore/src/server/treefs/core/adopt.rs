//! The core in a process that took a connection over (`docs/design/virtual-build-fs.md` section
//! 7.1): the backing ids the process before it registered, and the requests that name file handles
//! it gave out.
//!
//! The kernel keeps a file handle and a backing id for as long as an open file lives, whichever
//! process gave them. A read or a release then arrives for a handle this core never made. Its
//! number says so (`handle`), and its inode says which blob it was an open of (`ino.rs`), so a
//! cached open is made again on the first read that names it, and the release of a passthrough
//! open is counted on the blob's id as the process before would have counted it.

use std::sync::Arc;

use super::super::backing::{BackingTable, Guard, Saved};
use super::super::handle;
use super::super::ino;
use super::super::locked;
use super::{Core, Errno, Handle};
use crate::tree_manifest::{EntryKind, TreeDigest};

impl Core {
    /// Makes the opens of non-empty files passthrough opens from now on, over the ids that the
    /// process before this one registered (`saved`, its backing table); `wrap` makes the guard
    /// that closes one of them. The same as [`Core::enable_passthrough`] for a connection that
    /// has no ids yet.
    pub(in crate::server::treefs) fn adopt_passthrough(
        &self,
        saved: &[Saved],
        wrap: &dyn Fn(u32) -> Guard,
    ) {
        let table = BackingTable::adopt(
            self.backing_file.clone(),
            self.settings.max_backing_ids,
            saved,
            wrap,
        );
        // Once only, as for a first choice.
        let _ = self.backing.set(table);
    }

    /// Writes the backing table with its counts as they are now, marked exact: for a process that
    /// has stopped reading requests and is about to hand the connection over.
    pub(in crate::server::treefs) fn write_backing_exact(&self) {
        if let Some(table) = self.backing.get() {
            table.write_exact();
        }
    }

    /// Writes the backing table again with no count marked exact: a process that paused for a
    /// hand-over that did not complete serves again, and its counts are a lower bound again.
    #[cfg(target_os = "linux")]
    pub(in crate::server::treefs) fn write_backing_inexact(&self) {
        if let Some(table) = self.backing.get() {
            table.write_inexact();
        }
    }

    /// Leaves every backing id open in the kernel when this core goes: the process that takes the
    /// connection over adopts them. Dropping the guard of an id closes it, so one reference of
    /// each is given up without being dropped.
    #[cfg(target_os = "linux")]
    pub(in crate::server::treefs) fn abandon_backing(&self) {
        if let Some(table) = self.backing.get() {
            table.abandon();
        }
    }

    /// Whether `fh` was given out by a process before this one.
    fn foreign(&self, fh: u64) -> bool {
        handle::generation_of(fh) != self.settings.generation
    }

    /// The blob that the file with inode number `ino` has, if it has one.
    fn blob_of(&self, ino: u64) -> Result<Option<TreeDigest>, Errno> {
        let (number, index) = ino::split(ino);
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        Ok(match &entry.kind {
            EntryKind::File { size, digest, .. } if *size > 0 => Some(*digest),
            _ => None,
        })
    }

    /// [`Core::read`] of the open `fh` of the file `ino`. A handle that an earlier process gave
    /// out is made again here, as a cached open, on the first read: the page cache of the kernel
    /// is kept, and a read it misses reaches this process.
    ///
    /// # Errors
    /// As [`Core::read`]; and those of an open for the handle that is made again.
    pub(in crate::server::treefs) fn read_open(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        let known = locked(&self.handles).contains_key(&fh);
        if !known && self.foreign(fh) && !handle::is_backed(fh) {
            let handle = self.reopen(ino)?;
            let _ = locked(&self.handles)
                .entry(fh)
                .or_insert_with(|| Arc::new(handle));
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
            } => Ok(Handle::Cached(self.blob(
                &loaded,
                number,
                entry,
                (digest, *size, source.as_ref()),
            )?)),
            _ => Err(Errno::Inval),
        }
    }

    /// [`Core::release`] of the open `fh` of the file `ino`. The release of a passthrough open that
    /// an earlier process made ends one of the opens its blob's id was counted for.
    pub(in crate::server::treefs) fn release_open(&self, ino: u64, fh: u64) {
        if locked(&self.handles).contains_key(&fh) {
            self.release(fh);
        } else if self.foreign(fh)
            && handle::is_backed(fh)
            && let (Ok(Some(blob)), Some(table)) = (self.blob_of(ino), self.backing.get())
        {
            table.release_foreign(&blob);
        }
    }
}

#[cfg(test)]
#[path = "adopt_tests.rs"]
mod tests;
