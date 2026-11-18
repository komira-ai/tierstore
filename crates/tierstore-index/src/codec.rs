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

#[cfg(test)]
mod tests {
    use super::*;

    fn generation_of(b: u8) -> Generation {
        Generation([b; 16])
    }

    fn large() -> LargeFile {
        LargeFile {
            key_hash: KeyHash([3; 16]),
            generation: generation_of(4),
            size: 5_000_000_000,
        }
    }

    fn entry(location: EntryLocation, prior: &[u64], promoted: Option<HotClean>) -> IndexEntry {
        IndexEntry {
            generation: generation_of(7),
            location,
            length: 1234,
            stamp: Stamp::from_raw(0x0102_0304_0506),
            flags: RecordFlags(3),
            prior_blocks: prior.iter().map(|b| BlockId::from_raw(*b)).collect(),
            promoted,
        }
    }

    fn hot() -> HotClean {
        HotClean {
            node: NodeId(9),
            boot: 11,
            seq: 12,
            offset: 13,
        }
    }

    fn block_loc() -> EntryLocation {
        EntryLocation::Block {
            block: BlockId::from_raw(0x0100_0000_0000_0002),
            offset: 4096,
        }
    }

    fn row(position: Option<(NodeId, u64)>, names: Vec<Named>) -> TombRow {
        TombRow {
            generation: generation_of(1),
            names,
            position,
        }
    }

    /// The operation and the reason of a store error, read from its message.
    fn store_reason<T: std::fmt::Debug>(r: Result<T, IndexError>) -> (String, String) {
        let text = r.expect_err("an error").to_string();
        let (op, reason) = text
            .strip_prefix("index store: ")
            .and_then(|rest| rest.split_once(": "))
            .expect("the message of a store error");
        (op.to_string(), reason.to_string())
    }

    fn truncations_fail<T: std::fmt::Debug>(
        bytes: &[u8],
        decode: impl Fn(&[u8]) -> Result<T, IndexError>,
    ) {
        for n in 0..bytes.len() {
            let (_, reason) = store_reason(decode(&bytes[..n]));
            assert_eq!(
                reason,
                "value is truncated",
                "cut at {n} of {}",
                bytes.len()
            );
        }
        decode(bytes).expect("the whole value decodes");
    }

    fn trailing_fails<T: std::fmt::Debug>(
        bytes: &[u8],
        decode: impl Fn(&[u8]) -> Result<T, IndexError>,
    ) {
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert_eq!(store_reason(decode(&longer)).1, "trailing bytes");
    }

    fn version_fails<T: std::fmt::Debug>(
        bytes: &[u8],
        decode: impl Fn(&[u8]) -> Result<T, IndexError>,
    ) {
        let mut other = bytes.to_vec();
        other[0] = VALUE_VERSION + 1;
        assert_eq!(store_reason(decode(&other)).1, "unknown value version");
    }

    fn all_entries() -> Vec<Held> {
        vec![
            Held::Live(entry(block_loc(), &[], None)),
            Held::Live(entry(block_loc(), &[5, 6], Some(hot()))),
            Held::Live(entry(EntryLocation::Large(large()), &[8], None)),
            Held::Live(entry(
                EntryLocation::Chain {
                    first: BlockId::from_raw(77),
                },
                &[],
                Some(hot()),
            )),
            Held::Tomb(Stamp::from_raw(99)),
        ]
    }

    #[test]
    fn held_values_of_every_kind_round_trip() {
        for h in all_entries() {
            assert_eq!(decode_held(&encode_held(&h)).expect("decodes"), h);
        }
    }

    #[test]
    fn every_truncation_of_a_held_value_is_an_error() {
        for h in all_entries() {
            truncations_fail(&encode_held(&h), decode_held);
        }
    }

    #[test]
    fn held_value_with_unknown_version_or_trailing_bytes_is_an_error() {
        for h in all_entries() {
            let bytes = encode_held(&h);
            version_fails(&bytes, decode_held);
            trailing_fails(&bytes, decode_held);
        }
    }

    #[test]
    fn held_value_with_unknown_entry_tag_is_an_error() {
        let mut bytes = encode_held(&Held::Tomb(Stamp::from_raw(1)));
        bytes[1] = 9;
        let (op, reason) = store_reason(decode_held(&bytes));
        assert_eq!(
            (op.as_str(), reason.as_str()),
            ("decode entry", "unknown entry tag")
        );
    }

    #[test]
    fn held_value_with_unknown_location_tag_is_an_error() {
        let mut bytes = encode_held(&Held::Live(entry(block_loc(), &[], None)));
        bytes[36] = 9;
        assert_eq!(store_reason(decode_held(&bytes)).1, "unknown location kind");
    }

    #[test]
    fn held_value_with_unknown_promoted_tag_is_an_error() {
        let mut bytes = encode_held(&Held::Live(entry(block_loc(), &[], None)));
        let last = bytes.len() - 1;
        bytes[last] = 9;
        assert_eq!(store_reason(decode_held(&bytes)).1, "unknown promoted tag");
    }

    #[test]
    fn live_counts_round_trip_and_reject_damage() {
        let c = LiveCount {
            entries: 3,
            bytes: 1 << 40,
        };
        let bytes = encode_live(c);
        assert_eq!(decode_live(&bytes).expect("decodes"), c);
        truncations_fail(&bytes, decode_live);
        trailing_fails(&bytes, decode_live);
        version_fails(&bytes, decode_live);
    }

    #[test]
    fn replay_marks_round_trip_and_reject_damage() {
        let empty = ReplayMark::default();
        assert_eq!(decode_mark(&encode_mark(&empty)).expect("decodes"), empty);
        let m = ReplayMark {
            open_segments: vec![(BlockId::from_raw(1), 10), (BlockId::from_raw(2), 20)],
            tomb_cursors: vec![(NodeId(0), 5), (NodeId(255), 6)],
        };
        let bytes = encode_mark(&m);
        assert_eq!(decode_mark(&bytes).expect("decodes"), m);
        truncations_fail(&bytes, decode_mark);
        trailing_fails(&bytes, decode_mark);
        version_fails(&bytes, decode_mark);
    }

    #[test]
    fn u64_values_round_trip_and_name_the_value_in_errors() {
        let bytes = encode_u64(u64::MAX);
        assert_eq!(decode_u64(&bytes, "decode x").expect("decodes"), u64::MAX);
        truncations_fail(&bytes, |b| decode_u64(b, "decode x"));
        let mut longer = bytes;
        longer.push(1);
        let (op, reason) = store_reason(decode_u64(&longer, "decode x"));
        assert_eq!(
            (op.as_str(), reason.as_str()),
            ("decode x", "trailing bytes")
        );
    }

    #[test]
    fn id_keys_are_big_endian_and_decode_back() {
        assert_eq!(id_key(0x0102), vec![0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(id_of(&id_key(u64::MAX - 1)).expect("an id"), u64::MAX - 1);
        assert_eq!(block_key(BlockId::from_raw(5)), id_key(5));
        assert_eq!(stripe_key(StripeId::from_raw(6)), id_key(6));
        assert!(block_key(BlockId::from_raw(1)) < block_key(BlockId::from_raw(256)));
    }

    #[test]
    fn id_of_a_key_that_is_not_8_bytes_is_an_error() {
        for key in [&[][..], &[1; 7][..], &[1; 9][..]] {
            let (op, reason) = store_reason(id_of(key));
            assert_eq!(
                (op.as_str(), reason.as_str()),
                ("decode id key", "key is not 8 bytes")
            );
        }
    }

    fn names() -> Vec<Named> {
        vec![
            Named::Block(BlockId::from_raw(0x0200_0000_0000_0001)),
            Named::LargeFile(large()),
            Named::Unknown {
                kind: 9,
                bytes: vec![1, 2, 3],
            },
            Named::Unknown {
                kind: 200,
                bytes: Vec::new(),
            },
        ]
    }

    #[test]
    fn names_of_every_kind_round_trip_through_their_key_form() {
        for n in names() {
            let key = named_key(&n);
            assert_eq!(named_of(&key).expect("decodes"), (n, key.len()));
        }
    }

    #[test]
    fn name_keys_carry_kind_and_big_endian_length_and_none_is_a_prefix_of_another() {
        let key = named_key(&Named::Block(BlockId::from_raw(1)));
        assert_eq!(key, vec![1, 0, 8, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(named_key(&Named::LargeFile(large())).len(), 3 + 40);
        let all: Vec<Vec<u8>> = names().iter().map(named_key).collect();
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate().filter(|(j, _)| *j != i) {
                assert!(!b.starts_with(a), "{i} is a prefix of {j}");
            }
        }
    }

    #[test]
    fn name_of_the_wrong_length_for_its_kind_is_an_error() {
        let short_block = named_key(&Named::Unknown {
            kind: NAMED_BLOCK,
            bytes: vec![0; 7],
        });
        let long_large = named_key(&Named::Unknown {
            kind: NAMED_LARGE,
            bytes: vec![0; 41],
        });
        for key in [short_block, long_large] {
            let (op, reason) = store_reason(named_of(&key));
            assert_eq!(
                (op.as_str(), reason.as_str()),
                ("decode name", "name has the wrong length")
            );
        }
    }

    #[test]
    fn truncated_name_is_an_error() {
        let key = named_key(&Named::Block(BlockId::from_raw(1)));
        for n in 0..key.len() {
            let (op, reason) = store_reason(named_of(&key[..n]));
            assert_eq!(
                (op.as_str(), reason.as_str()),
                ("decode name", "name is truncated")
            );
        }
    }

    #[test]
    fn name_decoding_stops_after_its_own_bytes() {
        let mut key = named_key(&Named::Block(BlockId::from_raw(1)));
        let used = key.len();
        key.extend_from_slice(&[9, 9]);
        assert_eq!(
            named_of(&key).expect("decodes"),
            (Named::Block(BlockId::from_raw(1)), used)
        );
    }

    #[test]
    fn tomb_position_keys_round_trip_and_sort_by_origin_then_sequence() {
        let k = pos_key(NodeId(7), 0x0102);
        assert_eq!(k, vec![7, 0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(pos_of(&k).expect("decodes"), (NodeId(7), 0x0102));
        assert!(pos_key(NodeId(1), 9) < pos_key(NodeId(2), 1));
        assert!(pos_key(NodeId(1), 255) < pos_key(NodeId(1), 256));
    }

    #[test]
    fn tomb_position_key_of_the_wrong_length_is_an_error() {
        for key in [&[][..], &[0; 8][..], &[0; 10][..]] {
            let (op, reason) = store_reason(pos_of(key));
            assert_eq!(
                (op.as_str(), reason.as_str()),
                ("decode tomb position", "key is not 9 bytes")
            );
        }
    }

    fn tomb_rows() -> Vec<(Stamp, TombRow)> {
        vec![
            (Stamp::from_raw(10), row(None, Vec::new())),
            (Stamp::from_raw(20), row(Some((NodeId(3), 44)), names())),
            (Stamp::from_raw(30), row(None, names())),
        ]
    }

    #[test]
    fn tombstone_rows_round_trip_with_names_of_every_kind_and_positions() {
        assert_eq!(
            decode_tombs(&encode_tombs(&[])).expect("decodes"),
            Vec::new()
        );
        let rows = tomb_rows();
        assert_eq!(decode_tombs(&encode_tombs(&rows)).expect("decodes"), rows);
    }

    #[test]
    fn every_truncation_of_tombstone_rows_is_an_error() {
        truncations_fail(&encode_tombs(&tomb_rows()), decode_tombs);
    }

    #[test]
    fn tombstone_rows_with_unknown_version_or_trailing_bytes_are_an_error() {
        let bytes = encode_tombs(&tomb_rows());
        version_fails(&bytes, decode_tombs);
        trailing_fails(&bytes, decode_tombs);
    }

    #[test]
    fn tombstone_row_with_unknown_position_tag_is_an_error() {
        let mut bytes = encode_tombs(&[(Stamp::from_raw(1), row(None, Vec::new()))]);
        let last = bytes.len() - 1;
        bytes[last] = 9;
        assert_eq!(store_reason(decode_tombs(&bytes)).1, "unknown position tag");
    }

    #[test]
    fn tombstone_row_with_a_name_that_does_not_decode_is_an_error() {
        let mut e = Enc::new();
        e.u32(1).u64(5).bytes(&generation_of(1).0).u32(1);
        e.blob(&named_key(&Named::Unknown {
            kind: NAMED_BLOCK,
            bytes: vec![0; 3],
        }));
        e.u8(0);
        assert_eq!(
            store_reason(decode_tombs(&e.finish())).1,
            "name has the wrong length"
        );
    }

    #[test]
    fn tombstone_row_with_bytes_after_a_name_is_an_error() {
        let mut blob = named_key(&Named::Block(BlockId::from_raw(1)));
        blob.push(0);
        let mut e = Enc::new();
        e.u32(1)
            .u64(5)
            .bytes(&generation_of(1).0)
            .u32(1)
            .blob(&blob)
            .u8(0);
        let (op, reason) = store_reason(decode_tombs(&e.finish()));
        assert_eq!(
            (op.as_str(), reason.as_str()),
            ("decode tombstones", "name has trailing bytes")
        );
    }

    fn pos_rows() -> Vec<PosRow> {
        let mut with_reports = PosRow {
            target: Some((b"key/one".to_vec(), Stamp::from_raw(77))),
            reports: [0; 32],
        };
        with_reports.add(NodeId(0));
        with_reports.add(NodeId(255));
        vec![PosRow::default(), with_reports]
    }

    #[test]
    fn position_rows_round_trip_with_and_without_a_target() {
        for p in pos_rows() {
            assert_eq!(decode_pos(&encode_pos(&p)).expect("decodes"), p);
        }
    }

    #[test]
    fn every_truncation_of_a_position_row_is_an_error() {
        for p in pos_rows() {
            truncations_fail(&encode_pos(&p), decode_pos);
        }
    }

    #[test]
    fn position_row_with_unknown_version_or_trailing_bytes_is_an_error() {
        for p in pos_rows() {
            let bytes = encode_pos(&p);
            version_fails(&bytes, decode_pos);
            trailing_fails(&bytes, decode_pos);
        }
    }

    #[test]
    fn position_row_with_unknown_target_tag_is_an_error() {
        let mut bytes = encode_pos(&PosRow::default());
        bytes[1] = 9;
        assert_eq!(store_reason(decode_pos(&bytes)).1, "unknown target tag");
    }

    #[test]
    fn position_row_reports_are_one_bit_per_node_id_at_the_edges_of_a_byte() {
        for id in [0u8, 7, 8, 255] {
            let mut p = PosRow::default();
            assert!(!p.has(NodeId(id)));
            assert!(p.add(NodeId(id)), "first report of {id} is new");
            assert!(!p.add(NodeId(id)), "second report of {id} is not new");
            assert!(p.has(NodeId(id)));
            let set: Vec<usize> = (0..32).filter(|i| p.reports[*i] != 0).collect();
            assert_eq!(set, vec![usize::from(id / 8)]);
            assert_eq!(p.reports[usize::from(id / 8)], 1 << (id % 8));
            for other in 0..=u8::MAX {
                assert_eq!(p.has(NodeId(other)), other == id);
            }
        }
    }
}
