//! Helpers shared by the behaviour tests of the index core on the in-memory store: builders of
//! ids, entries, tombstones and facts, and an index that is ready or still `indexing`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers: a failed unwrap or expect is the failure the test reports"
)]

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, Generation, KeyHash, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{
    BlockDrop, LargeFile, LogEntry, LogRecord, Named, TombDrop, Tombstone,
};
use tierstore_index::backend::MemBackend;
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_store_api::index::{
    EntryLocation, Fact, Index, IndexEntry, LiveCount, Lookup, ReplayStats,
};

/// The bytes a block of the test geometry holds alone: 1 MiB blocks with a 256 KiB reserve.
pub const MAX_OBJECT: u64 = (1 << 20) - (1 << 18);

/// The geometry of every test index: a chain spans several blocks from 768 KiB up.
pub fn geometry() -> BlockGeometry {
    BlockGeometry::new(1 << 20, 1 << 18).unwrap()
}

/// The configuration of the test node 1.
pub fn config() -> IndexConfig {
    IndexConfig {
        node: NodeId(1),
        geometry: geometry(),
    }
}

/// An index on a new in-memory store that is still `indexing`.
pub fn indexing() -> KvIndex<MemBackend> {
    KvIndex::open(MemBackend::new(), config()).unwrap()
}

/// An index on a new in-memory store that answers reads.
pub fn ready() -> KvIndex<MemBackend> {
    let index = indexing();
    index.set_ready(true);
    index
}

/// Block `n` of node 1.
pub fn block(n: u64) -> BlockId {
    BlockId::new(NodeId(1), n).unwrap()
}

/// A generation whose 16 bytes are all `n`.
pub fn generation(n: u8) -> Generation {
    Generation([n; 16])
}

/// The stamp whose raw value is `n`.
pub fn stamp(n: u64) -> Stamp {
    Stamp::from_raw(n)
}

/// A record of generation `g` at offset `offset` of block `b`, `len` bytes, stamped `s`.
pub fn entry(g: u8, b: u64, offset: u64, s: u64, len: u64) -> IndexEntry {
    IndexEntry {
        generation: generation(g),
        location: EntryLocation::Block {
            block: block(b),
            offset,
        },
        length: len,
        stamp: stamp(s),
        flags: RecordFlags::default(),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

/// A record of generation `g` in the chain that starts at block `first`.
pub fn chain_entry(g: u8, first: u64, s: u64, len: u64) -> IndexEntry {
    IndexEntry {
        location: EntryLocation::Chain {
            first: block(first),
        },
        ..entry(g, first, 0, s, len)
    }
}

/// The large file `g` of `size` bytes.
pub fn large_file(g: u8, size: u64) -> LargeFile {
    LargeFile {
        key_hash: KeyHash([g; 16]),
        generation: generation(g),
        size,
    }
}

/// A record of generation `g` in a large file.
pub fn large_entry(g: u8, s: u64, size: u64) -> IndexEntry {
    IndexEntry {
        location: EntryLocation::Large(large_file(g, size)),
        ..entry(g, 1, 0, s, size)
    }
}

/// `e` as a relocate record: the flag set.
pub fn flagged(mut e: IndexEntry) -> IndexEntry {
    e.flags = e.flags.with(RecordFlags::RELOCATE);
    e
}

/// The location of offset `offset` in block `b`.
pub fn at(b: u64, offset: u64) -> EntryLocation {
    EntryLocation::Block {
        block: block(b),
        offset,
    }
}

/// A tombstone of `key` at stamp `s` that found generation `g` and names `names`.
pub fn tomb(key: &[u8], g: u8, s: u64, names: Vec<Named>) -> Tombstone {
    Tombstone {
        key: key.to_vec(),
        generation: generation(g),
        stamp: stamp(s),
        names,
    }
}

/// The record fact of `e` under `key`.
pub fn record(key: &[u8], e: IndexEntry) -> Fact {
    Fact::Record {
        key: key.to_vec(),
        entry: e,
    }
}

/// The relocate fact of `(g, s)` of `key` to `to`.
pub fn relocate(key: &[u8], g: u8, s: u64, to: EntryLocation) -> Fact {
    Fact::Relocate {
        key: key.to_vec(),
        generation: generation(g),
        stamp: stamp(s),
        to,
    }
}

/// The log fact at `(origin, seq)` holding `entry`.
pub fn log(origin: u8, seq: u64, entry: LogEntry) -> Fact {
    Fact::Log(LogRecord {
        origin: NodeId(origin),
        seq,
        entry,
    })
}

/// A log fact holding the tombstone `t`.
pub fn log_tomb(origin: u8, seq: u64, t: Tombstone) -> Fact {
    log(origin, seq, LogEntry::Tombstone(t))
}

/// A log fact holding the drop of block `b`.
pub fn log_drop(origin: u8, seq: u64, b: u64) -> Fact {
    log(
        origin,
        seq,
        LogEntry::BlockDrop(BlockDrop {
            block: block(b),
            stamp: stamp(1),
        }),
    )
}

/// A log fact holding the report on the tombstone at `(target_origin, target_seq)`.
pub fn log_report(origin: u8, seq: u64, target_origin: u8, target_seq: u64) -> Fact {
    log(
        origin,
        seq,
        LogEntry::TombDrop(TombDrop {
            target_origin: NodeId(target_origin),
            target_seq,
        }),
    )
}

/// Replays `facts` and returns the stats.
pub fn replay(index: &KvIndex<MemBackend>, facts: Vec<Fact>) -> ReplayStats {
    index.replay(&mut facts.into_iter()).unwrap()
}

/// The live entry of `key`; panics when it holds anything else.
pub fn live_entry(index: &KvIndex<MemBackend>, key: &[u8]) -> IndexEntry {
    match index.get(key).unwrap() {
        Lookup::Live(e) => e,
        other => panic!("expected a live entry, found {other:?}"),
    }
}

/// The live count of block `b`.
pub fn count(index: &KvIndex<MemBackend>, b: u64) -> LiveCount {
    index.live(block(b)).unwrap()
}

/// A live count.
pub fn lc(entries: u64, bytes: u64) -> LiveCount {
    LiveCount { entries, bytes }
}

/// Every permutation of `items`.
pub fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, head.clone());
            out.push(tail);
        }
    }
    out
}
