//! The block trailer (10.2), and the `.idx` files that hold the same bytes.
//!
//! A block is a sequence of records followed by its trailer. The trailer lists every record of
//! the block sorted by `(key, stamp, offset)`, then a crc32c per 1 MiB cell of the block's record
//! bytes, then a fixed 96-byte footer at the very end, so a reader that has only the tail of a
//! block (or of a data shard, whose bytes are the block's) finds the trailer from its end. The
//! trailer is a deterministic function of the block's bytes: [`Trailer::encode`] sorts the
//! entries, so two copies of the same bytes have byte-identical trailers.
//!
//! `seg/<block_id>.idx` and `blk/<block_id>.idx` hold exactly the trailer's bytes.
//!
//! An entry (little-endian, padded to 8 bytes): key length `u16`, flags `u16` ([`RecordFlags`];
//! a pack block's entries carry [`RecordFlags::RELOCATE`], 10.5), crc32c of the object's bytes in
//! the block `u32`, offset `u64`, length `u64`, stamp `u64`, generation (16 bytes), key bytes.
//!
//! The footer:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic `TSBT` |
//! | 4 | 2 | version (1) |
//! | 6 | 1 | block kind ([`BlockKind`]) |
//! | 7 | 1 | cell shift (20) |
//! | 8 | 8 | entry count |
//! | 16 | 8 | bytes of the entries |
//! | 24 | 8 | cell count |
//! | 32 | 8 | bytes of records the trailer covers (the trailer starts there) |
//! | 40 | 32 | blake3 of those bytes |
//! | 72 | 20 | reserved, zero |
//! | 92 | 4 | crc32c of the whole trailer before this field |

use crate::codec::{Dec, Enc, expect_crc, expect_magic, tail};
use crate::ids::Generation;
use crate::record::RecordFlags;
use crate::stamp::Stamp;
use crate::{CELL_SHIFT, FormatError, cells_for};

/// The magic of a trailer footer.
pub const TRAILER_MAGIC: u32 = u32::from_le_bytes(*b"TSBT");

/// The version of the trailer this release writes and reads.
pub const TRAILER_VERSION: u16 = 1;

/// The length of the trailer footer.
pub const TRAILER_FOOTER_LEN: usize = 96;

const WHAT: &str = "block trailer";

/// The kind of a block (10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// A sealed hot segment: small-object records in arrival order.
    Segment,
    /// A pack block assembled at destage from large objects, each record 4 KiB-aligned.
    Pack,
    /// One slice of a chain: a single record that is part of one large object.
    ChainSlice,
    /// A kind a later release writes, kept as its code (the trailer reads the same either way).
    Other(u8),
}

impl BlockKind {
    fn code(self) -> u8 {
        match self {
            Self::Segment => 1,
            Self::Pack => 2,
            Self::ChainSlice => 3,
            Self::Other(code) => code,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Segment,
            2 => Self::Pack,
            3 => Self::ChainSlice,
            _ => Self::Other(code),
        }
    }
}

/// One record of the block, as the trailer lists it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrailerEntry {
    /// The key.
    pub key: Vec<u8>,
    /// The record's stamp.
    pub stamp: Stamp,
    /// The offset of the record's header in the block.
    pub offset: u64,
    /// The bytes of the object (or slice) the record holds.
    pub length: u64,
    /// The object's generation.
    pub generation: Generation,
    /// The crc32c of the record's data.
    pub crc32c: u32,
    /// The record's flags.
    pub flags: RecordFlags,
}

/// A block's trailer, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailer {
    /// The kind of the block.
    pub kind: BlockKind,
    /// Every record of the block; [`Trailer::encode`] writes them sorted by `(key, stamp,
    /// offset)` and [`Trailer::decode`] refuses entries out of that order.
    pub entries: Vec<TrailerEntry>,
    /// The crc32c of each 1 MiB cell of the record bytes `[0, data_len)`.
    pub cell_crcs: Vec<u32>,
    /// The bytes of records the trailer covers: the trailer starts at this offset of the block.
    pub data_len: u64,
    /// The blake3 of the record bytes `[0, data_len)`.
    pub blake3: [u8; 32],
}

fn sort_key(e: &TrailerEntry) -> (&[u8], Stamp, u64) {
    (&e.key, e.stamp, e.offset)
}

impl Trailer {
    /// The trailer's bytes. The entries are sorted first, so the bytes do not depend on the order
    /// they were given in; two entries with the same `(key, stamp, offset)` are refused, as
    /// [`Trailer::decode`] refuses them. The cell table must have one crc per cell of `data_len`.
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        if self.cell_crcs.len() as u64 != cells_for(self.data_len) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "cell table does not cover the block",
            });
        }
        let mut entries: Vec<&TrailerEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        if entries.windows(2).any(|w| sort_key(w[0]) == sort_key(w[1])) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "duplicate entries",
            });
        }
        let mut e = Enc::new();
        for entry in entries {
            let key_len = u16::try_from(entry.key.len()).map_err(|_| FormatError::Invalid {
                what: WHAT,
                reason: "key longer than 65535 bytes",
            })?;
            e.u16(key_len)
                .u16(entry.flags.0)
                .u32(entry.crc32c)
                .u64(entry.offset)
                .u64(entry.length)
                .u64(entry.stamp.raw())
                .bytes(&entry.generation.0)
                .bytes(&entry.key)
                .pad_to(8);
        }
        let entries_len = e.len() as u64;
        for crc in &self.cell_crcs {
            e.u32(*crc);
        }
        e.u32(TRAILER_MAGIC)
            .u16(TRAILER_VERSION)
            .u8(self.kind.code())
            .u8(CELL_SHIFT)
            .u64(self.entries.len() as u64)
            .u64(entries_len)
            .u64(self.cell_crcs.len() as u64)
            .u64(self.data_len)
            .bytes(&self.blake3)
            .bytes(&[0; 20]);
        let mut out = e.finish();
        let crc = crate::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// The trailer that ends `buf`: the tail of a block, of a data shard's data region, or a whole
    /// `.idx` file. The crc covers the whole trailer, so any flipped byte of it is an error.
    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let footer = tail(buf, TRAILER_FOOTER_LEN as u64, WHAT)?;
        let mut f = Dec::new(footer, WHAT);
        expect_magic(WHAT, f.u32()?, TRAILER_MAGIC)?;
        let version = f.u16()?;
        if version != TRAILER_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: WHAT,
                version: u32::from(version),
            });
        }
        let kind = BlockKind::from_code(f.u8()?);
        let shift = f.u8()?;
        if shift != CELL_SHIFT {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "cell shift is not 20",
            });
        }
        let count = f.u64()?;
        let entries_len = f.u64()?;
        let cell_count = f.u64()?;
        let data_len = f.u64()?;
        let blake3 = f.array::<32>()?;
        f.take(20)?;
        let stored = f.u32()?;
        let total = entries_len
            .checked_add(cell_count.saturating_mul(4))
            .and_then(|n| n.checked_add(TRAILER_FOOTER_LEN as u64))
            .ok_or(FormatError::Invalid {
                what: WHAT,
                reason: "trailer length overflows",
            })?;
        let whole = tail(buf, total, WHAT)?;
        expect_crc(WHAT, &whole[..whole.len() - 4], stored)?;
        if cell_count != cells_for(data_len) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "cell table does not cover the block",
            });
        }
        let mut d = Dec::new(whole, WHAT);
        let mut entries: Vec<TrailerEntry> = Vec::new();
        while (d.pos() as u64) < entries_len {
            let key_len = usize::from(d.u16()?);
            let flags = RecordFlags(d.u16()?);
            let crc32c = d.u32()?;
            let offset = d.u64()?;
            let length = d.u64()?;
            let stamp = Stamp::from_raw(d.u64()?);
            let generation = Generation(d.array()?);
            let key = d.take(key_len)?.to_vec();
            d.skip_to(8)?;
            let entry = TrailerEntry {
                key,
                stamp,
                offset,
                length,
                generation,
                crc32c,
                flags,
            };
            if entries
                .last()
                .is_some_and(|prev| sort_key(prev) >= sort_key(&entry))
            {
                return Err(FormatError::Invalid {
                    what: WHAT,
                    reason: "entries are not sorted",
                });
            }
            entries.push(entry);
        }
        if entries.len() as u64 != count || d.pos() as u64 != entries_len {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "entry count does not match",
            });
        }
        let mut cell_crcs = Vec::new();
        for _ in 0..cell_count {
            cell_crcs.push(d.u32()?);
        }
        Ok(Self {
            kind,
            entries,
            cell_crcs,
            data_len,
            blake3,
        })
    }
}

/// The crc32c of each 1 MiB cell of `bytes`, the table a trailer and a shard footer carry.
#[must_use]
pub fn cell_crcs(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks(1 << CELL_SHIFT).map(crate::crc32c).collect()
}
