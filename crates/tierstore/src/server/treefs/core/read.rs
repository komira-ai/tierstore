//! Reads by inode (`docs/design/virtual-build-fs.md` section 6): for a frontend whose protocol has
//! no open and no release, as NFS has none. [`Core::read_ino`] reads a file's blob from the cache
//! file, filled first if the cache does not hold it, and keeps nothing open: no handle, no
//! backing id. FUSE's opens, handles and passthrough stay in `core.rs` and `core/passthrough.rs`.

use std::fs::File;
use std::os::unix::fs::FileExt as _;

use crate::tree_manifest::EntryKind;

use super::{Core, Errno, ino};

/// Reads up to `size` bytes of `file` at `offset`; fewer at the end of the file.
///
/// # Errors
/// [`Errno::Io`] for a read that fails.
pub(super) fn read_file(file: &File, offset: u64, size: usize) -> Result<Vec<u8>, Errno> {
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the NFS frontend (a later change) is its caller")
    )]
    pub(in crate::server::treefs) fn read_ino(
        &self,
        ino: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, Errno> {
        #[cfg(test)]
        self.note("read_ino");
        let (number, index) = ino::split(ino);
        if number == 0 {
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
        let wanted = length.saturating_sub(offset).min(u64::from(size));
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let file = self.blob(&loaded, number, entry, (digest, length, source.as_ref()))?;
        // `wanted` is at most `size`, a `u32`.
        read_file(&file, offset, wanted as usize)
    }
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
