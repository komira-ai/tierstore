//! The byte layout of an L2 tree list: the header, the sections and the fixed records. The
//! reference is `docs/tree-list-l2.md` of this crate.

/// The first eight bytes of every L2 tree list. A version 2 list begins `TSTREE\0\0`, a
/// version 1 list `{`.
pub const MAGIC: [u8; 8] = *b"TSL2TREE";

/// The magic of the version 2 list an L2 list is made from (`TSTREE\0\0`).
pub(crate) const V2_MAGIC: [u8; 8] = *b"TSTREE\0\0";

/// The major version this crate reads and writes; a reader refuses any other.
pub const MAJOR: u16 = 1;

/// The length of the header this crate writes, and the least a reader accepts.
pub const HEADER_LEN: usize = 192;

/// `log2` of the cell size: every cell is 1 MiB, the last of a pack may be short. A reader
/// accepts no other (design section 2).
pub const CELL_SHIFT: u8 = 20;

/// The bytes of a cell.
pub const CELL_BYTES: u64 = 1 << CELL_SHIFT;

/// The longest key of a pack, in bytes, as in a version 2 list.
pub const MAX_KEY_BYTES: usize = 1024;

/// The longest path of an entry, in bytes, as in a version 2 list.
pub const MAX_PATH_BYTES: usize = 4096;

/// The longest target of a link, in bytes, as in a version 2 list.
pub const MAX_TARGET_BYTES: usize = 4096;

/// The bit of an extension record's tag that says a reader that does not know the tag must refuse
/// the list.
pub const EXTENSION_REQUIRED: u32 = 1 << 31;

/// The prefix of the key `pack_root` hashes ahead of a pack's size, shift and digests (3.1).
pub(crate) const PACK_ROOT_PREFIX: &[u8; 7] = b"TSPACK1";

/// The length of an entry record, and of a digest.
pub(crate) const RECORD_LEN: usize = 32;

/// The length of a PACKS record.
pub(crate) const PACK_LEN: usize = 48;

/// The length of a range record.
pub(crate) const RANGE_LEN: usize = 24;

/// The length of a version 2 header.
pub(crate) const V2_HEADER_LEN: usize = 64;

/// Where the CRC-32C of the header is.
pub(crate) const CRC_AT: usize = 188;

/// Where the section table begins.
pub(crate) const SECTIONS_AT: usize = 56;

/// The kind of a file, in the top four bits of an entry's `kind_mode`.
pub(crate) const KIND_FILE: u8 = 0;
/// The kind of a directory.
pub(crate) const KIND_DIR: u8 = 1;
/// The kind of a symbolic link.
pub(crate) const KIND_SYMLINK: u8 = 2;

/// The SHA-256 of no bytes: the digest of every file of size 0.
pub(crate) const EMPTY_SHA256: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// A section of an L2 tree list, in the order of the header and of the object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// The entry records, 32 bytes each: the version 2 records, a file's key number being its
    /// pack's number.
    Entries,
    /// The SHA-256 of each entry's bytes (zero for a directory or a link).
    Digests,
    /// The PACKS records, 48 bytes each.
    Packs,
    /// The CELLS table: one 32-byte digest per cell, pack by pack.
    Cells,
    /// The bytes of the pack keys, back to back in the order of the packs.
    Keys,
    /// The bytes of the names and the link targets.
    Names,
    /// The range records, 24 bytes each.
    Ranges,
    /// The extension records.
    Extensions,
}

impl Section {
    /// Every section, in order.
    pub(crate) const ALL: [Self; 8] = [
        Self::Entries,
        Self::Digests,
        Self::Packs,
        Self::Cells,
        Self::Keys,
        Self::Names,
        Self::Ranges,
        Self::Extensions,
    ];

    /// The bytes of one unit of the section's count: a record's length, or 1 for a blob.
    pub(crate) const fn unit(self) -> u64 {
        match self {
            Self::Entries | Self::Digests | Self::Cells => RECORD_LEN as u64,
            Self::Packs => PACK_LEN as u64,
            Self::Ranges => RANGE_LEN as u64,
            Self::Keys | Self::Names | Self::Extensions => 1,
        }
    }
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Entries => "entry",
            Self::Digests => "sha256",
            Self::Packs => "pack",
            Self::Cells => "cell",
            Self::Keys => "key",
            Self::Names => "name",
            Self::Ranges => "range",
            Self::Extensions => "extension",
        })
    }
}

/// The little-endian `u16` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The little-endian `u32` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The little-endian `u64` at `at` of `bytes`, which the caller has bounded.
pub(crate) fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut eight = [0; 8];
    eight.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(eight)
}

/// The 32 bytes at `at` of `bytes`, which the caller has bounded.
pub(crate) fn digest_at(bytes: &[u8], at: usize) -> [u8; 32] {
    let mut digest = [0; 32];
    digest.copy_from_slice(&bytes[at..at + 32]);
    digest
}

/// `at` rounded up to a multiple of 8.
pub(crate) const fn align8(at: usize) -> usize {
    at.div_ceil(8) * 8
}

/// The number of cells of a pack of `size` bytes.
pub(crate) const fn cells_of(size: u64) -> u64 {
    size.div_ceil(CELL_BYTES)
}

/// The CRC-32C of a header: its first `len` bytes with the four bytes of the CRC as zero.
pub(crate) fn header_crc(header: &[u8]) -> u32 {
    let crc = crc32c::crc32c(&header[..CRC_AT]);
    let crc = crc32c::crc32c_append(crc, &[0; 4]);
    crc32c::crc32c_append(crc, &header[CRC_AT + 4..])
}

/// The fields of one 32-byte entry record.
#[derive(Clone, Copy)]
pub(crate) struct Record {
    pub(crate) parent: u32,
    pub(crate) name_off: u32,
    /// A file's pack number, a directory's first child.
    pub(crate) x: u32,
    pub(crate) kind: u8,
    pub(crate) mode: u16,
    pub(crate) name_len: u8,
    /// A file's offset, a directory's child count, a link target's offset.
    pub(crate) a: u64,
    /// A file's size, a link target's length.
    pub(crate) b: u64,
}

impl Record {
    /// The record whose 32 bytes are `bytes`.
    pub(crate) fn of(bytes: &[u8]) -> Self {
        let kind_mode = le_u16(bytes, 12);
        Self {
            parent: le_u32(bytes, 0),
            name_off: le_u32(bytes, 4),
            x: le_u32(bytes, 8),
            kind: (kind_mode >> 12) as u8,
            mode: kind_mode & 0o7777,
            name_len: bytes[14],
            a: le_u64(bytes, 16),
            b: le_u64(bytes, 24),
        }
    }
}
