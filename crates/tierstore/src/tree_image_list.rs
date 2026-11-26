//! A version 2 tree list as the server reads it in place ([`ListImage`]): on the heap, as the
//! walk of a [`TreeImage`] left it, or mapped read-only from the list's image file, so that the
//! list's bytes are pages the kernel may drop and read again from the file, not a heap copy.
//!
//! The crate forbids `unsafe`, so a mapping is never seen as a `&[u8]`: it is read through
//! `vm-memory`, and every read copies the bytes it reads out of it (a 32-byte record, a digest, a
//! name into a buffer on the stack, a link's target), as `tierstore-tree`'s mapped images are read.
//! What the walk read (where the sections are, the keys of the packs) is kept from it, so a list is
//! walked once, before it is mapped, and never again for the mapping.
//!
//! An image file never changes once it is in place: the tree store writes it beside the walk
//! into a directory of its own and renames that directory into place, and it never writes,
//! punches or truncates an image while a tree names it (`server/local_cache/trees`). A file cut
//! short under a mapping would raise `SIGBUS` at the next read of a page past its end; nothing
//! cuts one. A file removed while it is mapped stays readable through the mapping until the
//! mapping is released. Bytes changed in place after the walk (a damaged disk, or a hand) read as
//! absent entries, empty names and empty listings, never as a panic: every read is bounded by the
//! section it reads, and a directory's children by the entries.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{self, Read as _};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use vm_memory::{FileOffset, MmapRegion, VolatileMemory as _};

use crate::tree_image::TreeImage;
use crate::tree_list_v2::{KIND_DIR, KIND_FILE, RECORD_LEN, TreeListV2Error};
use crate::tree_list_v2_build::Record;
use crate::{MAX_TREE_LIST_BYTES, Mode, TreeDigest};

/// The longest name of an entry, in bytes: a record's name length is one byte.
const NAME_MAX: usize = 255;

/// Where the bytes of a list are.
enum Backing {
    /// On the heap: the bytes the walk read.
    Heap(Bytes),
    /// A read-only, private mapping of the list's image file.
    Mapped(MmapRegion<()>),
}

/// A version 2 tree list whose every entry was checked, read in place on the heap or from a
/// mapping of its file (module docs).
pub(crate) struct ListImage {
    backing: Backing,
    /// The bytes of the list.
    len: u64,
    /// Where the entry records are, 32 bytes each.
    entries: Range<usize>,
    /// Where the SHA-256 of each entry is, 32 bytes each.
    digests: Range<usize>,
    /// Where the names and link targets are.
    names: Range<usize>,
    /// The keys of the packs, shared by the spans of their files.
    keys: Vec<Arc<str>>,
}

impl std::fmt::Debug for ListImage {
    /// The size of the list, whether it is mapped, and what it holds, not its bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListImage")
            .field("bytes", &self.len)
            .field("mapped", &self.is_mapped())
            .field("entries", &self.len())
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

/// One entry of a [`ListImage`], read from its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageEntry {
    /// Its index: its number in the tree, the root being 0.
    pub index: u32,
    /// The index of the directory that holds it; none for the root.
    pub parent: Option<u32>,
    /// Its name; empty for the root.
    pub name: String,
    /// What it is.
    pub kind: ImageKind,
}

/// What an entry of a [`ListImage`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageKind {
    /// A directory: its mode and the indices of its children, in name order.
    Dir { mode: Mode, children: Range<u32> },
    /// A regular file.
    File {
        /// Its mode: the permission bits, setuid, setgid and sticky.
        mode: Mode,
        /// Its length.
        size: u64,
        /// The SHA-256 of its bytes.
        sha256: TreeDigest,
        /// Where its bytes are, the key of a pack and the offset in it; none for an empty file.
        span: Option<(Arc<str>, u64)>,
    },
    /// A symbolic link, and what it says.
    Symlink { target: String },
}

/// Why a path of a [`ListImage`] leads nowhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImagePathError {
    /// A name of the path is below a file or a link.
    NotADirectory,
    /// A directory of the path has no child of the name.
    NotFound,
}

/// Why the image file of a list was not read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ImageFileError {
    /// The file could not be opened, read or mapped.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The file is larger than a list may be.
    #[error("{size} bytes, more than a tree list's {max}")]
    TooLarge { size: u64, max: usize },
    /// The bytes are not a tree list the walk accepts.
    #[error("{0}")]
    List(#[from] TreeListV2Error),
}

/// The size of a page of memory, which a mapping takes whole.
fn page() -> u64 {
    rustix::param::page_size() as u64
}

/// `len` bytes rounded up to whole pages: what a mapping of them takes.
pub(crate) fn in_pages(len: u64) -> u64 {
    len.div_ceil(page()).saturating_mul(page())
}

impl ListImage {
    /// The list `image` read in place on the heap, from the bytes its walk read.
    pub(crate) fn heap(image: &TreeImage) -> Self {
        Self::over(image, Backing::Heap(image.bytes().clone()))
    }

    /// The list `image` read in place from a read-only, private mapping of `file`, which holds the
    /// bytes `image` was walked from (the tree store wrote them there, or the caller read them from
    /// it). The walk is not made again.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidData`] for a file whose length is not the list's, and what the
    /// system said when the file cannot be measured or mapped.
    pub(crate) fn mapped(image: &TreeImage, file: File) -> io::Result<Self> {
        let len = image.bytes().len();
        if !file.metadata().is_ok_and(|meta| meta.len() == len as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the image file is not the length of its list",
            ));
        }
        let map = MmapRegion::build(
            Some(FileOffset::new(file, 0)),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
        )
        .map_err(io::Error::other)?;
        Ok(Self::over(image, Backing::Mapped(map)))
    }

    /// The list `image` read from the image file at `path` it was written to: mapped, or on the
    /// heap where the file cannot be mapped (it is gone, or not the list).
    pub(crate) fn stored(image: &TreeImage, path: &Path) -> Self {
        File::open(path)
            .and_then(|file| Self::mapped(image, file))
            .unwrap_or_else(|_| Self::heap(image))
    }

    /// The list in the image file at `path`, read whole once and walked for its shape
    /// ([`TreeImage::checked`]), then mapped (on the heap where it cannot be); the bytes read are
    /// dropped. Its SHA-256 is not
    /// checked: the tree store checked it when it took the list.
    ///
    /// # Errors
    /// [`ImageFileError`]: the file cannot be read or mapped, is too large, or is no list.
    pub(crate) fn open_file(path: &Path) -> Result<Self, ImageFileError> {
        let file = File::open(path)?;
        let size = file.metadata().map_or(0, |meta| meta.len());
        if size > MAX_TREE_LIST_BYTES as u64 {
            return Err(ImageFileError::TooLarge {
                size,
                max: MAX_TREE_LIST_BYTES,
            });
        }
        let mut bytes = Vec::new();
        let _ = (&file)
            .take(MAX_TREE_LIST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let image = TreeImage::checked(bytes.into())?;
        Ok(Self::mapped(&image, file).unwrap_or_else(|_| Self::heap(&image)))
    }

    fn over(image: &TreeImage, backing: Backing) -> Self {
        let [entries, digests, names] = image.sections();
        Self {
            backing,
            len: image.bytes().len() as u64,
            entries,
            digests,
            names,
            keys: image.keys().to_vec(),
        }
    }

    /// Whether the list is read from a mapping of its file.
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self.backing, Backing::Mapped(_))
    }

    /// The bytes the list's mapping takes, in whole pages; 0 for a list on the heap.
    pub(crate) fn mapped_bytes(&self) -> u64 {
        if self.is_mapped() {
            in_pages(self.len)
        } else {
            0
        }
    }

    /// The bytes of the list.
    pub(crate) fn list_bytes(&self) -> u64 {
        self.len
    }

    /// How many entries the tree has, the root included.
    pub(crate) fn len(&self) -> usize {
        self.entries.len() / RECORD_LEN
    }

    /// The keys of the packs the files are read from, each once: a span's key is its number
    /// here.
    pub(crate) fn keys(&self) -> &[Arc<str>] {
        &self.keys
    }

    /// Copies the bytes at `at..at + buf.len()` of `section` into `buf`; none for bytes past its
    /// end.
    fn copy(&self, section: &Range<usize>, at: usize, buf: &mut [u8]) -> Option<()> {
        let start = section.start.checked_add(at)?;
        let end = start
            .checked_add(buf.len())
            .filter(|end| *end <= section.end)?;
        // The walk checked every section to be within the list, which the heap holds and the
        // mapping maps whole.
        match &self.backing {
            Backing::Heap(bytes) => {
                buf.copy_from_slice(&bytes[start..end]);
                Some(())
            }
            Backing::Mapped(map) => map.get_slice(start, buf.len()).ok().map(|slice| {
                let _ = slice.copy_to(buf);
            }),
        }
    }

    fn record(&self, index: u32) -> Option<Record> {
        let mut bytes = [0; RECORD_LEN];
        self.copy(&self.entries, index as usize * RECORD_LEN, &mut bytes)?;
        Some(Record::of(&bytes))
    }

    /// The bytes `start..start + len` of the names, copied into `buf`, which is at least `len`
    /// long; none past the names or the buffer.
    fn text_into<'b>(&self, start: u64, len: u64, buf: &'b mut [u8]) -> Option<&'b [u8]> {
        // A `u64` is a `usize` on the 64-bit targets tierstore runs on.
        let (start, len) = (start as usize, len as usize);
        let out = buf.get_mut(..len)?;
        self.copy(&self.names, start, out)?;
        Some(out)
    }

    /// The name of the entry of `record`, as bytes copied into `buf`; empty where it cannot be
    /// read.
    fn name_bytes<'b>(&self, record: Record, buf: &'b mut [u8; NAME_MAX]) -> &'b [u8] {
        let (start, len) = (u64::from(record.name_off), u64::from(record.name_len));
        match self.text_into(start, len, buf) {
            Some(name) => name,
            None => &[],
        }
    }

    /// The name of the entry of `record`, as text; empty where it is not (the walk checked every
    /// name to be text).
    fn name(&self, record: Record) -> String {
        let mut buf = [0; NAME_MAX];
        let bytes = self.name_bytes(record, &mut buf);
        std::str::from_utf8(bytes).unwrap_or_default().to_owned()
    }

    /// The target of the link of `record`, as text; empty where it cannot be read. Bounded by the
    /// names before anything is allocated for it.
    fn target(&self, record: Record) -> String {
        let fits = record
            .a
            .checked_add(record.b)
            .is_some_and(|end| end <= self.names.len() as u64);
        let len = if fits { record.b as usize } else { 0 };
        let mut buf = vec![0; len];
        self.text_into(record.a, record.b, &mut buf)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// The SHA-256 of the entry `index`, which has a record: the walk checked the digests to be
    /// as many as the records.
    fn digest(&self, index: u32) -> TreeDigest {
        let mut sha256 = [0; 32];
        let _ = self.copy(&self.digests, index as usize * RECORD_LEN, &mut sha256);
        TreeDigest::from_bytes(sha256)
    }

    /// The indices `first..first + count`, cut to the entries the tree has.
    fn bounded(&self, first: u32, count: u64) -> Range<u32> {
        let len = u32::try_from(self.len()).unwrap_or(u32::MAX);
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        let start = first.min(len);
        start..first.saturating_add(count).min(len)
    }

    /// The entry `index`; none past the last.
    pub(crate) fn entry(&self, index: u32) -> Option<ImageEntry> {
        let record = self.record(index)?;
        let kind = match record.kind {
            KIND_DIR => ImageKind::Dir {
                mode: record.mode(),
                children: self.bounded(record.x, record.a),
            },
            KIND_FILE => ImageKind::File {
                mode: record.mode(),
                size: record.b,
                sha256: self.digest(index),
                span: (record.b > 0)
                    .then(|| self.keys.get(record.x as usize))
                    .flatten()
                    .map(|key| (Arc::clone(key), record.a)),
            },
            _ => ImageKind::Symlink {
                target: self.target(record),
            },
        };
        Some(ImageEntry {
            index,
            parent: (index != 0).then_some(record.parent),
            name: self.name(record),
            kind,
        })
    }

    /// The stored mode of the entry `index`, a link's being [`Mode::SYMLINK`]; none past the last.
    /// Its record alone is read.
    pub(crate) fn mode(&self, index: u32) -> Option<Mode> {
        let record = self.record(index)?;
        Some(match record.kind {
            KIND_DIR | KIND_FILE => record.mode(),
            _ => Mode::SYMLINK,
        })
    }

    /// The indices of the children of the directory `dir`, in name order; empty for anything
    /// else.
    pub(crate) fn children(&self, dir: u32) -> Range<u32> {
        match self.record(dir) {
            Some(record) if record.kind == KIND_DIR => self.bounded(record.x, record.a),
            _ => 0..0,
        }
    }

    /// The name of the entry `index`, which the caller got from this tree; empty past the last.
    #[cfg(test)]
    pub(crate) fn name_of(&self, index: u32) -> String {
        self.record(index)
            .map(|record| self.name(record))
            .unwrap_or_default()
    }

    /// Whether the entry `index` is a directory.
    pub(crate) fn is_dir(&self, index: u32) -> bool {
        self.record(index)
            .is_some_and(|record| record.kind == KIND_DIR)
    }

    /// How the name of the entry `index` sorts against `name`, bytewise, with no allocation; an
    /// entry whose name cannot be read sorts as the empty name.
    fn cmp_name(&self, index: u32, name: &[u8]) -> Ordering {
        let mut buf = [0; NAME_MAX];
        let ours = self
            .record(index)
            .map_or(&[][..], |record| self.name_bytes(record, &mut buf));
        ours.cmp(name)
    }

    /// The index of the child of `dir` called `name`: a binary search of its children, which
    /// are sorted by name, bytewise.
    pub(crate) fn child(&self, dir: u32, name: &str) -> Option<u32> {
        let children = self.children(dir);
        let (mut low, mut high) = (children.start, children.end);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.cmp_name(middle, name.as_bytes()) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// The index of the first child of `dir` whose name sorts after `after`: where a listing
    /// that goes on from `after` begins.
    pub(crate) fn children_after(&self, dir: u32, after: &str) -> Range<u32> {
        let children = self.children(dir);
        let (mut low, mut high) = (children.start, children.end);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.cmp_name(middle, after.as_bytes()) != Ordering::Greater {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low..children.end
    }

    /// The entry the path `path` (names joined by `/`; empty for the root) leads to.
    ///
    /// # Errors
    /// [`ImagePathError::NotADirectory`] for a name below a file or a link, and
    /// [`ImagePathError::NotFound`] for a name a directory does not have.
    pub(crate) fn find(&self, path: &str) -> Result<u32, ImagePathError> {
        let mut at = 0;
        for name in path.split('/').filter(|_| !path.is_empty()) {
            if !self.is_dir(at) {
                return Err(ImagePathError::NotADirectory);
            }
            at = self.child(at, name).ok_or(ImagePathError::NotFound)?;
        }
        Ok(at)
    }

    /// The entry the path `path` leads to, as [`ListImage::find`] finds it.
    ///
    /// # Errors
    /// As [`ListImage::find`].
    pub(crate) fn find_entry(&self, path: &str) -> Result<ImageEntry, ImagePathError> {
        // `find` gives the index of an entry of the image.
        self.entry(self.find(path)?).ok_or(ImagePathError::NotFound)
    }
}

#[cfg(test)]
#[path = "tree_image_list_tests.rs"]
mod tests;
