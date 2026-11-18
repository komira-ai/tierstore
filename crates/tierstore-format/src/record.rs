//! The record (10.2): the unit of append, of a replication batch and of an index entry.
//!
//! A record is a 64-byte header, the key bytes, the data, then zero padding to 16 bytes, so a
//! segment is a sequence of self-describing records that can be scanned without any index. The
//! header layout (little-endian):
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic `TSR1` |
//! | 4 | 2 | flags ([`RecordFlags`]) |
//! | 6 | 2 | key length |
//! | 8 | 8 | data length |
//! | 16 | 16 | generation |
//! | 32 | 8 | stamp |
//! | 40 | 8 | object offset (of a chain slice; 0 otherwise) |
//! | 48 | 4 | crc32c of the data |
//! | 52 | 4 | crc32c of the key bytes |
//! | 56 | 4 | reserved, zero |
//! | 60 | 4 | crc32c of bytes 0..60 |
//!
//! **Relocate.** A relocate (a GC rewrite, 10.5) is a record with [`RecordFlags::RELOCATE`] that
//! carries the stamp of the record it moves; at equal stamps it supersedes that record. A pack
//! block assembled at destage carries the flag in its trailer entries instead (see
//! [`crate::trailer`]), since its records are applied with the descriptor commit.

use crate::FormatError;
use crate::codec::{Dec, Enc, expect_crc, expect_magic};
use crate::ids::Generation;
use crate::stamp::Stamp;

/// The magic of a record header.
pub const RECORD_MAGIC: u32 = u32::from_le_bytes(*b"TSR1");

/// The length of a record header.
pub const RECORD_HEADER_LEN: usize = 64;

/// The alignment of records in a segment (a 1 KiB object costs about 1.1 KiB, 10.3).
pub const RECORD_ALIGN: usize = 16;

const WHAT: &str = "record";

/// The flags of a record header and of a trailer entry. Bits this release does not know are kept
/// and ignored, so a later release can add one (AGENTS.md B8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RecordFlags(pub u16);

impl RecordFlags {
    /// A relocate: the record moves an earlier record of the same key, generation and stamp.
    pub const RELOCATE: Self = Self(1);
    /// One slice of a chain (an object larger than `block_bytes - block_reserve`, 10.2); the
    /// header's object offset says where the slice starts in the object.
    pub const CHAIN_SLICE: Self = Self(1 << 1);

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union of the two sets of bits.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A record's header, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// The record's flags.
    pub flags: RecordFlags,
    /// The length of the key bytes that follow the header.
    pub key_len: u16,
    /// The length of the data that follows the key.
    pub data_len: u64,
    /// The generation of the object (of the whole object, for a chain slice).
    pub generation: Generation,
    /// The cluster stamp the writer assigned at append (or of the moved record, for a relocate).
    pub stamp: Stamp,
    /// Where this record's data starts in the object: 0 unless the record is a chain slice.
    pub object_offset: u64,
    /// The crc32c of the data.
    pub data_crc: u32,
    /// The crc32c of the key bytes, so a flipped key byte is an error and never a record read
    /// under another key.
    pub key_crc: u32,
}

impl RecordHeader {
    /// The 64 header bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_HEADER_LEN] {
        let mut e = Enc::new();
        e.u32(RECORD_MAGIC)
            .u16(self.flags.0)
            .u16(self.key_len)
            .u64(self.data_len)
            .bytes(&self.generation.0)
            .u64(self.stamp.raw())
            .u64(self.object_offset)
            .u32(self.data_crc)
            .u32(self.key_crc)
            .u32(0);
        let mut out = [0u8; RECORD_HEADER_LEN];
        let body = e.finish();
        out[..60].copy_from_slice(&body);
        let crc = crate::crc32c(&out[..60]);
        out[60..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// The header at the start of `buf`: its magic and header crc are checked; the reserved
    /// bytes are not (a later release may use them).
    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let mut d = Dec::new(buf, WHAT);
        let head = d.take(RECORD_HEADER_LEN)?;
        let mut d = Dec::new(head, WHAT);
        expect_magic(WHAT, d.u32()?, RECORD_MAGIC)?;
        let flags = RecordFlags(d.u16()?);
        let key_len = d.u16()?;
        let data_len = d.u64()?;
        let generation = Generation(d.array()?);
        let stamp = Stamp::from_raw(d.u64()?);
        let object_offset = d.u64()?;
        let data_crc = d.u32()?;
        let key_crc = d.u32()?;
        d.take(4)?;
        let stored = d.u32()?;
        expect_crc(WHAT, &head[..60], stored)?;
        Ok(Self {
            flags,
            key_len,
            data_len,
            generation,
            stamp,
            object_offset,
            data_crc,
            key_crc,
        })
    }

    /// The bytes the whole record takes in a segment: header, key, data and padding.
    #[must_use]
    pub fn record_len(&self) -> u64 {
        record_len(self.key_len, self.data_len)
    }
}

/// The bytes a record of `key_len` key bytes and `data_len` data bytes takes in a segment.
#[must_use]
pub fn record_len(key_len: u16, data_len: u64) -> u64 {
    // Saturating, so a header that claims a `data_len` near `u64::MAX` (its crc can be right) is
    // a record longer than any buffer, a `Short` for the decode, never an overflow.
    let align = RECORD_ALIGN as u64;
    (RECORD_HEADER_LEN as u64 + u64::from(key_len))
        .saturating_add(data_len)
        .saturating_add(align - 1)
        & !(align - 1)
}

/// A record to encode: what the writer knows when it appends.
#[derive(Debug, Clone, Copy)]
pub struct RecordIn<'a> {
    /// The flags.
    pub flags: RecordFlags,
    /// The key bytes (at most `u16::MAX`).
    pub key: &'a [u8],
    /// The data.
    pub data: &'a [u8],
    /// The object's generation.
    pub generation: Generation,
    /// The stamp.
    pub stamp: Stamp,
    /// The object offset of a chain slice, else 0.
    pub object_offset: u64,
}

/// The bytes of one record: header, key, data and padding to [`RECORD_ALIGN`].
pub fn encode_record(rec: &RecordIn<'_>) -> Result<Vec<u8>, FormatError> {
    let key_len = u16::try_from(rec.key.len()).map_err(|_| FormatError::Invalid {
        what: WHAT,
        reason: "key longer than 65535 bytes",
    })?;
    let header = RecordHeader {
        flags: rec.flags,
        key_len,
        data_len: rec.data.len() as u64,
        generation: rec.generation,
        stamp: rec.stamp,
        object_offset: rec.object_offset,
        data_crc: crate::crc32c(rec.data),
        key_crc: crate::crc32c(rec.key),
    };
    let mut e = Enc::new();
    e.bytes(&header.encode())
        .bytes(rec.key)
        .bytes(rec.data)
        .pad_to(RECORD_ALIGN);
    Ok(e.finish())
}

/// A record decoded in place: the header and slices of the buffer it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// The header.
    pub header: RecordHeader,
    /// The key bytes.
    pub key: &'a [u8],
    /// The data, whose crc32c equals the header's.
    pub data: &'a [u8],
}

/// The record at the start of `buf` and the bytes it takes (padding included). The header crc,
/// the key crc and the data crc are all checked, so a torn or bit-flipped record is an error.
pub fn decode_record(buf: &[u8]) -> Result<(Record<'_>, u64), FormatError> {
    let header = RecordHeader::decode(buf)?;
    let len = header.record_len();
    let have = buf.len() as u64;
    if len > have {
        return Err(FormatError::Short {
            what: WHAT,
            need: len,
            have,
        });
    }
    let key_end = RECORD_HEADER_LEN + usize::from(header.key_len);
    // `len <= have`, so the data end is inside `buf`.
    let data_end = key_end + header.data_len as usize;
    let key = &buf[RECORD_HEADER_LEN..key_end];
    expect_crc(WHAT, key, header.key_crc)?;
    let data = &buf[key_end..data_end];
    expect_crc(WHAT, data, header.data_crc)?;
    Ok((Record { header, key, data }, len))
}

/// What a scan of a segment's bytes found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// Each valid record from the start: its offset in the scanned bytes and its header.
    pub records: Vec<(u64, RecordHeader)>,
    /// The bytes up to the end of the last valid record: the committed prefix a replay keeps.
    pub valid_len: u64,
    /// Why the scan stopped before the end of the bytes, if it did (a torn or flipped record).
    pub stop: Option<FormatError>,
}

/// Scans `buf` from offset 0 record by record and stops at the first record that does not decode
/// (10.2 replay: the record scan of an open segment from its mark to the last record whose crc
/// verifies). Bytes that are all zero from some offset on are the end of the records, not an
/// error: a preallocated segment is zero past its committed length.
#[must_use]
pub fn scan(buf: &[u8]) -> Scan {
    let mut records = Vec::new();
    let mut pos = 0u64;
    let mut stop = None;
    while (pos as usize) < buf.len() {
        let rest = &buf[pos as usize..];
        if rest.iter().all(|b| *b == 0) {
            break;
        }
        match decode_record(rest) {
            Ok((rec, len)) => {
                records.push((pos, rec.header));
                pos += len;
            }
            Err(e) => {
                stop = Some(e);
                break;
            }
        }
    }
    Scan {
        records,
        valid_len: pos,
        stop,
    }
}
