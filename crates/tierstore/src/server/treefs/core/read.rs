//! Reads by inode (`docs/design/virtual-build-fs.md` section 6): for a frontend whose protocol has
//! no open and no release, as NFS has none. [`Core::blob_file`] gives a file's blob as the cache
//! file, filled first if the cache does not hold it, and keeps nothing open: no handle, no
//! backing id; the frontend keeps the descriptor as long as it likes and reads it with
//! [`read_file`]. [`Core::readdir_after`] lists a directory from the last inode a client saw.
//! FUSE's opens, handles and passthrough stay in `core.rs` and `core/passthrough.rs`.

use std::fs::File;
use std::os::unix::fs::FileExt as _;

use crate::tree_manifest::EntryKind;

use super::{Core, DirItem, Entry, Errno, TREE_TTL, ino};

/// The bytes of a file of a tree: `size` bytes of `file` from `base`. The file is the cache file
/// of the blob, from 0, or the file of the pack the file is a span of, from the span's offset.
#[derive(Debug)]
pub(in crate::server::treefs) struct FileBytes {
    /// The file that holds the bytes.
    pub file: File,
    /// Where the first byte is in it.
    pub base: u64,
    /// How many bytes there are.
    pub size: u64,
}

impl FileBytes {
    /// The whole of `file`, the cache file of a blob of `size` bytes.
    pub(in crate::server::treefs) fn whole(file: File, size: u64) -> Self {
        Self {
            file,
            base: 0,
            size,
        }
    }

    /// Reads up to `size` bytes at `offset`; fewer at the end, none past it.
    ///
    /// # Errors
    /// [`Errno::Io`] for a read that fails.
    pub(in crate::server::treefs) fn read(
        &self,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, Errno> {
        // At most `size`, a `usize`.
        let wanted = self.size.saturating_sub(offset).min(size as u64) as usize;
        read_file(&self.file, self.base.saturating_add(offset), wanted)
    }
}

/// Reads up to `size` bytes of `file` at `offset`; fewer at the end of the file.
///
/// # Errors
/// [`Errno::Io`] for a read that fails.
pub(in crate::server::treefs) fn read_file(
    file: &File,
    offset: u64,
    size: usize,
) -> Result<Vec<u8>, Errno> {
    let mut data = vec![0u8; size];
    let mut have = 0;
    while have < data.len() {
        let got = file
            .read_at(&mut data[have..], offset + have as u64)
            .map_err(|_| Errno::Io)?;
        if got == 0 {
            break;
        }
        have += got;
    }
    data.truncate(have);
    Ok(data)
}

impl Core {
    /// Reads up to `size` bytes at `offset` of the file `ino`; fewer at its end, none past it. A
    /// blob the cache does not hold is filled first through the fill socket; a fill that fails
    /// is `Errno::Io` and is recorded on the attachments of the tree, as for an open.
    ///
    /// The bytes are asked for by the manifest's size, so that a request for more than the file
    /// has allocates no more than the file has.
    ///
    /// # Errors
    /// [`Errno::IsDir`] for a directory, [`Errno::Inval`] for a link, [`Errno::NoEnt`] for an
    /// inode no tree has, [`Errno::Io`] for a blob that cannot be had or read.
    #[cfg(test)]
    pub(in crate::server::treefs) fn read_ino(
        &self,
        ino: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        use super::Kind;
        self.note("read_ino");
        let attr = self.getattr(ino)?.attr;
        match attr.kind {
            Kind::Dir => return Err(Errno::IsDir),
            Kind::Symlink => return Err(Errno::Inval),
            Kind::File => {}
        }
        let wanted = attr.size.saturating_sub(offset).min(u64::from(size));
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let (bytes, _) = self.blob_file(ino)?.ok_or(Errno::Io)?;
        // `wanted` is at most `size`, a `u32`.
        bytes.read(offset, wanted as usize)
    }

    /// The bytes of the file `ino` and its length, filled first through the fill socket if the
    /// cache does not hold them; `None` for a file of no bytes, which has no blob. A frontend
    /// that keeps descriptors open reads them with [`FileBytes::read`], and does not come back for
    /// as long as it keeps the descriptor.
    ///
    /// # Errors
    /// As [`Core::read_ino`], but for the bytes.
    pub(in crate::server::treefs) fn blob_file(
        &self,
        ino: u64,
    ) -> Result<Option<(FileBytes, u64)>, Errno> {
        #[cfg(test)]
        self.note("blob_file");
        let (number, index) = ino::split(ino);
        if number == 0 || ino::is_placed_dir(ino) {
            return Err(Errno::IsDir);
        }
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        let (digest, length, source) = match &entry.kind {
            EntryKind::Dir { .. } => return Err(Errno::IsDir),
            EntryKind::Symlink { .. } => return Err(Errno::Inval),
            EntryKind::File {
                digest,
                size: length,
                source,
                ..
            } => (digest, *length, source),
        };
        if length == 0 {
            return Ok(None);
        }
        let file = self.blob(&loaded, number, &entry, (digest, length, source.as_ref()))?;
        Ok(Some((file, length)))
    }

    /// The inode of the parent of `ino`: the root of the mount for itself and for `trees/`'s
    /// parent, `trees/` for the root of a tree, and for a placed tree's root the directory above
    /// its location (`core/placed.rs`).
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for an inode no tree and no namespace has.
    pub(in crate::server::treefs) fn parent(&self, ino: u64) -> Result<u64, Errno> {
        #[cfg(test)]
        self.note("parent");
        let (number, index) = ino::split(ino);
        if ino::is_placed_dir(ino) {
            return self.placed_parent(ino);
        }
        if number == 0 {
            return match ino {
                ino::ROOT | ino::TREES => Ok(ino::ROOT),
                _ => Err(Errno::NoEnt),
            };
        }
        let loaded = self.tree(number)?;
        let entry = loaded.tree.entry(index).ok_or(Errno::NoEnt)?;
        Ok(entry.parent.map_or_else(
            || self.root_parent(&loaded.tree),
            |parent| ino::of(number, parent),
        ))
    }

    /// Gives the entries of the directory `ino` whose inode number is above `after` (0 for all),
    /// in ascending order of inode number, to `sink` until it answers false; `.` and `..` are not
    /// among them. The inode numbers of a directory's entries ascend in their order, so that the
    /// last inode a client saw is its place in the listing, whatever the entries were then.
    ///
    /// # Errors
    /// As [`Core::opendir`]: [`Errno::Acces`] for the namespace's directories.
    pub(in crate::server::treefs) fn readdir_after(
        &self,
        ino: u64,
        after: u64,
        sink: &mut dyn FnMut(DirItem) -> bool,
    ) -> Result<(), Errno> {
        #[cfg(test)]
        self.note("readdir_after");
        self.opendir(ino)?;
        let (number, index) = ino::split(ino);
        let loaded = self.tree(number)?;
        let kids = loaded.tree.children(index);
        let first = kids.partition_point(|kid| ino::of(number, kid) <= after);
        let entries = kids.from(first).filter_map(|kid| loaded.tree.entry(kid));
        for entry in entries {
            let item = DirItem {
                name: entry.name.to_string(),
                entry: Entry {
                    attr: self.attr_of(number, &loaded.tree, &entry),
                    ttl: TREE_TTL,
                },
            };
            if !sink(item) {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
