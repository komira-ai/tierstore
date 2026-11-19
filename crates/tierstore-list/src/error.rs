//! Why a list is refused, and why a sidecar cannot be written.

use tierstore_fetch_api::list::ListVersionUnsupported;

use crate::layout::Section;

/// Why a list was not opened: a list this release does not read ([`ListVersionUnsupported`], the
/// distinct status the CAS service takes its miss path on, design 3.5), or bytes that are not an L2
/// list ([`Malformed`]).
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// A version 1 or 2 list, a missing sidecar, an unknown major version or an unknown digest
    /// function.
    #[error(transparent)]
    Unsupported(#[from] ListVersionUnsupported),
    /// Bytes that are not what the format says.
    #[error("tree list L2: {0}")]
    Malformed(#[from] Malformed),
}

/// What is wrong with bytes that claim to be an L2 tree list. A list from a later release that
/// sets a required flag or a required extension this reader does not know is refused here too:
/// it cannot be read correctly, and is written only after every reader knows it.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    /// The object does not begin with the L2 magic (nor with a version 1 or 2 list's).
    #[error("bad magic")]
    BadMagic,
    /// The header, or where it says a section is.
    #[error("header: {0}")]
    Header(HeaderFault),
    /// A PACKS record, numbered from 0.
    #[error("pack {index}: {fault}")]
    Pack {
        /// The number of the pack.
        index: u32,
        /// What is wrong with it.
        fault: PackFault,
    },
    /// The last key does not end at the end of the keys.
    #[error("the keys do not end where the last pack's key ends")]
    KeysLength,
    /// The CELLS table does not hold exactly the cells of the packs.
    #[error("{found} cells for packs of {expected}")]
    CellTotal {
        /// The cells the packs have together.
        expected: u64,
        /// The count of the CELLS section.
        found: u64,
    },
    /// A range that names no pack, is empty, or ends past its pack, numbered from 0.
    #[error("range {0}: no such pack, empty, or past the end of its pack")]
    Range(u32),
    /// The extension records.
    #[error("extension at byte {at}: {fault}")]
    Extension {
        /// The offset of the record within the extension section.
        at: u64,
        /// What is wrong with it.
        fault: ExtensionFault,
    },
    /// An entry, numbered from 0 (the root).
    #[error("entry {index}: {fault}")]
    Entry {
        /// The number of the entry.
        index: u32,
        /// What is wrong with it.
        fault: EntryFault,
    },
    /// Entries from this number on are in no directory's children.
    #[error("entries from {0} on are in no directory")]
    Orphans(u32),
    /// The header names another tree than the one asked for.
    #[error("the list is of tree {}, not {}", hex(found), hex(expected))]
    WrongTree {
        /// The tree id asked for (the digest of the key `cas/l/<id>`).
        expected: [u8; 32],
        /// The tree id of the header.
        found: [u8; 32],
    },
    /// The version 2 list the L2 list holds does not hash to its tree id: the entries, names, keys
    /// or ranges are not those of the tree the id names.
    #[error(
        "the version 2 list it holds hashes to {}, not to its tree id",
        hex(found)
    )]
    Projection {
        /// The SHA-256 of the version 2 projection.
        found: [u8; 32],
    },
}

/// What is wrong with the header of an L2 list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderFault {
    /// The object is shorter than a header.
    #[error("truncated")]
    Truncated,
    /// A header length below 192, not a multiple of 8, or past the end of the object.
    #[error("bad header length {0}")]
    Length(u32),
    /// The CRC-32C of the header is not the one it holds.
    #[error("crc32c {computed:#010x}, the header says {stored:#010x}")]
    Crc {
        /// The CRC the header holds.
        stored: u32,
        /// The CRC of its bytes.
        computed: u32,
    },
    /// A required flag this reader does not know.
    #[error("unknown required flags {0:#x}")]
    RequiredFlags(u32),
    /// A default cell shift other than 20.
    #[error("default cell shift {0}, not 20")]
    CellShift(u8),
    /// A section whose offset is not a multiple of 8.
    #[error("the {0} section is not 8-byte aligned")]
    Misaligned(Section),
    /// A section that begins before the end of the header or of the one before it.
    #[error("the {0} section overlaps the one before it")]
    Overlap(Section),
    /// A section that ends past the end of the object.
    #[error("the {0} section is past the end")]
    PastEnd(Section),
    /// A section whose count does not fit the 32 bits its records number it with.
    #[error("the {0} section has more than 2^32 - 1 records or bytes")]
    TooMany(Section),
    /// An entry count of 0: there is always the root.
    #[error("no entries")]
    NoEntries,
    /// A digest table whose count is not the entry count.
    #[error("{0} digests for another count of entries")]
    DigestCount(u64),
    /// An extension section whose length is not a multiple of 8.
    #[error("an extension section of {0} bytes, not a multiple of 8")]
    ExtensionLength(u64),
}

/// What is wrong with one PACKS record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PackFault {
    /// Its key does not begin where the key before it ends (the first at 0).
    #[error("its key does not begin where the one before it ends")]
    KeyStart,
    /// A key of no bytes, or over 1024.
    #[error("a key of {0} bytes, not 1 to 1024")]
    KeyLength(u32),
    /// A key past the end of the keys.
    #[error("its key is past the end of the keys")]
    KeyBounds,
    /// A key that is not UTF-8.
    #[error("its key is not UTF-8")]
    KeyNotUtf8,
    /// A key that does not sort after the one before it, bytewise.
    #[error("its key does not sort after the one before it")]
    KeyNotAscending,
    /// A pack of size 0.
    #[error("size 0")]
    Empty,
    /// A cell shift other than 20.
    #[error("cell shift {0}, not 20")]
    CellShift(u8),
    /// A cell count that is not the number of cells of its size.
    #[error("{found} cells, its size has {expected}")]
    CellCount {
        /// The cells of its size.
        expected: u64,
        /// The count it holds.
        found: u32,
    },
    /// Its first cell is not where the cells of the packs before it end.
    #[error("first cell {found}, the packs before it end at {expected}")]
    CellFirst {
        /// Where the cells of the packs before it end.
        expected: u64,
        /// Its `cell_first`.
        found: u64,
    },
}

/// What is wrong with an extension record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExtensionFault {
    /// A record whose header or bytes run past the end of the section.
    #[error("truncated")]
    Truncated,
    /// A required tag this reader does not know.
    #[error("unknown required tag {0:#x}")]
    Required(u32),
}

/// What is wrong with one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryFault {
    /// Entry 0 is not a directory with parent 0 and no name.
    #[error("the root is not a directory with no name and parent 0")]
    Root,
    /// A kind other than 0 (file), 1 (directory) and 2 (link).
    #[error("unknown kind {0}")]
    UnknownKind(u8),
    /// A directory that no directory before it holds.
    #[error("a directory before the children of the directories before it")]
    Unreachable,
    /// A directory whose first child is not the entry after the children of the directories
    /// before it, or whose children run past the last entry.
    #[error("children not where the breadth-first order puts them")]
    Children,
    /// A child whose parent is not the directory whose children it is among.
    #[error("parent {0} is not the directory that holds it")]
    Parent(u32),
    /// A name past the end of the names.
    #[error("name past the end of the names")]
    NameBounds,
    /// A name that is empty, `.`, `..`, not UTF-8, or has a `/` or a NUL.
    #[error("bad name")]
    BadName,
    /// A name that does not sort after its sibling's before it, bytewise.
    #[error("name not after its sibling's")]
    NotAscending,
    /// A path over 4096 bytes.
    #[error("path over 4096 bytes")]
    PathTooLong,
    /// A file of size above 0 whose pack number is not in PACKS.
    #[error("no pack {0}")]
    Pack(u32),
    /// A file whose bytes end past the end of its pack.
    #[error("bytes past the end of its pack")]
    OutsidePack,
    /// A file of size 0 whose digest is not the SHA-256 of no bytes.
    #[error("a file of size 0 has the digest of no bytes")]
    EmptyDigest,
    /// A link target that is empty, over 4096 bytes, past the end of the names, not UTF-8, or
    /// has a NUL.
    #[error("bad target")]
    Target,
}

/// Why a sidecar could not be made of a version 2 list.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    /// The version 2 list is not one this writer reads.
    #[error("version 2 list: {0}")]
    V2(V2Fault),
    /// The caller has no cells of a pack the list names.
    #[error("no cells of pack {0:?}")]
    UnknownPack(String),
    /// A pack of size 0.
    #[error("pack {0:?} has size 0")]
    EmptyPack(String),
    /// Cells that are not those of the pack's size.
    #[error("pack {key:?}: {found} cell digests, its size has {expected}")]
    CellCount {
        /// The pack's key.
        key: String,
        /// The cells of its size.
        expected: u64,
        /// The digests given.
        found: u64,
    },
    /// A file or range whose bytes end past the end of its pack.
    #[error("pack {0:?}: a file or range past its end")]
    OutsidePack(String),
}

/// What is wrong with a version 2 list a sidecar is made of. The writer copies its sections, so it
/// takes only a list whose bytes are exactly what the version 2 writer writes; the L2 reader checks
/// the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum V2Fault {
    /// Not the version 2 magic, or shorter than its header.
    #[error("not a version 2 list")]
    NotV2,
    /// A major version other than 1, a minor other than 0 or a flag set: a list of another writer.
    #[error("not major 1, minor 0, no flags")]
    Version,
    /// A section not at the next multiple of 8 after the one before, a byte between sections that
    /// is not zero, a section past the end, or bytes after the last section.
    #[error("sections not where the version 2 writer puts them")]
    Layout,
    /// An entry count of 0, or a digest count other than the entry count.
    #[error("bad entry or digest count")]
    Counts,
    /// Keys that are not 1 to 1024 bytes of UTF-8, back to back, sorted and each once.
    #[error("bad keys")]
    Keys,
    /// A file of size above 0 whose key number is not in the keys, or a range that names no key.
    #[error("a key number past the keys")]
    KeyNumber,
    /// A range of no bytes.
    #[error("an empty range")]
    EmptyRange,
}

/// `bytes` as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
