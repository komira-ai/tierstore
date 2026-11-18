//! The tombstone log `tomb/<origin>/<first_seq>.log` (10.2): tombstones, block drops and tomb
//! drops.
//!
//! The log of each origin node is totally ordered by sequence number and replicated to every
//! storage node; a node pulls `tomb?origin=<n>&after=<seq>` with one cursor per origin. The
//! numbering rule: an origin numbers every entry of its log, of any kind, `1, 2, 3, ...` with no
//! gap, and the number is part of the entry's frame, so every entry has a position whatever its
//! kind. Sequence 0 is not a position: it is the cursor before the first entry ("nothing yet").
//! A node stores each origin's log, its own and every pulled one, under `tomb/<origin>/`, in files
//! that each hold consecutive entries starting at the sequence the file name carries
//! ([`crate::names::HotName::TombLog`]).
//!
//! Every entry is framed as a 24-byte header and a body (little-endian):
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic `TSTL` |
//! | 4 | 1 | kind (1 tombstone, 2 block drop, 3 tomb drop) |
//! | 5 | 1 | origin node id |
//! | 6 | 2 | reserved, zero |
//! | 8 | 8 | seq, the entry's position in the origin's log |
//! | 16 | 4 | body length |
//! | 20 | 4 | crc32c of frame bytes 0..20 followed by the body |
//!
//! The bodies, by kind:
//! - tombstone: key length `u16`, key, generation (16 bytes), stamp `u64`, name count `u32`, and
//!   per name its kind `u8`, length `u16` and bytes;
//! - block drop: block id `u64`, stamp `u64`;
//! - tomb drop: target origin `u8`, target seq `u64`; the reporter is the frame's origin.
//!
//! An entry of a kind this release does not know decodes to [`LogEntry::Unknown`] with its body
//! and keeps its frame's position, so a later release may add a kind and an older node still
//! reads, keeps and forwards its log (AGENTS.md B8).

use crate::FormatError;
use crate::codec::{Dec, Enc, expect_crc, expect_magic};
use crate::ids::{BlockId, Generation, KeyHash, NodeId};
use crate::stamp::Stamp;

/// The magic of a log entry.
pub const TOMB_MAGIC: u32 = u32::from_le_bytes(*b"TSTL");

/// The length of a log entry's frame header.
pub const TOMB_FRAME_LEN: usize = 24;

const WHAT: &str = "tomb log entry";

const KIND_TOMBSTONE: u8 = 1;
const KIND_BLOCK_DROP: u8 = 2;
const KIND_TOMB_DROP: u8 = 3;

/// A large hot file, named by what its file name carries (10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LargeFile {
    /// The hash of the key.
    pub key_hash: KeyHash,
    /// The object's generation.
    pub generation: Generation,
    /// The object's size.
    pub size: u64,
}

/// Something a tombstone names: a block or large file that may list a record of the key with a
/// lower stamp (10.2). The tombstone is dropped only when everything it names is freed.
///
/// Each name is stored as a kind byte, a `u16` length and that many bytes, so a later release can
/// add a kind of name; this release keeps one it does not know as [`Named::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Named {
    /// A block (hot segment, pack block or a chain slice).
    Block(BlockId),
    /// A large hot file.
    LargeFile(LargeFile),
    /// A name of a kind this release does not know, kept as it was read.
    Unknown {
        /// The kind byte.
        kind: u8,
        /// The name's bytes.
        bytes: Vec<u8>,
    },
}

/// A tombstone: the DELETE of a key (10.2, 10.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    /// The key deleted.
    pub key: Vec<u8>,
    /// The generation the deleting writer found (zero bytes when it found none).
    pub generation: Generation,
    /// A stamp above every record of the key the deleting writer found.
    pub stamp: Stamp,
    /// Every block or large file that may list an older record of the key.
    pub names: Vec<Named>,
}

/// A block drop: a committed hot-only eviction of a sealed block (10.14); it counts as the free
/// of that block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockDrop {
    /// The block dropped.
    pub block: BlockId,
    /// The stamp of the drop.
    pub stamp: Stamp,
}

/// A tomb drop: the node whose log holds the entry (the frame's origin) has applied every free
/// that the tombstone `(target_origin, target_seq)` waits for (10.2). A node deletes a tombstone
/// when every node not `out` has reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TombDrop {
    /// The origin of the tombstone reported on.
    pub target_origin: NodeId,
    /// The tombstone's position in its origin's log.
    pub target_seq: u64,
}

/// One entry of a tombstone log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEntry {
    /// A tombstone.
    Tombstone(Tombstone),
    /// A block drop.
    BlockDrop(BlockDrop),
    /// A tomb drop.
    TombDrop(TombDrop),
    /// An entry of a kind this release does not know, kept as it was read.
    Unknown {
        /// The kind byte.
        kind: u8,
        /// The body bytes.
        body: Vec<u8>,
    },
}

/// An entry of a tombstone log with its position: the origin whose log holds it and its sequence
/// number there (at least 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// The node whose log holds the entry.
    pub origin: NodeId,
    /// The entry's position in the origin's log.
    pub seq: u64,
    /// The entry.
    pub entry: LogEntry,
}

impl LogRecord {
    /// The bytes of the record, frame header and body. A sequence of 0, or a [`LogEntry::Unknown`]
    /// whose kind is 1, 2 or 3 (it would decode as the known entry), is refused.
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        if self.seq == 0 {
            return Err(invalid("sequence 0 is not a position"));
        }
        let (kind, body) = match &self.entry {
            LogEntry::Tombstone(t) => (KIND_TOMBSTONE, encode_tombstone(t)?),
            LogEntry::BlockDrop(b) => {
                let mut e = Enc::new();
                e.u64(b.block.raw()).u64(b.stamp.raw());
                (KIND_BLOCK_DROP, e.finish())
            }
            LogEntry::TombDrop(d) => {
                let mut e = Enc::new();
                e.u8(d.target_origin.0).u64(d.target_seq);
                (KIND_TOMB_DROP, e.finish())
            }
            LogEntry::Unknown { kind, body } => {
                if matches!(*kind, KIND_TOMBSTONE | KIND_BLOCK_DROP | KIND_TOMB_DROP) {
                    return Err(invalid("an unknown kind uses a known code"));
                }
                (*kind, body.clone())
            }
        };
        // A body of 4 GiB cannot be built in a test; the error is made before the check so that
        // the line of the check runs on every encode.
        let too_long = invalid("body longer than 4 GiB");
        let len = u32::try_from(body.len()).or(Err(too_long))?;
        let mut head = Enc::new();
        head.u32(TOMB_MAGIC)
            .u8(kind)
            .u8(self.origin.0)
            .bytes(&[0; 2])
            .u64(self.seq)
            .u32(len);
        let mut head = head.finish();
        let crc = frame_crc(&head, &body);
        head.extend_from_slice(&crc.to_le_bytes());
        head.extend_from_slice(&body);
        Ok(head)
    }

    /// The record at the start of `buf` and the bytes it takes. A sequence of 0 is refused.
    pub fn decode(buf: &[u8]) -> Result<(Self, u64), FormatError> {
        let mut d = Dec::new(buf, WHAT);
        expect_magic(WHAT, d.u32()?, TOMB_MAGIC)?;
        let kind = d.u8()?;
        let origin = NodeId(d.u8()?);
        d.take(2)?;
        let seq = d.u64()?;
        let len = d.u32()? as usize;
        let crc = d.u32()?;
        let body = d.take(len)?;
        expect_crc_of(&buf[..TOMB_FRAME_LEN - 4], body, crc)?;
        if seq == 0 {
            return Err(invalid("sequence 0 is not a position"));
        }
        let mut b = Dec::new(body, WHAT);
        let entry = match kind {
            KIND_TOMBSTONE => LogEntry::Tombstone(decode_tombstone(&mut b)?),
            KIND_BLOCK_DROP => LogEntry::BlockDrop(BlockDrop {
                block: BlockId::from_raw(b.u64()?),
                stamp: Stamp::from_raw(b.u64()?),
            }),
            KIND_TOMB_DROP => LogEntry::TombDrop(TombDrop {
                target_origin: NodeId(b.u8()?),
                target_seq: b.u64()?,
            }),
            _ => LogEntry::Unknown {
                kind,
                body: body.to_vec(),
            },
        };
        Ok((Self { origin, seq, entry }, (TOMB_FRAME_LEN + len) as u64))
    }
}

/// The records of a log file's bytes, every one of `origin` and numbered consecutively: the first
/// may carry any sequence (the file's `first_seq`), each next one is the previous plus one. A record
/// of another origin, a gap, a repeat, or any decode error refuses the whole buffer.
pub fn scan_log(buf: &[u8], origin: NodeId) -> Result<Vec<LogRecord>, FormatError> {
    scan(buf, origin, None)
}

/// Like [`scan_log`], for a run that must continue a log at `after`: the first record must carry
/// `after + 1` (`after` 0 is the cursor before the first entry).
pub fn scan_log_after(
    buf: &[u8],
    origin: NodeId,
    after: u64,
) -> Result<Vec<LogRecord>, FormatError> {
    scan(buf, origin, Some(after))
}

fn scan(buf: &[u8], origin: NodeId, mut prev: Option<u64>) -> Result<Vec<LogRecord>, FormatError> {
    let mut out = Vec::new();
    let mut rest = buf;
    while !rest.is_empty() {
        let (record, used) = LogRecord::decode(rest)?;
        if record.origin != origin {
            return Err(invalid("a record of another origin"));
        }
        if prev.is_some_and(|p| p.checked_add(1) != Some(record.seq)) {
            return Err(invalid("a sequence that is not the previous plus one"));
        }
        prev = Some(record.seq);
        rest = &rest[used as usize..];
        out.push(record);
    }
    Ok(out)
}

/// The crc32c of a frame's first 20 bytes followed by its body.
fn frame_crc(head: &[u8], body: &[u8]) -> u32 {
    crate::crc32c(&[head, body].concat())
}

fn expect_crc_of(head: &[u8], body: &[u8], stored: u32) -> Result<(), FormatError> {
    expect_crc(WHAT, &[head, body].concat(), stored)
}

fn invalid(reason: &'static str) -> FormatError {
    FormatError::Invalid { what: WHAT, reason }
}

const NAMED_BLOCK: u8 = 1;
const NAMED_LARGE_FILE: u8 = 2;

fn encode_tombstone(t: &Tombstone) -> Result<Vec<u8>, FormatError> {
    let key_len = u16::try_from(t.key.len()).map_err(|_| FormatError::Invalid {
        what: WHAT,
        reason: "key longer than 65535 bytes",
    })?;
    let too_many = invalid("more than 2^32 - 1 names");
    let names = u32::try_from(t.names.len()).or(Err(too_many))?;
    let mut e = Enc::new();
    e.u16(key_len)
        .bytes(&t.key)
        .bytes(&t.generation.0)
        .u64(t.stamp.raw())
        .u32(names);
    for n in &t.names {
        let (kind, bytes) = match n {
            Named::Block(b) => (NAMED_BLOCK, b.raw().to_le_bytes().to_vec()),
            Named::LargeFile(f) => {
                let mut v = Enc::new();
                v.bytes(&f.key_hash.0).bytes(&f.generation.0).u64(f.size);
                (NAMED_LARGE_FILE, v.finish())
            }
            Named::Unknown { kind, bytes } => {
                if matches!(*kind, NAMED_BLOCK | NAMED_LARGE_FILE) {
                    return Err(invalid("an unknown kind uses a known code"));
                }
                (*kind, bytes.clone())
            }
        };
        let len = u16::try_from(bytes.len()).map_err(|_| FormatError::Invalid {
            what: WHAT,
            reason: "name longer than 65535 bytes",
        })?;
        e.u8(kind).u16(len).bytes(&bytes);
    }
    Ok(e.finish())
}

fn decode_tombstone(d: &mut Dec<'_>) -> Result<Tombstone, FormatError> {
    let key_len = usize::from(d.u16()?);
    let key = d.take(key_len)?.to_vec();
    let generation = Generation(d.array()?);
    let stamp = Stamp::from_raw(d.u64()?);
    let count = d.u32()?;
    let mut names = Vec::new();
    for _ in 0..count {
        let kind = d.u8()?;
        let len = usize::from(d.u16()?);
        let bytes = d.take(len)?;
        let mut n = Dec::new(bytes, WHAT);
        let name = match kind {
            NAMED_BLOCK => Named::Block(BlockId::from_raw(n.u64()?)),
            NAMED_LARGE_FILE => Named::LargeFile(LargeFile {
                key_hash: KeyHash(n.array()?),
                generation: Generation(n.array()?),
                size: n.u64()?,
            }),
            _ => Named::Unknown {
                kind,
                bytes: bytes.to_vec(),
            },
        };
        names.push(name);
    }
    Ok(Tombstone {
        key,
        generation,
        stamp,
        names,
    })
}
