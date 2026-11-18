//! The bytes of the index's own values in the store.
//!
//! These are internal to one node's store: the store is a cache of the bytes on disk, never
//! shipped, and rebuilt from segments, trailers and logs when it cannot be read. Every value starts
//! with a version byte ([`VALUE_VERSION`]); a value of another version, or one that does not decode,
//! is an error ([`IndexError::Store`] with `op` naming the value), never a different answer. The
//! keys of the store are built so that byte order is the order the index needs: ids and stamps
//! are big-endian.

use tierstore_format::ids::{BlockId, Generation, KeyHash, NodeId, StripeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LargeFile, Named};
use tierstore_store_api::index::{
    EntryLocation, HotClean, IndexEntry, IndexError, LiveCount, ReplayMark,
};

/// The version byte every value starts with.
pub(crate) const VALUE_VERSION: u8 = 1;

/// A builder of bytes, little-endian.
#[derive(Default)]
pub(crate) struct Enc(Vec<u8>);

impl Enc {
    pub(crate) fn new() -> Self {
        let mut e = Self(Vec::new());
        e.u8(VALUE_VERSION);
        e
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    pub(crate) fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }

    /// A length-prefixed (`u32`) byte string.
    pub(crate) fn blob(&mut self, v: &[u8]) -> &mut Self {
        // Values the index writes are far below 4 GiB; a longer one is cut to its length's low
        // bits only in a value the index never writes.
        self.u32(v.len() as u32).bytes(v)
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// A reader of bytes written by [`Enc`].
pub(crate) struct Dec<'a> {
    buf: &'a [u8],
    what: &'static str,
}

impl<'a> Dec<'a> {
    /// A reader of a value `what`; checks the version byte.
    pub(crate) fn new(buf: &'a [u8], what: &'static str) -> Result<Self, IndexError> {
        let mut d = Self { buf, what };
        let v = d.u8()?;
        if v != VALUE_VERSION {
            return Err(d.bad("unknown value version"));
        }
        Ok(d)
    }

    pub(crate) fn bad(&self, reason: &str) -> IndexError {
        IndexError::Store {
            op: self.what,
            reason: reason.to_string(),
        }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], IndexError> {
        if self.buf.len() < n {
            return Err(self.bad("value is truncated"));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], IndexError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, IndexError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, IndexError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, IndexError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, IndexError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn blob(&mut self) -> Result<&'a [u8], IndexError> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// Fails unless every byte was read.
    pub(crate) fn end(&self) -> Result<(), IndexError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(self.bad("trailing bytes"))
        }
    }
}

/// What the index holds for a key in [`crate::backend::Space::Entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Held {
    /// A live entry.
    Live(IndexEntry),
    /// A tombstone entry.
    Tomb(Stamp),
}

const TAG_LIVE: u8 = 1;
const TAG_TOMB: u8 = 2;

const LOC_BLOCK: u8 = 1;
const LOC_LARGE: u8 = 2;
const LOC_CHAIN: u8 = 3;

pub(crate) fn encode_location(e: &mut Enc, loc: &EntryLocation) {
    match loc {
        EntryLocation::Block { block, offset } => {
            e.u8(LOC_BLOCK).u64(block.raw()).u64(*offset);
        }
        EntryLocation::Large(f) => {
            e.u8(LOC_LARGE)
                .bytes(&f.key_hash.0)
                .bytes(&f.generation.0)
                .u64(f.size);
        }
        EntryLocation::Chain { first } => {
            e.u8(LOC_CHAIN).u64(first.raw());
        }
    }
}

pub(crate) fn decode_location(d: &mut Dec<'_>) -> Result<EntryLocation, IndexError> {
    match d.u8()? {
        LOC_BLOCK => Ok(EntryLocation::Block {
            block: BlockId::from_raw(d.u64()?),
            offset: d.u64()?,
        }),
        LOC_LARGE => Ok(EntryLocation::Large(LargeFile {
            key_hash: KeyHash(d.array()?),
            generation: Generation(d.array()?),
            size: d.u64()?,
        })),
        LOC_CHAIN => Ok(EntryLocation::Chain {
            first: BlockId::from_raw(d.u64()?),
        }),
        _ => Err(d.bad("unknown location kind")),
    }
}

/// The bytes of an entry, live or tombstone.
pub(crate) fn encode_held(h: &Held) -> Vec<u8> {
    let mut e = Enc::new();
    match h {
        Held::Live(entry) => {
            e.u8(TAG_LIVE)
                .bytes(&entry.generation.0)
                .u64(entry.stamp.raw())
                .u16(entry.flags.0)
                .u64(entry.length);
            encode_location(&mut e, &entry.location);
            e.u32(entry.prior_blocks.len() as u32);
            for b in &entry.prior_blocks {
                e.u64(b.raw());
            }
            match &entry.promoted {
                None => {
                    e.u8(0);
                }
                Some(p) => {
                    e.u8(1).u8(p.node.0).u64(p.boot).u64(p.seq).u64(p.offset);
                }
            }
        }
        Held::Tomb(stamp) => {
            e.u8(TAG_TOMB).u64(stamp.raw());
        }
    }
    e.finish()
}

/// The entry `buf` holds.
pub(crate) fn decode_held(buf: &[u8]) -> Result<Held, IndexError> {
    let mut d = Dec::new(buf, "decode entry")?;
    let held = match d.u8()? {
        TAG_LIVE => {
            let generation = Generation(d.array()?);
            let stamp = Stamp::from_raw(d.u64()?);
            let flags = RecordFlags(d.u16()?);
            let length = d.u64()?;
            let location = decode_location(&mut d)?;
            let n = d.u32()?;
            let mut prior_blocks = Vec::new();
            for _ in 0..n {
                prior_blocks.push(BlockId::from_raw(d.u64()?));
            }
            let promoted = match d.u8()? {
                0 => None,
                1 => Some(HotClean {
                    node: NodeId(d.u8()?),
                    boot: d.u64()?,
                    seq: d.u64()?,
                    offset: d.u64()?,
                }),
                _ => return Err(d.bad("unknown promoted tag")),
            };
            Held::Live(IndexEntry {
                generation,
                location,
                length,
                stamp,
                flags,
                prior_blocks,
                promoted,
            })
        }
        TAG_TOMB => Held::Tomb(Stamp::from_raw(d.u64()?)),
        _ => return Err(d.bad("unknown entry tag")),
    };
    d.end()?;
    Ok(held)
}

/// The key of a block or stripe id in the store: big-endian, so byte order is id order.
pub(crate) fn id_key(raw: u64) -> Vec<u8> {
    raw.to_be_bytes().to_vec()
}

/// The id of an 8-byte big-endian key.
pub(crate) fn id_of(key: &[u8]) -> Result<u64, IndexError> {
    let arr: [u8; 8] = key.try_into().map_err(|_| IndexError::Store {
        op: "decode id key",
        reason: "key is not 8 bytes".to_string(),
    })?;
    Ok(u64::from_be_bytes(arr))
}

/// A block id's key.
pub(crate) fn block_key(b: BlockId) -> Vec<u8> {
    id_key(b.raw())
}

/// A stripe id's key.
pub(crate) fn stripe_key(s: StripeId) -> Vec<u8> {
    id_key(s.raw())
}

pub(crate) fn encode_live(c: LiveCount) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(c.entries).u64(c.bytes);
    e.finish()
}

pub(crate) fn decode_live(buf: &[u8]) -> Result<LiveCount, IndexError> {
    let mut d = Dec::new(buf, "decode live count")?;
    let c = LiveCount {
        entries: d.u64()?,
        bytes: d.u64()?,
    };
    d.end()?;
    Ok(c)
}

pub(crate) fn encode_mark(m: &ReplayMark) -> Vec<u8> {
    let mut e = Enc::new();
    e.u32(m.open_segments.len() as u32);
    for (b, len) in &m.open_segments {
        e.u64(b.raw()).u64(*len);
    }
    e.u32(m.tomb_cursors.len() as u32);
    for (n, seq) in &m.tomb_cursors {
        e.u8(n.0).u64(*seq);
    }
    e.finish()
}

pub(crate) fn decode_mark(buf: &[u8]) -> Result<ReplayMark, IndexError> {
    let mut d = Dec::new(buf, "decode replay mark")?;
    let mut m = ReplayMark::default();
    for _ in 0..d.u32()? {
        m.open_segments
            .push((BlockId::from_raw(d.u64()?), d.u64()?));
    }
    for _ in 0..d.u32()? {
        m.tomb_cursors.push((NodeId(d.u8()?), d.u64()?));
    }
    d.end()?;
    Ok(m)
}

pub(crate) fn encode_u64(v: u64) -> Vec<u8> {
    let mut e = Enc::new();
    e.u64(v);
    e.finish()
}

pub(crate) fn decode_u64(buf: &[u8], what: &'static str) -> Result<u64, IndexError> {
    let mut d = Dec::new(buf, what)?;
    let v = d.u64()?;
    d.end()?;
    Ok(v)
}

const NAMED_BLOCK: u8 = 1;
const NAMED_LARGE: u8 = 2;

/// The key form of a name: kind, `u16` big-endian length, bytes (ids big-endian). Names that are
/// equal have equal bytes, and no name's bytes are a prefix of another's.
pub(crate) fn named_key(n: &Named) -> Vec<u8> {
    let (kind, bytes) = match n {
        Named::Block(b) => (NAMED_BLOCK, b.raw().to_be_bytes().to_vec()),
        Named::LargeFile(f) => {
            let mut v = Vec::with_capacity(40);
            v.extend_from_slice(&f.key_hash.0);
            v.extend_from_slice(&f.generation.0);
            v.extend_from_slice(&f.size.to_be_bytes());
            (NAMED_LARGE, v)
        }
        Named::Unknown { kind, bytes } => (*kind, bytes.clone()),
    };
    let mut out = Vec::with_capacity(3 + bytes.len());
    out.push(kind);
    // A name's bytes are at most 65535 long (its length is a u16 in the tomb log).
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(&bytes);
    out
}

/// The name at the start of `buf` and the bytes it takes.
pub(crate) fn named_of(buf: &[u8]) -> Result<(Named, usize), IndexError> {
    let bad = |reason: &str| IndexError::Store {
        op: "decode name",
        reason: reason.to_string(),
    };
    if buf.len() < 3 {
        return Err(bad("name is truncated"));
    }
    let kind = buf[0];
    let len = usize::from(u16::from_be_bytes([buf[1], buf[2]]));
    let bytes = buf
        .get(3..3 + len)
        .ok_or_else(|| bad("name is truncated"))?;
    let named = match (kind, bytes.len()) {
        (NAMED_BLOCK, 8) => {
            let mut a = [0u8; 8];
            a.copy_from_slice(bytes);
            Named::Block(BlockId::from_raw(u64::from_be_bytes(a)))
        }
        (NAMED_LARGE, 40) => {
            let mut h = [0u8; 16];
            let mut g = [0u8; 16];
            let mut s = [0u8; 8];
            h.copy_from_slice(&bytes[..16]);
            g.copy_from_slice(&bytes[16..32]);
            s.copy_from_slice(&bytes[32..]);
            Named::LargeFile(LargeFile {
                key_hash: KeyHash(h),
                generation: Generation(g),
                size: u64::from_be_bytes(s),
            })
        }
        (NAMED_BLOCK | NAMED_LARGE, _) => return Err(bad("name has the wrong length")),
        _ => Named::Unknown {
            kind,
            bytes: bytes.to_vec(),
        },
    };
    Ok((named, 3 + len))
}

/// The key of a tomb log position in [`crate::backend::Space::TombPos`].
pub(crate) fn pos_key(origin: NodeId, seq: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(origin.0);
    out.extend_from_slice(&seq.to_be_bytes());
    out
}

/// The origin and sequence of a position key.
pub(crate) fn pos_of(k: &[u8]) -> Result<(NodeId, u64), IndexError> {
    if k.len() != 9 {
        return Err(IndexError::Store {
            op: "decode tomb position",
            reason: "key is not 9 bytes".to_string(),
        });
    }
    let mut s = [0u8; 8];
    s.copy_from_slice(&k[1..]);
    Ok((NodeId(k[0]), u64::from_be_bytes(s)))
}

/// A tombstone the index holds until its drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TombRow {
    /// The generation the deleting writer found.
    pub(crate) generation: Generation,
    /// Every block or large file that may list an older record of the key, sorted.
    pub(crate) names: Vec<Named>,
    /// Its position in its origin's log, once known.
    pub(crate) position: Option<(NodeId, u64)>,
}

/// The tombstones of one key ([`crate::backend::Space::Tombs`]), by stamp.
pub(crate) fn encode_tombs(rows: &[(Stamp, TombRow)]) -> Vec<u8> {
    let mut e = Enc::new();
    e.u32(rows.len() as u32);
    for (stamp, t) in rows {
        e.u64(stamp.raw()).bytes(&t.generation.0);
        e.u32(t.names.len() as u32);
        for n in &t.names {
            e.blob(&named_key(n));
        }
        match t.position {
            None => {
                e.u8(0);
            }
            Some((o, s)) => {
                e.u8(1).u8(o.0).u64(s);
            }
        }
    }
    e.finish()
}

pub(crate) fn decode_tombs(buf: &[u8]) -> Result<Vec<(Stamp, TombRow)>, IndexError> {
    let mut d = Dec::new(buf, "decode tombstones")?;
    let mut rows = Vec::new();
    for _ in 0..d.u32()? {
        let stamp = Stamp::from_raw(d.u64()?);
        let generation = Generation(d.array()?);
        let mut names = Vec::new();
        for _ in 0..d.u32()? {
            let blob = d.blob()?;
            let (n, used) = named_of(blob)?;
            if used != blob.len() {
                return Err(d.bad("name has trailing bytes"));
            }
            names.push(n);
        }
        let position = match d.u8()? {
            0 => None,
            1 => Some((NodeId(d.u8()?), d.u64()?)),
            _ => return Err(d.bad("unknown position tag")),
        };
        rows.push((
            stamp,
            TombRow {
                generation,
                names,
                position,
            },
        ));
    }
    d.end()?;
    Ok(rows)
}

/// A tomb log position: the tombstone there, once known, and the nodes that reported its drop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PosRow {
    /// The object key and stamp of the tombstone at the position.
    pub(crate) target: Option<(Vec<u8>, Stamp)>,
    /// The reporting nodes, a bit per node id.
    pub(crate) reports: [u8; 32],
}

impl PosRow {
    pub(crate) fn has(&self, n: NodeId) -> bool {
        self.reports[usize::from(n.0 / 8)] & (1 << (n.0 % 8)) != 0
    }

    pub(crate) fn add(&mut self, n: NodeId) -> bool {
        let had = self.has(n);
        self.reports[usize::from(n.0 / 8)] |= 1 << (n.0 % 8);
        !had
    }
}

pub(crate) fn encode_pos(p: &PosRow) -> Vec<u8> {
    let mut e = Enc::new();
    match &p.target {
        None => {
            e.u8(0);
        }
        Some((k, s)) => {
            e.u8(1).blob(k).u64(s.raw());
        }
    }
    e.bytes(&p.reports);
    e.finish()
}

pub(crate) fn decode_pos(buf: &[u8]) -> Result<PosRow, IndexError> {
    let mut d = Dec::new(buf, "decode tomb position row")?;
    let target = match d.u8()? {
        0 => None,
        1 => Some((d.blob()?.to_vec(), Stamp::from_raw(d.u64()?))),
        _ => return Err(d.bad("unknown target tag")),
    };
    let reports = d.array()?;
    d.end()?;
    Ok(PosRow { target, reports })
}
