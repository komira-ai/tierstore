//! A tree served straight from its version 2 tree list (the `TSTREE` image): the bytes of the list,
//! checked once by the walk of [`TreeListV2`], and read in place from then on.
//!
//! The list already is the tree. Its entries are in breadth-first order with each directory's
//! children next to one another and sorted by name, so an entry's number is its index (the root
//! is 0), a directory's children are the indices `first..first + count` of its record, a lookup
//! of a name is a binary search over that range, and what an entry is (kind, mode, size, digest,
//! the span of its bytes, a link's target) is its 32-byte record and its digest. Nothing is made
//! per entry: no name, no path and no per-directory record or hash. The tree is named by the
//! SHA-256 of the list's bytes, which the caller checks (the mount provider, against the key the
//! list is stored under).
//!
//! A file is read at its own span, the key, offset and size of its record: two files of one
//! digest at two spans are two reads, both checked against the digest.

use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;

use crate::tree_list_v2::{KIND_DIR, KIND_FILE, RECORD_LEN, TreeListV2, TreeListV2Error};
use crate::tree_list_v2_build::{Record, Spans, Walked};
use crate::{ListSpan, Mode, TreeDigest, TreeLimits};

/// A version 2 tree list whose every entry was checked, held whole and read in place.
#[derive(Debug)]
pub struct TreeImage {
    /// The whole list, as it is stored.
    bytes: Bytes,
    /// The entry records, 32 bytes each.
    entries: Bytes,
    /// The SHA-256 of each entry, 32 bytes each.
    digests: Bytes,
    /// The names and link targets.
    names: Bytes,
    /// The keys of the packs, shared by the spans of their files.
    keys: Vec<Arc<str>>,
    /// The files and their bytes.
    walked: Walked,
}

/// One entry of a [`TreeImage`], read from its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageEntry<'a> {
    /// Its index: its number in the tree, the root being 0.
    pub index: u32,
    /// The index of the directory that holds it; none for the root.
    pub parent: Option<u32>,
    /// Its name; empty for the root.
    pub name: &'a str,
    /// What it is.
    pub kind: ImageKind<'a>,
}

/// What an entry of a [`TreeImage`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageKind<'a> {
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
        span: Option<(&'a Arc<str>, u64)>,
    },
    /// A symbolic link, and what it says.
    Symlink { target: &'a str },
}

/// The limits a tree is checked against when only its shape matters, as in the mount, which
/// serves the trees the node's tree store accepted under the node's own limits.
const SHAPE: TreeLimits = TreeLimits {
    max_entries: u64::MAX,
    max_depth: usize::MAX,
    max_dir_entries: usize::MAX,
};

impl TreeImage {
    /// Opens the version 2 tree list `bytes` and walks it once ([`TreeListV2::open`], then the
    /// checks of [`TreeListV2::build`] against `limits`), calling `on_span` with each span of a
    /// file of size above 0 as the walk meets it, each distinct span once. Nothing of the list is
    /// copied but the keys of its packs; its SHA-256 is not checked here.
    ///
    /// The spans handed out before a refusal are not taken back, as with
    /// [`TreeListV2::build`]: each is checked against its digest when it is read.
    ///
    /// # Errors
    /// What [`TreeListV2::open`] and [`TreeListV2::build`] refuse.
    pub fn open(
        bytes: Bytes,
        limits: &TreeLimits,
        on_span: impl FnMut(ListSpan),
    ) -> Result<Self, TreeListV2Error> {
        Self::walked(bytes, limits, Spans::PerSpan, on_span)
    }

    /// Opens the version 2 tree list `bytes` as [`TreeImage::open`] does, checking its shape and
    /// none of the node's limits, and handing out no span.
    pub(crate) fn checked(bytes: Bytes) -> Result<Self, TreeListV2Error> {
        Self::walked(bytes, &SHAPE, Spans::None, |_| {})
    }

    fn walked(
        bytes: Bytes,
        limits: &TreeLimits,
        spans: Spans,
        on_span: impl FnMut(ListSpan),
    ) -> Result<Self, TreeListV2Error> {
        let list = TreeListV2::open(&bytes)?;
        let walked = list.walk(limits, spans, on_span)?;
        Ok(Self {
            entries: bytes.slice_ref(list.entries),
            digests: bytes.slice_ref(list.digests),
            names: bytes.slice_ref(list.names),
            keys: list.keys,
            walked,
            bytes,
        })
    }

    /// The bytes of the list, as it is stored.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// How many entries the tree has, the root included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / RECORD_LEN
    }

    /// Whether the tree is the root alone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 1
    }

    /// The number of files of the tree, of any size.
    #[must_use]
    pub fn files(&self) -> u64 {
        self.walked.files
    }

    /// The bytes of the files of the tree together, at most `u64::MAX`.
    #[must_use]
    pub fn file_bytes(&self) -> u64 {
        self.walked.bytes
    }

    /// The keys of the packs the files are read from, each once.
    pub(crate) fn keys(&self) -> &[Arc<str>] {
        &self.keys
    }

    fn record(&self, index: u32) -> Option<Record> {
        let start = (index as usize).checked_mul(RECORD_LEN)?;
        let bytes = self.entries.get(start..start + RECORD_LEN)?;
        Some(Record::of(bytes))
    }

    /// The bytes `start..start + len` of the names, as text: the walk checked every name and
    /// target to be text within the names.
    fn text(&self, start: u64, len: u64) -> &str {
        let range = usize::try_from(start)
            .ok()
            .zip(usize::try_from(len).ok())
            .and_then(|(start, len)| self.names.get(start..start.checked_add(len)?));
        range
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or_default()
    }

    /// The name of the entry of `record`.
    fn name(&self, record: Record) -> &str {
        self.text(u64::from(record.name_off), u64::from(record.name_len))
    }

    /// The entry `index`; none past the last.
    pub(crate) fn entry(&self, index: u32) -> Option<ImageEntry<'_>> {
        let record = self.record(index)?;
        let kind = match record.kind {
            KIND_DIR => {
                // The walk checked the children to be within the entries, which number fewer
                // than 2^32.
                let first = record.x;
                let count = u32::try_from(record.a).unwrap_or(0);
                ImageKind::Dir {
                    mode: record.mode(),
                    children: first..first.saturating_add(count),
                }
            }
            KIND_FILE => {
                let start = index as usize * RECORD_LEN;
                let mut sha256 = [0; 32];
                sha256.copy_from_slice(&self.digests[start..start + RECORD_LEN]);
                let span = (record.b > 0)
                    .then(|| self.keys.get(record.x as usize))
                    .flatten()
                    .map(|key| (key, record.a));
                ImageKind::File {
                    mode: record.mode(),
                    size: record.b,
                    sha256: TreeDigest::from_bytes(sha256),
                    span,
                }
            }
            _ => ImageKind::Symlink {
                target: self.text(record.a, record.b),
            },
        };
        Some(ImageEntry {
            index,
            parent: (index != 0).then_some(record.parent),
            name: self.name(record),
            kind,
        })
    }

    /// The indices of the children of the directory `dir`, in name order; empty for anything
    /// else.
    pub(crate) fn children(&self, dir: u32) -> Range<u32> {
        match self.entry(dir).map(|entry| entry.kind) {
            Some(ImageKind::Dir { children, .. }) => children,
            _ => 0..0,
        }
    }

    /// The name of the entry `index`, which the caller got from this tree.
    pub(crate) fn name_of(&self, index: u32) -> &str {
        self.record(index)
            .map(|record| self.name(record))
            .unwrap_or_default()
    }

    /// Whether the entry `index` is a directory.
    pub(crate) fn is_dir(&self, index: u32) -> bool {
        self.record(index)
            .is_some_and(|record| record.kind == KIND_DIR)
    }

    /// The index of the child of `dir` called `name`: a binary search of its children, which
    /// are sorted by name, bytewise.
    pub(crate) fn child(&self, dir: u32, name: &str) -> Option<u32> {
        let children = self.children(dir);
        let (mut low, mut high) = (children.start, children.end);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.name_of(middle).as_bytes().cmp(name.as_bytes()) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
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
            if self.name_of(middle).as_bytes() <= after.as_bytes() {
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
}

/// Why a path of a [`TreeImage`] leads nowhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImagePathError {
    /// A name of the path is below a file or a link.
    NotADirectory,
    /// A directory of the path has no child of the name.
    NotFound,
}

#[cfg(test)]
#[path = "tree_image_tests.rs"]
mod tests;
