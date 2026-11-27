//! [`TreeListL2`]: an L2 tree list over its bytes, checked once at open and then read in place.

use std::ops::Range;
use std::sync::OnceLock;

use ring::digest::{Context, SHA256};
use tierstore_fetch_api::ids::{PackKey, PackRoot};
use tierstore_fetch_api::list::{
    CellDigest, ListL2, ListVersionUnsupported, NodeIndex, PackRecord, Refusal,
};

use crate::error::{Malformed, OpenError};
use crate::layout::{
    CELL_SHIFT, KIND_DIR, KIND_FILE, PACK_LEN, PACK_ROOT_PREFIX, RANGE_LEN, RECORD_LEN, Record,
    Section, V2_HEADER_LEN, V2_MAGIC, align8, digest_at, le_u32, le_u64,
};
use crate::open::{self, Header};
use crate::walk::{self, Shape};

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind<'a> {
    /// A directory: its children are the entries `first..first + count`, sorted by name.
    Dir {
        /// Its first child.
        first: u64,
        /// How many children it has.
        count: u64,
    },
    /// A file: its bytes are `size` bytes at `offset` of pack `pack` (none for a file of size 0).
    File {
        /// The number of its pack in PACKS, for a file of size above 0.
        pack: Option<u32>,
        /// Where its bytes begin in the pack (0 for a file of size 0).
        offset: u64,
        /// Its size.
        size: u64,
    },
    /// A symbolic link to `target`, never followed.
    Symlink {
        /// What the link says.
        target: &'a [u8],
    },
}

/// An entry of the tree, read from its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node<'a> {
    /// The directory it is in (the root's parent is the root).
    pub parent: NodeIndex,
    /// Its name (empty for the root).
    pub name: &'a [u8],
    /// The permission bits with setuid, setgid and sticky (0 for a link).
    pub mode: u16,
    /// What it is.
    pub kind: NodeKind<'a>,
}

/// A range of a pack the writer says the tree reads: the files' bytes, merged across small gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackRange {
    /// The number of the pack in PACKS.
    pub pack: u32,
    /// The first byte.
    pub offset: u64,
    /// The number of bytes, above 0.
    pub len: u64,
}

/// An L2 tree list over its bytes `B` (a `Vec<u8>`, an `Arc<[u8]>`, a mapping): every section
/// checked once by [`open`](Self::open), then every read indexes into the bytes. An index outside
/// the list reads as `None`.
pub struct TreeListL2<B> {
    bytes: B,
    header: Header,
    shape: Shape,
    /// `pack_root` of each pack, computed when first asked (3.1).
    roots: Box<[OnceLock<[u8; 32]>]>,
}

impl<B> std::fmt::Debug for TreeListL2<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeListL2")
            .field("tree_id", &crate::error::hex(&self.header.tree_id))
            .field("shape", &self.shape)
            .finish_non_exhaustive()
    }
}

impl<B: AsRef<[u8]>> TreeListL2<B> {
    /// Opens the list in `bytes`: checks the magic, the version, the header's length and CRC-32C,
    /// its flags, digest function and cell shift; that every section is aligned, after the one
    /// before and within the object; every PACKS record and key, that CELLS holds exactly the
    /// packs' cells, every range and extension record; then walks the entries once, checking each
    /// by the rules of a version 2 list and each file's bytes to lie within its pack.
    ///
    /// It does not check the tree id: [`open_sidecar`](Self::open_sidecar) does, or a caller that
    /// hashes beside other work calls [`check_tree_id`](Self::check_tree_id).
    pub fn open(bytes: B) -> Result<Self, OpenError> {
        let all = bytes.as_ref();
        let header = open::header(all)?;
        open::packs(all, &header)?;
        open::ranges(all, &header)?;
        open::extensions(all, &header)?;
        let shape = walk::walk(all, &header)?;
        let roots = (0..header.place(Section::Packs).count)
            .map(|_| OnceLock::new())
            .collect();
        Ok(Self {
            bytes,
            header,
            shape,
            roots,
        })
    }

    /// Opens the sidecar of tree `tree_id` (the digest of the key `cas/l/<id>` a job names), as
    /// fetched: `None` if the store has none. Refuses a missing sidecar, a version 1 or 2 list and
    /// an unknown version or digest function with [`ListVersionUnsupported`]; then
    /// [`open`](Self::open)s it, and refuses a list of another tree and one whose version 2
    /// projection does not hash to its tree id ([`check_tree_id`](Self::check_tree_id)).
    pub fn open_sidecar(fetched: Option<B>, tree_id: &[u8; 32]) -> Result<Self, OpenError> {
        let bytes = fetched.ok_or(ListVersionUnsupported {
            reason: Refusal::MissingSidecar,
        })?;
        let list = Self::open(bytes)?;
        if list.header.tree_id != *tree_id {
            return Err(OpenError::Malformed(Malformed::WrongTree {
                expected: *tree_id,
                found: list.header.tree_id,
            }));
        }
        list.check_tree_id()?;
        Ok(list)
    }

    fn all(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    fn section(&self, section: Section) -> &[u8] {
        self.header.place(section).of(self.all())
    }

    /// The tree id the header names: the SHA-256 of the version 2 list the tree is named by.
    #[must_use]
    pub const fn tree_id(&self) -> &[u8; 32] {
        &self.header.tree_id
    }

    /// What the walk at open learnt of the tree.
    #[must_use]
    pub const fn shape(&self) -> Shape {
        self.shape
    }

    /// The number of entries, the root included.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.header.places[Section::Entries as usize].count
    }

    /// Whether the tree has nothing but the root.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 1
    }

    /// The bytes of the list.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.all()
    }

    /// Where `section` is in the list, in bytes from its start: checked by the open to lie within
    /// the list and to begin at a multiple of 8. A reader that maps the list reads records there
    /// (an entry's at `start + index * ENTRY_LEN`, a name's at `start + name_offset` of the
    /// names), and advises the kernel of the sections it reads most.
    #[must_use]
    pub fn section_range(&self, section: Section) -> Range<u64> {
        let place = self.header.place(section);
        place.start as u64..(place.start + place.len) as u64
    }

    fn record(&self, node: NodeIndex) -> Option<Record> {
        let at = usize::try_from(node.0).ok()?.checked_mul(RECORD_LEN)?;
        let entries = self.section(Section::Entries);
        entries.get(at..at + RECORD_LEN).map(Record::of)
    }

    fn name_of(&self, record: Record) -> &[u8] {
        let start = record.name_off as usize;
        // The walk checked every name to be within the names.
        &self.section(Section::Names)[start..start + usize::from(record.name_len)]
    }

    /// Entry `node`.
    #[must_use]
    pub fn node(&self, node: NodeIndex) -> Option<Node<'_>> {
        let record = self.record(node)?;
        let kind = match record.kind {
            KIND_DIR => NodeKind::Dir {
                first: u64::from(record.x),
                count: record.a,
            },
            KIND_FILE => NodeKind::File {
                pack: (record.b > 0).then_some(record.x),
                offset: if record.b > 0 { record.a } else { 0 },
                size: record.b,
            },
            // The walk checked every kind, and every link's target to be within the names.
            _ => NodeKind::Symlink {
                target: &self.section(Section::Names)
                    [record.a as usize..(record.a + record.b) as usize],
            },
        };
        Some(Node {
            parent: NodeIndex(u64::from(record.parent)),
            name: self.name_of(record),
            mode: record.mode,
            kind,
        })
    }

    /// The children of directory `dir`, in name order; `None` if `dir` is not a directory.
    #[must_use]
    pub fn children(&self, dir: NodeIndex) -> Option<Range<u64>> {
        let record = self.record(dir)?;
        (record.kind == KIND_DIR).then(|| u64::from(record.x)..u64::from(record.x) + record.a)
    }

    /// The SHA-256 of file `file`'s bytes; `None` if it is not a file.
    #[must_use]
    pub fn file_sha256(&self, file: NodeIndex) -> Option<[u8; 32]> {
        let record = self.record(file)?;
        (record.kind == KIND_FILE)
            .then(|| digest_at(self.section(Section::Digests), file.0 as usize * RECORD_LEN))
    }

    /// The key of pack `pack`.
    #[must_use]
    pub fn pack_key(&self, pack: u32) -> Option<&str> {
        let record = self.pack_bytes(pack)?;
        let start = le_u32(record, 0) as usize;
        let len = le_u32(record, 4) as usize;
        // The open checked every key to be UTF-8 within the keys.
        std::str::from_utf8(&self.section(Section::Keys)[start..start + len]).ok()
    }

    fn pack_bytes(&self, pack: u32) -> Option<&[u8]> {
        let at = pack as usize * PACK_LEN;
        self.section(Section::Packs).get(at..at + PACK_LEN)
    }

    /// `upload_part_size` of pack `pack`: kept in the record for lists of packs uploaded in parts,
    /// read by no fill (design 3.1).
    #[must_use]
    pub fn upload_part_size(&self, pack: u32) -> Option<u64> {
        self.pack_bytes(pack).map(|record| le_u64(record, 32))
    }

    /// The ranges of the packs the writer says the tree reads, in the order of the list: a hint
    /// of what to read, which the open checked to lie within their packs.
    pub fn ranges(&self) -> impl Iterator<Item = PackRange> + '_ {
        self.section(Section::Ranges)
            .chunks_exact(RANGE_LEN)
            .map(|range| PackRange {
                pack: le_u32(range, 0),
                offset: le_u64(range, 8),
                len: le_u64(range, 16),
            })
    }

    /// The version 2 list this list holds, byte for byte: its header as the version 2 writer
    /// writes it (major 1, minor 0, no flags), then the entries, the digests, the key offsets made
    /// from the packs' keys, the keys, the names and the ranges, each at the next multiple of 8 with
    /// zeros between them. Its SHA-256 is [`projection_digest`](Self::projection_digest): for a
    /// list [`open_sidecar`](Self::open_sidecar) opened, the tree id, so these are the bytes of
    /// the list `cas/l/<id>` the tree is named by. The bytes are a new allocation of the list's
    /// size, beside the L2 list's own.
    #[must_use]
    pub fn projection(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.project(|piece| bytes.extend_from_slice(piece));
        bytes
    }

    /// The SHA-256 of the version 2 list this list holds ([`projection`](Self::projection)),
    /// hashed piece by piece with no copy of the list. It is the SHA-256 of the version 2 list the
    /// L2 list was made from, the digest of the key the tree is named by.
    #[must_use]
    pub fn projection_digest(&self) -> [u8; 32] {
        let mut context = Context::new(&SHA256);
        self.project(|piece| context.update(piece));
        let mut digest = [0; 32];
        digest.copy_from_slice(context.finish().as_ref());
        digest
    }

    /// Gives `put` the bytes of the version 2 list this list holds, in order, a piece at a time
    /// ([`projection`](Self::projection)).
    fn project(&self, mut put: impl FnMut(&[u8])) {
        let packs = self.header.place(Section::Packs).count;
        let mut offsets = Vec::with_capacity((packs as usize + 1) * 4);
        offsets.extend_from_slice(&0u32.to_le_bytes());
        for record in self.section(Section::Packs).chunks_exact(PACK_LEN) {
            let end = le_u32(record, 0).wrapping_add(le_u32(record, 4));
            offsets.extend_from_slice(&end.to_le_bytes());
        }
        let place = |section| self.header.place(section);
        let sections: [(&[u8], u32); 6] = [
            (
                self.section(Section::Entries),
                place(Section::Entries).count,
            ),
            (
                self.section(Section::Digests),
                place(Section::Digests).count,
            ),
            (&offsets, packs),
            (self.section(Section::Keys), place(Section::Keys).count),
            (self.section(Section::Names), place(Section::Names).count),
            (self.section(Section::Ranges), place(Section::Ranges).count),
        ];
        let mut header = Vec::with_capacity(V2_HEADER_LEN);
        header.extend_from_slice(&V2_MAGIC);
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        let mut at = V2_HEADER_LEN;
        let mut places = [0usize; 6];
        for (place, (bytes, count)) in places.iter_mut().zip(&sections) {
            *place = align8(at);
            at = *place + bytes.len();
            // A version 2 list is at most 256 MiB: an offset past 32 bits is no version 2 list,
            // and its wrapped value makes a digest no key names.
            header.extend_from_slice(&(*place as u32).to_le_bytes());
            header.extend_from_slice(&count.to_le_bytes());
        }
        put(&header);
        let mut at = V2_HEADER_LEN;
        for (place, (bytes, _)) in places.iter().zip(&sections) {
            put(&[0u8; 8][..place - at]);
            put(bytes);
            at = place + bytes.len();
        }
    }

    /// Checks that the version 2 list this list holds hashes to its tree id
    /// ([`projection_digest`](Self::projection_digest)): every entry, name, file digest, pack key,
    /// offset and range is then the one the tree id names. The packs' sizes and flags and the cell
    /// digests are not covered by the id (design 3.4).
    pub fn check_tree_id(&self) -> Result<(), Malformed> {
        let found = self.projection_digest();
        if found == self.header.tree_id {
            Ok(())
        } else {
            Err(Malformed::Projection { found })
        }
    }

    /// The cell digests of pack `pack`.
    fn pack_cells(&self, pack: u32) -> Option<&[u8]> {
        let record = self.pack_bytes(pack)?;
        // The open checked every pack's cells to lie within CELLS.
        let first = le_u64(record, 16) as usize;
        let count = le_u32(record, 24) as usize;
        Some(&self.section(Section::Cells)[first * RECORD_LEN..(first + count) * RECORD_LEN])
    }
}

/// `pack_root` of a pack of `size` bytes whose cell digests are `cells` (3.1):
/// `sha256("TSPACK1" || size_le64 || shift_u8 || digest[0] || ... || digest[n-1])`.
#[must_use]
pub fn pack_root(size: u64, cells: &[u8]) -> [u8; 32] {
    let mut context = Context::new(&SHA256);
    context.update(PACK_ROOT_PREFIX);
    context.update(&size.to_le_bytes());
    context.update(&[CELL_SHIFT]);
    context.update(cells);
    let mut root = [0; 32];
    root.copy_from_slice(context.finish().as_ref());
    root
}

impl<B: AsRef<[u8]> + Send + Sync> ListL2 for TreeListL2<B> {
    fn pack_count(&self) -> u32 {
        self.header.place(Section::Packs).count
    }

    fn pack_record(&self, pack: u32) -> Option<PackRecord> {
        let record = self.pack_bytes(pack)?;
        Some(PackRecord {
            key: PackKey(self.pack_key(pack)?.to_owned()),
            pack_size: le_u64(record, 8),
            cell_first: le_u64(record, 16),
            cell_count: le_u32(record, 24),
            cell_shift: record[28],
            pack_flags: record[29],
        })
    }

    fn cell_digest(&self, pack: u32, cell: u32) -> Option<CellDigest> {
        let cells = self.pack_cells(pack)?;
        let at = cell as usize * RECORD_LEN;
        cells.get(at..at + RECORD_LEN).map(|_| digest_at(cells, at))
    }

    fn pack_root(&self, pack: u32) -> Option<PackRoot> {
        let cells = self.pack_cells(pack)?;
        let size = le_u64(self.pack_bytes(pack)?, 8);
        let root = self.roots[pack as usize].get_or_init(|| pack_root(size, cells));
        Some(PackRoot::Digest(*root))
    }

    fn dir_lookup(&self, dir: NodeIndex, name: &[u8]) -> Option<NodeIndex> {
        let children = self.children(dir)?;
        let entries = self.section(Section::Entries);
        // The walk checked the children of every directory to be within the entries, sorted.
        let (first, count) = (
            children.start as usize,
            (children.end - children.start) as usize,
        );
        let records = &entries[first * RECORD_LEN..(first + count) * RECORD_LEN];
        let mut low = 0usize;
        let mut high = count;
        while low < high {
            let mid = low + (high - low) / 2;
            let record = Record::of(&records[mid * RECORD_LEN..(mid + 1) * RECORD_LEN]);
            match self.name_of(record).cmp(name) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Some(NodeIndex((first + mid) as u64)),
            }
        }
        None
    }
}
