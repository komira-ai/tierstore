//! The opens that are answered now, with no request of the fill socket, on the reader that read
//! them (`node_fs.rs`, `fetches.rs`): the open of a file of a tree in memory whose bytes the
//! process can have without asking the node. Every other open is parked and made by
//! [`Core::open_from`] on a thread of the fetches, so that no reader waits on the fill socket.
//!
//! The bytes are had without the node:
//! - for a file of a tree of manifests, when the cache file of its blob opens and the blob needs
//!   no pin asked (it is pinned for the tree already, or the tree is not attached, which pins
//!   nothing);
//! - for a file of a placed tree, when the tree was given the span of that file before and the
//!   pack's file it read it from is still open (`Loaded::held_span`); or when the node holds the
//!   span now and says so without waiting (`Fills::span_held`, which only a core in the node's
//!   process answers: the whole pack of a job's output on the node that wrote it, or the ranges
//!   a node filled before) and the pack's file opens; the span is then kept with the tree;
//! - for an empty file, always;
//! - for a blob an earlier release registered a backing id for, when no pin is asked, as for a
//!   blob of the cache: the open is answered with that id.
//!
//! The open is then the one [`Core::open`] makes: the same handle, the same descriptor shared.

use crate::server::local_cache::blob_path;
use crate::tree_manifest::{BlobSource, EntryKind, TreeDigest};

use super::read::FileBytes;
use super::{Core, Errno, Handle, Loaded, Opened, ino};

impl Core {
    /// The open of `ino`, for writing if `writing`, where it is answered with no request of the
    /// fill socket (module docs); none where it is not, which [`Core::open_from`] then makes.
    pub(in crate::server::treefs) fn open_now(
        &self,
        ino: u64,
        writing: bool,
    ) -> Option<Result<Opened, Errno>> {
        if writing {
            return Some(Err(Errno::RoFs));
        }
        let (number, index) = ino::split(ino);
        if number == 0 || ino::is_placed_dir(ino) {
            return Some(Err(Errno::IsDir));
        }
        // A tree that is not in memory is read first, which may ask the fill socket.
        let loaded = self.in_memory(number)?;
        let Some(entry) = loaded.tree.entry(index) else {
            return Some(Err(Errno::NoEnt));
        };
        let (digest, size, source) = match &entry.kind {
            EntryKind::Dir { .. } => return Some(Err(Errno::IsDir)),
            EntryKind::Symlink { .. } => return Some(Err(Errno::Inval)),
            EntryKind::File { size: 0, .. } => {
                #[cfg(test)]
                self.note("open");
                return Some(Ok(Opened::Cached(self.new_handle(Handle::Empty))));
            }
            EntryKind::File {
                digest,
                size,
                source,
                ..
            } => (digest, *size, source.as_ref()),
        };
        let placed = loaded.tree.location().is_some();
        // A pin to ask first: the first open of a blob of an attached tree.
        let pins = !placed && !loaded.pinned.contains(digest) && self.attached(number);
        // An open answered with an earlier release's backing id, as `Core::open_from` answers it.
        let adopted = self.adopted.get().filter(|_| !placed);
        if let Some(id) = adopted.and_then(|adopted| adopted.get(digest)) {
            if pins {
                return None;
            }
            #[cfg(test)]
            self.note("open");
            let fh = self.new_handle(Handle::Adopted { ino });
            let id = std::sync::Arc::clone(id);
            return Some(Ok(Opened::Adopted { fh, id }));
        }
        let bytes = if placed {
            match loaded.held_span(digest, size) {
                Some(bytes) => bytes,
                // A span the tree was never given that the node holds is asked for at once, never
                // waited for: an open parked waits in the queue of the fetches, which every job of
                // the node shares, behind the opens that wait for fills.
                None => self.span_now(&loaded, (digest, size, source))?,
            }
        } else {
            if pins {
                return None;
            }
            let path = blob_path(&self.settings.cache_path, &digest.to_string());
            FileBytes::whole(self.open_files.get(&path).ok()?, size)
        };
        #[cfg(test)]
        self.note("open");
        Some(Ok(Opened::Cached(
            self.new_handle(Handle::Cached { bytes, ino }),
        )))
    }

    /// The bytes of a file of a placed tree, the blob `digest` of `size` bytes at `source`, if the
    /// node holds its span now (`Fills::span_held`) and the pack's file opens: kept with the tree
    /// for the opens that follow (`Loaded::keep_span`). None otherwise, and the open is parked.
    fn span_now(
        &self,
        loaded: &Loaded,
        (digest, size, source): (&TreeDigest, u64, Option<&BlobSource>),
    ) -> Option<FileBytes> {
        let source = source?;
        let sha = digest.to_string();
        let (base, path) = self
            .fills
            .span_held((&sha, size), (&source.key, source.offset))?;
        let file = self.open_files.get(&path).ok()?;
        let bytes = FileBytes { file, base, size };
        loaded.keep_span(digest, &bytes);
        Some(bytes)
    }

    /// Whether a read of the handle `fh` must first make the open again, which may ask the fill
    /// socket: a handle an earlier process gave out that no read made again yet (`core/adopt.rs`).
    pub(in crate::server::treefs) fn read_reopens(&self, fh: u64) -> bool {
        !self.handles.contains(fh) && self.foreign(fh)
    }
}

#[cfg(test)]
#[path = "now_tests.rs"]
mod tests;
