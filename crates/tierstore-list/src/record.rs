//! [`EntryRecord`]: one entry record decoded from its own 32 bytes, for a reader that holds the
//! list in a mapping it reads by copying (a volatile mapping) rather than as one slice: it copies a
//! record out by [`TreeListL2::section_range`](crate::TreeListL2::section_range) and decodes it
//! here, by the same rules as [`TreeListL2::node`](crate::TreeListL2::node).

use tierstore_fetch_api::list::NodeIndex;

use crate::layout::{KIND_DIR, KIND_FILE, KIND_SYMLINK, RECORD_LEN, Record};

/// The bytes of one entry record.
pub const ENTRY_LEN: usize = RECORD_LEN;

/// What an entry is, by its record: the offsets of a link's target are in the names, as for an
/// entry's name, so a reader copies them from there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
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
    /// A symbolic link: its target is `len` bytes at `offset` of the names.
    Symlink {
        /// Where the target begins in the names.
        offset: u64,
        /// The target's length.
        len: u64,
    },
    /// A kind this release does not know: [`TreeListL2::open`](crate::TreeListL2::open) refuses a
    /// list with one, so only bytes changed after the open read as it.
    Unknown(u8),
}

/// One entry record, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryRecord {
    /// The directory it is in (the root's parent is the root).
    pub parent: NodeIndex,
    /// Where its name begins in the names.
    pub name_offset: u32,
    /// Its name's length (0 for the root).
    pub name_len: u8,
    /// The permission bits with setuid, setgid and sticky (0 for a link).
    pub mode: u16,
    /// What it is.
    pub kind: RecordKind,
}

impl EntryRecord {
    /// The record whose 32 bytes are `bytes`.
    #[must_use]
    pub fn decode(bytes: &[u8; ENTRY_LEN]) -> Self {
        let record = Record::of(bytes);
        let kind = match record.kind {
            KIND_DIR => RecordKind::Dir {
                first: u64::from(record.x),
                count: record.a,
            },
            KIND_FILE => RecordKind::File {
                pack: (record.b > 0).then_some(record.x),
                offset: if record.b > 0 { record.a } else { 0 },
                size: record.b,
            },
            KIND_SYMLINK => RecordKind::Symlink {
                offset: record.a,
                len: record.b,
            },
            other => RecordKind::Unknown(other),
        };
        Self {
            parent: NodeIndex(u64::from(record.parent)),
            name_offset: record.name_off,
            name_len: record.name_len,
            mode: record.mode,
            kind,
        }
    }
}
