//! [`Image`]: a tree's L2 list, mapped read-only and read by copying records out (5.3).

use std::fs::File;
use std::io;
use std::num::NonZeroU64;
use std::ops::Range;
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;

use rustix::fs::{Advice as Fadvice, fadvise};
use tierstore_fetch_api::ids::PackId;
use tierstore_fetch_api::list::{ListL2 as _, NodeIndex};
use tierstore_list::{ENTRY_LEN, EntryRecord, RecordKind, Section, Shape, TreeListL2};
use vm_memory::{FileOffset, MmapRegion, VolatileMemory as _};

use crate::error::TreeError;

/// The longest name of an entry, in bytes (the record's `name_len` is one byte).
pub const NAME_MAX: usize = 255;

/// How a tree's image is held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageOptions {
    /// A hot mount: the record sections (entries and names) are also mapped `MAP_LOCKED`, so the
    /// kernel keeps their pages resident while the image is open. The image is small next to its
    /// packs' data, so the lock is bounded by the list's own size. A refused lock (a memlock
    /// limit) is reported in [`Advice::lock_refused`] and the image is served unlocked.
    pub lock: bool,
}

/// What was asked of the kernel for the image's pages at open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Advice {
    /// The bytes of the record sections advised `WILLNEED`: read ahead into the page cache.
    pub willneed_bytes: u64,
    /// The bytes mapped `MAP_LOCKED` (whole pages), 0 unless [`ImageOptions::lock`].
    pub locked_bytes: u64,
    /// Why the locked mapping was refused, if it was.
    pub lock_refused: Option<io::ErrorKind>,
}

/// A pack of the tree: its id (key and `pack_root`, computed from the list's cell digests) and
/// its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePack {
    /// The pack's id.
    pub id: PackId,
    /// The pack's size in bytes.
    pub size: u64,
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A directory: its children are the entries `first..first + count`, sorted by name.
    Dir {
        /// Its first child.
        first: u64,
        /// How many children it has.
        count: u64,
    },
    /// A file: `size` bytes at `offset` of pack `pack` (none for a file of size 0).
    File {
        /// The number of its pack in the list.
        pack: Option<u32>,
        /// Where its bytes begin in the pack.
        offset: u64,
        /// Its size.
        size: u64,
    },
    /// A symbolic link whose target is `len` bytes.
    Symlink {
        /// The target's length.
        len: u64,
    },
}

/// An entry of the tree, read from its record. Its name is read with [`Image::name`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    /// The entry's index: its inode in the image.
    pub index: NodeIndex,
    /// The directory it is in (the root's parent is the root).
    pub parent: NodeIndex,
    /// The permission bits with setuid, setgid and sticky (0 for a link).
    pub mode: u16,
    /// What it is.
    pub kind: NodeKind,
    name: (u32, u8),
    target: u64,
}

/// A tree's L2 list, mapped.
///
/// [`open`](Self::open) reads the file once and checks it whole with `tierstore-list` (the
/// format, and that it is the list of the tree id), then maps it `PROT_READ`, `MAP_PRIVATE` and
/// keeps no copy. The file must not change while it is open (the cache never writes, punches or
/// truncates an image); bytes changed after the check read as absent entries, never as a panic.
pub struct Image {
    map: MmapRegion<()>,
    _locked: Vec<MmapRegion<()>>,
    entries: Range<u64>,
    names: Range<u64>,
    len: u64,
    shape: Shape,
    tree_id: [u8; 32],
    packs: Box<[ImagePack]>,
    advice: Advice,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("len", &self.len)
            .field("packs", &self.packs.len())
            .field("advice", &self.advice)
            .finish_non_exhaustive()
    }
}

fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> TreeError {
    move |source| TreeError::Io { what, source }
}

/// A read-only mapping of `len` bytes of `file` from `start` with `flags` besides `MAP_PRIVATE`.
fn map(file: &Arc<File>, start: u64, len: usize, flags: i32) -> io::Result<MmapRegion<()>> {
    MmapRegion::build(
        Some(FileOffset::from_arc(Arc::clone(file), start)),
        len,
        libc::PROT_READ,
        libc::MAP_PRIVATE | flags,
    )
    .map_err(io::Error::other)
}

/// The whole pages that hold `range`, which ends within the file.
fn pages(range: &Range<u64>) -> Range<u64> {
    let page = rustix::param::page_size() as u64;
    let start = range.start / page * page;
    start..range.end.div_ceil(page) * page
}

impl Image {
    /// Opens the image in `file`, the L2 list of tree `tree_id`.
    ///
    /// # Errors
    ///
    /// [`TreeError::Io`] if the file cannot be read or mapped; [`TreeError::ListVersionUnsupported`]
    /// for a list this release does not read; [`TreeError::Malformed`] for one that breaks the
    /// format or is of another tree.
    pub fn open(file: File, tree_id: &[u8; 32], options: ImageOptions) -> Result<Self, TreeError> {
        let file = Arc::new(file);
        let size = file.metadata().map_err(io_error("stat"))?.len();
        let mut bytes = vec![0u8; bytes_len(size)];
        file.read_exact_at(&mut bytes, 0)
            .map_err(io_error("read"))?;
        let list = TreeListL2::open_sidecar(Some(bytes), tree_id).map_err(|e| match e {
            tierstore_list::OpenError::Unsupported(u) => TreeError::ListVersionUnsupported(u),
            tierstore_list::OpenError::Malformed(m) => TreeError::Malformed(m),
        })?;
        let packs = (0..list.pack_count())
            .filter_map(|pack| {
                let record = list.pack_record(pack)?;
                Some(ImagePack {
                    id: PackId {
                        key: record.key,
                        root: list.pack_root(pack)?,
                    },
                    size: record.pack_size,
                })
            })
            .collect();
        let entries = list.section_range(Section::Entries);
        let names = list.section_range(Section::Names);
        let (len, shape, tree_id) = (u64::from(list.len()), list.shape(), *list.tree_id());
        drop(list);

        // `open_sidecar` refused an empty file, so the mapping has at least the header.
        let whole = map(&file, 0, bytes_len(size), 0).map_err(io_error("map"))?;
        let mut advice = Advice::default();
        for range in [&entries, &names] {
            advice.willneed_bytes += NonZeroU64::new(range.end - range.start)
                .and_then(|len| {
                    fadvise(&*file, range.start, Some(len), Fadvice::WillNeed)
                        .ok()
                        .map(|()| len.get())
                })
                .unwrap_or(0);
        }
        let mut locked = Vec::new();
        if options.lock {
            for range in [&entries, &names].into_iter().filter(|r| r.end > r.start) {
                let span = pages(range);
                match map(
                    &file,
                    span.start,
                    bytes_len(span.end - span.start),
                    libc::MAP_LOCKED | libc::MAP_POPULATE,
                ) {
                    Ok(region) => {
                        advice.locked_bytes += span.end - span.start;
                        locked.push(region);
                    }
                    Err(error) => {
                        advice.lock_refused = Some(error.kind());
                        locked.clear();
                        advice.locked_bytes = 0;
                        break;
                    }
                }
            }
        }
        Ok(Self {
            map: whole,
            _locked: locked,
            entries,
            names,
            len,
            shape,
            tree_id,
            packs,
            advice,
        })
    }

    /// The number of entries, the root included.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the tree has nothing but the root.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 1
    }

    /// What the check at open learnt of the tree: entries, files, file bytes, depth.
    #[must_use]
    pub const fn shape(&self) -> Shape {
        self.shape
    }

    /// The tree id.
    #[must_use]
    pub const fn tree_id(&self) -> &[u8; 32] {
        &self.tree_id
    }

    /// The tree's packs, by their number in the list.
    #[must_use]
    pub fn packs(&self) -> &[ImagePack] {
        &self.packs
    }

    /// What was asked of the kernel at open.
    #[must_use]
    pub const fn advice(&self) -> Advice {
        self.advice
    }

    /// Copies `buf.len()` bytes at `at` of the mapping into `buf`.
    fn copy(&self, at: u64, buf: &mut [u8]) -> Option<()> {
        let copied = usize::try_from(at)
            .ok()
            .and_then(|at| self.map.get_slice(at, buf.len()).ok())
            .map(|slice| slice.copy_to(buf));
        (copied == Some(buf.len())).then_some(())
    }

    /// Entry `index`, or `None` past the last entry.
    #[must_use]
    pub fn node(&self, index: NodeIndex) -> Option<Node> {
        if index.0 >= self.len {
            return None;
        }
        let mut bytes = [0u8; ENTRY_LEN];
        self.copy(self.entries.start + index.0 * ENTRY_LEN as u64, &mut bytes)?;
        let record = EntryRecord::decode(&bytes);
        let (kind, target) = match record.kind {
            RecordKind::Dir { first, count } => (NodeKind::Dir { first, count }, 0),
            RecordKind::File { pack, offset, size } => (NodeKind::File { pack, offset, size }, 0),
            RecordKind::Symlink { offset, len } => (NodeKind::Symlink { len }, offset),
            RecordKind::Unknown(_) => return None,
        };
        Some(Node {
            index,
            parent: record.parent,
            mode: record.mode,
            kind,
            name: (record.name_offset, record.name_len),
            target,
        })
    }

    /// Copies `len` bytes at `offset` of the names into `buf`, which is at least `len` long.
    fn names_at<'b>(&self, offset: u64, len: usize, buf: &'b mut [u8]) -> Option<&'b [u8]> {
        let at = self.names.start.checked_add(offset)?;
        let end = at.checked_add(len as u64)?;
        if end > self.names.end {
            return None;
        }
        let out = buf.get_mut(..len)?;
        self.copy(at, out)?;
        Some(out)
    }

    /// The name of `node`, copied into `buf` (empty for the root).
    #[must_use]
    pub fn name<'b>(&self, node: &Node, buf: &'b mut [u8; NAME_MAX]) -> Option<&'b [u8]> {
        self.names_at(u64::from(node.name.0), usize::from(node.name.1), buf)
    }

    /// The target of link `node`; `None` if it is not a link.
    #[must_use]
    pub fn read_link(&self, node: &Node) -> Option<Vec<u8>> {
        let NodeKind::Symlink { len } = node.kind else {
            return None;
        };
        // Checked against the names before the target's buffer is allocated: a record changed
        // after the open must not name an allocation.
        if self
            .names
            .start
            .checked_add(node.target)?
            .checked_add(len)?
            > self.names.end
        {
            return None;
        }
        let len = usize::try_from(len).ok()?;
        let mut target = vec![0u8; len];
        self.names_at(node.target, len, &mut target)?;
        Some(target)
    }

    /// The child of directory `dir` named `name`: a binary search of its children's records,
    /// which are sorted by name bytes. `None` if `dir` is not a directory or has no such child.
    #[must_use]
    pub fn lookup(&self, dir: NodeIndex, name: &[u8]) -> Option<NodeIndex> {
        let NodeKind::Dir { first, count } = self.node(dir)?.kind else {
            return None;
        };
        let mut buf = [0u8; NAME_MAX];
        let (mut low, mut high) = (0u64, count);
        while low < high {
            let mid = low + (high - low) / 2;
            let child = self.node(NodeIndex(first.checked_add(mid)?))?;
            match self.name(&child, &mut buf)?.cmp(name) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Some(child.index),
            }
        }
        None
    }
}

/// `n` bytes of the image as a length in memory (a `u64` fits a `usize` on the 64-bit targets
/// tierstore runs on).
fn bytes_len(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
