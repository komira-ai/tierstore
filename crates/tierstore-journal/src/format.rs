//! The on-disk formats of an entry's journal (`<hash>.jnl`) and snapshot (`<hash>.meta`)
//! (section 8). Every multi-byte integer is little-endian; every header, record and snapshot
//! carries a crc32c, so a torn or flipped byte is detected, never read as another answer.
//!
//! **Journal.** A 48-byte header, then 16-byte records appended in order:
//!
//! | Bytes | Header field |
//! |---|---|
//! | 0..8 | magic `TSJOURNL` |
//! | 8..10 | version, 1 |
//! | 10..12 | flags: bit 0 = the boot id is known; other bits written 0, ignored when read |
//! | 12..16 | reserved, written 0, ignored when read |
//! | 16..24 | epoch: the snapshot epoch this journal follows |
//! | 24..40 | boot id of the boot that started the journal (zeros when unknown) |
//! | 40..44 | reserved, written 0, ignored when read |
//! | 44..48 | crc32c of bytes 0..44 |
//!
//! | Bytes | Record field |
//! |---|---|
//! | 0 | kind: 1 = `ADD`, 2 = `PUNCH` |
//! | 1..4 | reserved, written 0, ignored when read |
//! | 4..8 | first cell of the run |
//! | 8..12 | cell count of the run, at least 1 |
//! | 12..16 | crc32c of the journal's epoch (8 bytes) followed by bytes 0..12 |
//!
//! A record's crc is seeded with the epoch, so a record left over from an earlier epoch (a
//! truncate that did not reach the disk before a power loss) fails its crc under the new header
//! and ends the replay like a torn append.
//!
//! **Snapshot.** One record, replaced whole by rename:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0..8 | magic `TSSNAPSH` |
//! | 8..10 | version, 1 |
//! | 10..12 | flags: bit 0 = the boot id is known; other bits written 0, ignored when read |
//! | 12..16 | `ext_len`: bytes of the extension area, written 0, skipped when read |
//! | 16..24 | epoch |
//! | 24..40 | boot id (zeros when unknown) |
//! | 40..48 | pack size in bytes |
//! | 48..56 | last use, milliseconds of the injected clock |
//! | 56..60 | cell count (the size in 1 MiB cells, rounded up) |
//! | 60 | root kind: 1 = digest (`pack_root`), 2 = ETag (generation) |
//! | 61..64 | reserved, written 0, ignored when read |
//! | 64..96 | root: the 32-byte digest, or the 16-byte generation then 16 zero bytes |
//! | 96..100 | key length |
//! | 100.. | key, UTF-8 |
//! | then | `present` bitmap, one bit per cell, LSB of byte 0 first: committed and trusted in the snapshot's boot |
//! | then | `recheck` bitmap: bytes on disk that must be re-hashed before they are served |
//! | then | extension area of `ext_len` bytes |
//! | last 4 | crc32c of every byte before it |
//!
//! **Change.** Additive only: a later release may define a flag bit, a reserved byte, a record
//! kind or bytes of the snapshot's extension area, and keeps version 1. This release reads a
//! record kind it does not know as the end of what it can trust, so it discards the entry and
//! refetches (it never serves a cell a record it cannot read may have punched).

use tierstore_fetch_api::ids::{CellRun, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_format::ids::Generation;

use crate::boot::BootId;
use crate::cells::CellSet;

/// The magic of a journal header.
pub const JOURNAL_MAGIC: [u8; 8] = *b"TSJOURNL";
/// The magic of a snapshot.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"TSSNAPSH";
/// The version of both formats.
pub const VERSION: u16 = 1;
/// The bytes of a journal header.
pub const HEADER_LEN: usize = 48;
/// The bytes of a journal record.
pub const RECORD_LEN: usize = 16;
/// The flag bit that says the boot id is known.
pub const FLAG_BOOT_KNOWN: u16 = 1;
/// The bytes of a snapshot before its key.
pub const SNAPSHOT_FIXED_LEN: usize = 100;

const ROOT_DIGEST: u8 = 1;
const ROOT_ETAG: u8 = 2;

/// Why bytes are not a journal header or a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// Fewer bytes than the format needs.
    #[error("{what}: {got} bytes, at least {need} needed")]
    Short {
        /// The format.
        what: &'static str,
        /// The bytes needed.
        need: usize,
        /// The bytes given.
        got: usize,
    },
    /// The magic is another format's.
    #[error("{what}: bad magic")]
    Magic {
        /// The format.
        what: &'static str,
    },
    /// A version this release does not read.
    #[error("{what}: version {version}, this release reads {VERSION}")]
    Version {
        /// The format.
        what: &'static str,
        /// The version found.
        version: u16,
    },
    /// The crc does not match.
    #[error("{what}: crc mismatch")]
    Crc {
        /// The format.
        what: &'static str,
    },
    /// The snapshot's lengths do not add up (key, bitmaps, extension, total).
    #[error("snapshot: {0}")]
    Layout(&'static str),
    /// The snapshot's root kind is neither digest nor ETag.
    #[error("snapshot: root kind {0}")]
    RootKind(u8),
}

/// A journal header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    /// The snapshot epoch the journal follows.
    pub epoch: u64,
    /// The boot that started the journal.
    pub boot: Option<BootId>,
}

impl JournalHeader {
    /// The 48 bytes of the header.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&JOURNAL_MAGIC);
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        let (flags, boot) = boot_fields(self.boot);
        b[10..12].copy_from_slice(&flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.epoch.to_le_bytes());
        b[24..40].copy_from_slice(&boot);
        let crc = crc32c::crc32c(&b[0..44]);
        b[44..48].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// The header at the start of `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        const WHAT: &str = "journal header";
        let b = bytes.get(..HEADER_LEN).ok_or(FormatError::Short {
            what: WHAT,
            need: HEADER_LEN,
            got: bytes.len(),
        })?;
        check_magic_version(WHAT, b, &JOURNAL_MAGIC)?;
        if crc32c::crc32c(&b[0..44]) != u32_at(b, 44) {
            return Err(FormatError::Crc { what: WHAT });
        }
        Ok(Self {
            epoch: u64_at(b, 16),
            boot: boot_of(u16_at(b, 10), &b[24..40]),
        })
    }
}

/// The kind of a journal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// The run's bytes are durable and its cells `present`.
    Add,
    /// The run's bytes are released (written before the punch).
    Punch,
}

impl RecordKind {
    /// The kind's byte.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Add => 1,
            Self::Punch => 2,
        }
    }
}

/// A journal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// What happened to the run.
    pub kind: RecordKind,
    /// The run.
    pub run: CellRun,
}

impl Record {
    /// The 16 bytes of the record in a journal of `epoch`.
    #[must_use]
    pub fn encode(&self, epoch: u64) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        b[0] = self.kind.byte();
        b[4..8].copy_from_slice(&self.run.first.to_le_bytes());
        b[8..12].copy_from_slice(&self.run.count.to_le_bytes());
        let crc = record_crc(epoch, &b[0..12]);
        b[12..16].copy_from_slice(&crc.to_le_bytes());
        b
    }
}

/// What one 16-byte slot of a journal holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// A record of this release.
    Record(Record),
    /// A record whose crc matches but whose kind (the byte) this release does not know, or whose
    /// run is empty.
    Unknown(u8),
    /// Not a record of this epoch: torn, flipped or left over from an earlier epoch.
    Invalid,
}

/// What the 16 bytes `b` of a journal of `epoch` hold.
#[must_use]
pub fn decode_record(epoch: u64, b: &[u8; RECORD_LEN]) -> Slot {
    if record_crc(epoch, &b[0..12]) != u32_at(b, 12) {
        return Slot::Invalid;
    }
    let run = CellRun {
        first: u32_at(b, 4),
        count: u32_at(b, 8),
    };
    let kind = match b[0] {
        1 => RecordKind::Add,
        2 => RecordKind::Punch,
        other => return Slot::Unknown(other),
    };
    if run.count == 0 {
        return Slot::Unknown(b[0]);
    }
    Slot::Record(Record { kind, run })
}

/// A journal file read whole: its header and the records up to the first slot that is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalImage {
    /// The header, or why there is none (a missing, torn or flipped header).
    pub header: Result<JournalHeader, FormatError>,
    /// The records in order, up to the first slot that is not a record of this release.
    pub records: Vec<Record>,
    /// The kind byte of the first record this release cannot read, if the replay stopped at one.
    pub unknown: Option<u8>,
    /// The bytes of the header and the records read: where the next record goes.
    pub valid_len: u64,
    /// The bytes after `valid_len`: a torn append, or records of an earlier epoch.
    pub tail_len: u64,
}

impl JournalImage {
    /// The image of the journal `bytes`.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Self {
        let header = JournalHeader::decode(bytes);
        let mut image = Self {
            header: header.clone(),
            records: Vec::new(),
            unknown: None,
            valid_len: 0,
            tail_len: bytes.len() as u64,
        };
        let Ok(h) = header else {
            return image;
        };
        let mut at = HEADER_LEN;
        for chunk in bytes[HEADER_LEN..].chunks_exact(RECORD_LEN) {
            let slot: &[u8; RECORD_LEN] = chunk.try_into().unwrap_or(&[0; RECORD_LEN]);
            match decode_record(h.epoch, slot) {
                Slot::Record(r) => image.records.push(r),
                Slot::Unknown(kind) => {
                    image.unknown = Some(kind);
                    break;
                }
                Slot::Invalid => break,
            }
            at += RECORD_LEN;
        }
        image.valid_len = at as u64;
        image.tail_len = (bytes.len() - at) as u64;
        image
    }
}

/// An entry's snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The epoch: one more at every fold; the journal that follows carries it.
    pub epoch: u64,
    /// The boot that wrote it.
    pub boot: Option<BootId>,
    /// The pack.
    pub pack: PackId,
    /// The pack's size.
    pub size: u64,
    /// The entry's last use.
    pub last_use: u64,
    /// Cells committed and trusted in `boot`.
    pub present: CellSet,
    /// Cells whose bytes are on disk but must be re-hashed before they are served.
    pub recheck: CellSet,
}

/// The number of 1 MiB cells of a pack of `size` bytes, or `None` past `u32::MAX`.
#[must_use]
pub fn cell_count(size: u64) -> Option<u32> {
    u32::try_from(size.div_ceil(1 << CELL_SHIFT)).ok()
}

impl Snapshot {
    /// The bytes of the snapshot. The two bitmaps must be of the pack's cell count, as
    /// [`crate::EntryJournal`] makes them.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let key = self.pack.key.0.as_bytes();
        let mut b = vec![0u8; SNAPSHOT_FIXED_LEN];
        b[0..8].copy_from_slice(&SNAPSHOT_MAGIC);
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        let (flags, boot) = boot_fields(self.boot);
        b[10..12].copy_from_slice(&flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.epoch.to_le_bytes());
        b[24..40].copy_from_slice(&boot);
        b[40..48].copy_from_slice(&self.size.to_le_bytes());
        b[48..56].copy_from_slice(&self.last_use.to_le_bytes());
        b[56..60].copy_from_slice(&self.present.cell_count().to_le_bytes());
        let (kind, root) = root_fields(self.pack.root);
        b[60] = kind;
        b[64..96].copy_from_slice(&root);
        // A key is a path-like object key, far below 4 GiB.
        b[96..100].copy_from_slice(&(key.len() as u32).to_le_bytes());
        b.extend_from_slice(key);
        b.extend_from_slice(self.present.bits());
        b.extend_from_slice(self.recheck.bits());
        let crc = crc32c::crc32c(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// The snapshot `bytes` hold.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        const WHAT: &str = "snapshot";
        if bytes.len() < SNAPSHOT_FIXED_LEN + 4 {
            return Err(FormatError::Short {
                what: WHAT,
                need: SNAPSHOT_FIXED_LEN + 4,
                got: bytes.len(),
            });
        }
        check_magic_version(WHAT, bytes, &SNAPSHOT_MAGIC)?;
        let (body, crc) = bytes.split_at(bytes.len() - 4);
        if crc32c::crc32c(body) != u32_at(crc, 0) {
            return Err(FormatError::Crc { what: WHAT });
        }
        let size = u64_at(body, 40);
        let cells = u32_at(body, 56);
        if cell_count(size) != Some(cells) {
            return Err(FormatError::Layout("cell count is not the size's"));
        }
        let root = match body[60] {
            ROOT_DIGEST => PackRoot::Digest(body[64..96].try_into().unwrap_or([0; 32])),
            ROOT_ETAG => PackRoot::Etag(Generation(body[64..80].try_into().unwrap_or([0; 16]))),
            other => return Err(FormatError::RootKind(other)),
        };
        let key_len = u32_at(body, 96) as usize;
        let ext_len = u32_at(body, 12) as usize;
        let map = CellSet::bytes_for(cells);
        let need = [key_len, map, map, ext_len]
            .iter()
            .try_fold(SNAPSHOT_FIXED_LEN, |acc, n| acc.checked_add(*n));
        if need != Some(body.len()) {
            return Err(FormatError::Layout(
                "lengths do not add up to the snapshot's",
            ));
        }
        let key_end = SNAPSHOT_FIXED_LEN + key_len;
        let key = std::str::from_utf8(&body[SNAPSHOT_FIXED_LEN..key_end])
            .map_err(|_| FormatError::Layout("key is not UTF-8"))?;
        let present = CellSet::from_bits(cells, &body[key_end..key_end + map]).ok_or(
            FormatError::Layout("present bitmap sets a bit past the pack"),
        )?;
        let recheck = CellSet::from_bits(cells, &body[key_end + map..key_end + 2 * map]).ok_or(
            FormatError::Layout("recheck bitmap sets a bit past the pack"),
        )?;
        Ok(Self {
            epoch: u64_at(body, 16),
            boot: boot_of(u16_at(body, 10), &body[24..40]),
            pack: PackId {
                key: PackKey(key.to_owned()),
                root,
            },
            size,
            last_use: u64_at(body, 48),
            present,
            recheck,
        })
    }
}

fn check_magic_version(what: &'static str, b: &[u8], magic: &[u8; 8]) -> Result<(), FormatError> {
    if &b[0..8] != magic {
        return Err(FormatError::Magic { what });
    }
    let version = u16_at(b, 8);
    if version != VERSION {
        return Err(FormatError::Version { what, version });
    }
    Ok(())
}

fn record_crc(epoch: u64, body: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&epoch.to_le_bytes()), body)
}

fn boot_fields(boot: Option<BootId>) -> (u16, [u8; 16]) {
    match boot {
        Some(b) => (FLAG_BOOT_KNOWN, b.0),
        None => (0, [0; 16]),
    }
}

fn boot_of(flags: u16, bytes: &[u8]) -> Option<BootId> {
    let id: [u8; 16] = bytes.try_into().ok()?;
    (flags & FLAG_BOOT_KNOWN != 0).then_some(BootId(id))
}

fn root_fields(root: PackRoot) -> (u8, [u8; 32]) {
    match root {
        PackRoot::Digest(d) => (ROOT_DIGEST, d),
        PackRoot::Etag(g) => {
            let mut r = [0u8; 32];
            r[..16].copy_from_slice(&g.0);
            (ROOT_ETAG, r)
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}
