//! A tree served straight from its version 2 tree list (the `TSTREE` image): the bytes of the list,
//! checked once by the walk of [`TreeListV2`], and read in place from then on, by the server as a
//! [`ListImage`](crate::tree_image_list::ListImage), on the heap or mapped from the list's file.
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

#[cfg(feature = "server")]
use crate::ListRange;
use crate::tree_list_v2::{RECORD_LEN, TreeListV2, TreeListV2Error};
use crate::tree_list_v2_build::{Counted, Spans};
use crate::{ListSpan, TreeLimits};

/// A version 2 tree list whose every entry was checked, held whole on the heap. The server reads
/// it in place as a [`ListImage`](crate::tree_image_list::ListImage), on the heap or mapped from
/// its file.
pub struct TreeImage {
    /// The whole list, as it is stored.
    bytes: Bytes,
    /// Where the entry records are in the list, 32 bytes each.
    entries: Range<usize>,
    /// Where the SHA-256 of each entry is, 32 bytes each.
    #[cfg(feature = "server")]
    digests: Range<usize>,
    /// Where the names and link targets are.
    #[cfg(feature = "server")]
    names: Range<usize>,
    /// The keys of the packs, shared by the spans of their files.
    keys: Vec<Arc<str>>,
    /// The ranges of the packs the list's writer says the tree reads.
    #[cfg(feature = "server")]
    ranges: Vec<ListRange>,
    /// The files and their bytes.
    counted: Counted,
}

impl std::fmt::Debug for TreeImage {
    /// The size of the image and what it holds, not its bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeImage")
            .field("bytes", &self.bytes.len())
            .field("entries", &self.len())
            .field("files", &self.counted.files)
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "server")]
/// The limits a tree is checked against when only its shape matters, as in the mount, which
/// serves the trees the node's tree store accepted under the node's own limits.
const SHAPE: TreeLimits = TreeLimits {
    max_entries: u64::MAX,
    max_depth: usize::MAX,
    max_dir_entries: usize::MAX,
};

/// Where `part`, a slice of `whole`, is in it.
fn within(whole: &[u8], part: &[u8]) -> Range<usize> {
    let start = part.as_ptr().addr() - whole.as_ptr().addr();
    start..start + part.len()
}

impl TreeImage {
    /// Opens the version 2 tree list `bytes` and walks it once ([`TreeListV2::open`], then the
    /// checks of [`TreeListV2::check`] against `limits`), calling `on_span` with the span of each
    /// file of size above 0 as the walk meets it (a file is read at its own span; the caller reads
    /// a span that several files have once). Nothing of the list is copied but the keys of its packs and its ranges; its
    /// SHA-256 is not checked here.
    ///
    /// The spans handed out before a refusal are not taken back, as with
    /// [`TreeListV2::check`]: each is checked against its digest when it is read.
    ///
    /// # Errors
    /// What [`TreeListV2::open`] and [`TreeListV2::check`] refuse.
    pub fn open(
        bytes: Bytes,
        limits: &TreeLimits,
        on_span: impl FnMut(ListSpan),
    ) -> Result<Self, TreeListV2Error> {
        Self::walked(bytes, limits, Spans::PerFile, on_span)
    }

    #[cfg(feature = "server")]
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
        let (_, counted) = list.walk(limits, spans, on_span)?;
        Ok(Self {
            entries: within(&bytes, list.entries),
            #[cfg(feature = "server")]
            digests: within(&bytes, list.digests),
            #[cfg(feature = "server")]
            names: within(&bytes, list.names),
            #[cfg(feature = "server")]
            ranges: list.ranges().collect(),
            keys: list.keys,
            counted,
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
        self.counted.files
    }

    /// The bytes of the files of the tree together, at most `u64::MAX`.
    #[must_use]
    pub fn file_bytes(&self) -> u64 {
        self.counted.bytes
    }
}

#[cfg(feature = "server")]
impl TreeImage {
    /// The keys of the packs the files are read from, each once: a span's key is its number
    /// here.
    pub(crate) fn keys(&self) -> &[Arc<str>] {
        &self.keys
    }

    /// The ranges of the packs the list's writer says the tree reads: a hint, as
    /// [`TreeListV2::ranges`] gives them.
    pub(crate) fn ranges(&self) -> impl Iterator<Item = ListRange> + '_ {
        self.ranges.iter().cloned()
    }

    /// Where the entry records, the digests and the names are in the list.
    pub(crate) fn sections(&self) -> [Range<usize>; 3] {
        [
            self.entries.clone(),
            self.digests.clone(),
            self.names.clone(),
        ]
    }
}

#[cfg(test)]
#[path = "tree_image_tests.rs"]
pub(crate) mod tests;
