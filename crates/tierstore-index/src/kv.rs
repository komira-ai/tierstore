//! [`KvIndex`]: the frozen [`Index`] of a storage node on a [`Backend`].
//!
//! Every write takes one lock (the index has one writer at a time; the key lock of 10.5 is the
//! caller's) and is one atomic batch of the store: an entry, the live counts it changes and the
//! tombstone rows it touches are committed together, so any prefix of batches a crash leaves is a
//! consistent index and replay from the mark (10.2) completes it. Reads take no lock.
//!
//! The index answers [`IndexError::Indexing`] to `get`, `list` and `put_if_absent` until
//! [`KvIndex::set_ready`]: a node is `indexing` until replay and the tombstone pull of 10.2 are done,
//! and while it is, every write that recovery needs (`delete`, `replay`, `flush_mark`) works.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LogEntry, LogRecord, Tombstone};
use tierstore_store_api::index::{
    DeleteOutcome, EntryLocation, Fact, Index, IndexEntry, IndexError, LiveCount, Lookup,
    PutOutcome, ReplayMark, ReplayStats,
};

use crate::backend::{Backend, BackendError, Op, Pairs, Space};
use crate::codec::{self, Held};
use crate::rules;

/// The version of the store's layout this release writes and reads (in [`Space::Meta`]).
pub const STORE_VERSION: u64 = 1;

const META_VERSION: &[u8] = b"version";
const META_MARK: &[u8] = b"mark";
const META_CURSOR: &[u8] = b"cursor/";
const META_BOOT: &[u8] = b"boot/";

/// Facts applied per batch of the store during a replay.
pub(crate) const REPLAY_BATCH: usize = 1024;

/// Pairs read per scan of the store.
pub(crate) const SCAN_PAGE: usize = 1024;

/// What an index needs to know about its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexConfig {
    /// This node's id (its own drop reports, 10.2).
    pub node: NodeId,
    /// The block geometry (`cold.block_bytes`, `cold.block_reserve`): the blocks of a chain.
    pub geometry: BlockGeometry,
}

/// The index of a storage node on the store `B`.
pub struct KvIndex<B> {
    pub(crate) backend: B,
    pub(crate) cfg: IndexConfig,
    ready: AtomicBool,
    write: Mutex<()>,
    /// Blocks dropped by a block drop (hot-only mode, 10.14): an entry into one is a miss.
    pub(crate) dropped: RwLock<BTreeSet<BlockId>>,
    /// The boot epoch of each node whose promoted copies are hinted; a hint of another epoch is
    /// ignored (10.5).
    boots: RwLock<BTreeMap<NodeId, u64>>,
}

impl<B> std::fmt::Debug for KvIndex<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvIndex")
            .field("cfg", &self.cfg)
            .field("ready", &self.ready.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// The error of a failing store.
pub(crate) fn store_err(e: BackendError) -> IndexError {
    IndexError::Store {
        op: e.op,
        reason: e.to_string(),
    }
}

/// Writes of one batch over the store's state: reads see the batch's own writes first.
pub(crate) struct Txn<'a> {
    backend: &'a dyn Backend,
    writes: BTreeMap<(Space, Vec<u8>), Option<Vec<u8>>>,
}

impl<'a> Txn<'a> {
    pub(crate) fn new(backend: &'a dyn Backend) -> Self {
        Self {
            backend,
            writes: BTreeMap::new(),
        }
    }

    pub(crate) fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, IndexError> {
        match self.writes.get(&(space, key.to_vec())) {
            Some(v) => Ok(v.clone()),
            None => self.backend.get(space, key).map_err(store_err),
        }
    }

    pub(crate) fn put(&mut self, space: Space, key: Vec<u8>, value: Vec<u8>) {
        self.writes.insert((space, key), Some(value));
    }

    pub(crate) fn delete(&mut self, space: Space, key: Vec<u8>) {
        self.writes.insert((space, key), None);
    }

    pub(crate) fn commit(self) -> Result<(), IndexError> {
        if self.writes.is_empty() {
            return Ok(());
        }
        let ops = self
            .writes
            .into_iter()
            .map(|((space, key), v)| match v {
                Some(value) => Op::Put { space, key, value },
                None => Op::Delete { space, key },
            })
            .collect();
        self.backend.commit(ops).map_err(store_err)
    }

    /// What the index holds for `key`.
    pub(crate) fn held(&self, key: &[u8]) -> Result<Option<Held>, IndexError> {
        self.get(Space::Entries, key)?
            .map(|v| codec::decode_held(&v))
            .transpose()
    }

    pub(crate) fn set_held(&mut self, key: &[u8], h: &Held) {
        self.put(Space::Entries, key.to_vec(), codec::encode_held(h));
    }

    /// Adds (`sign` true) or removes an entry's contribution to its blocks' live counts.
    pub(crate) fn count(
        &mut self,
        geometry: BlockGeometry,
        e: &IndexEntry,
        add: bool,
    ) -> Result<(), IndexError> {
        for (block, c) in rules::contrib(geometry, e) {
            let key = codec::block_key(block);
            let mut cur = match self.get(Space::Live, &key)? {
                Some(v) => codec::decode_live(&v)?,
                None => LiveCount::default(),
            };
            if add {
                cur.entries = cur.entries.saturating_add(c.entries);
                cur.bytes = cur.bytes.saturating_add(c.bytes);
            } else {
                cur.entries = cur.entries.saturating_sub(c.entries);
                cur.bytes = cur.bytes.saturating_sub(c.bytes);
            }
            if cur == LiveCount::default() {
                self.delete(Space::Live, key);
            } else {
                self.put(Space::Live, key, codec::encode_live(cur));
            }
        }
        Ok(())
    }
}

fn lock(m: &Mutex<()>) -> MutexGuard<'_, ()> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a fact did.
pub(crate) enum Applied {
    /// It changed the index.
    Changed,
    /// It changed nothing (older than what the index holds, or already applied).
    Unchanged,
}

impl<B: Backend> KvIndex<B> {
    /// The index on `backend`, `indexing` until [`KvIndex::set_ready`]. A new store is stamped
    /// with [`STORE_VERSION`]; a store of another version is refused (the caller then rebuilds
    /// it from the bytes, 10.2 step 3).
    pub fn open(backend: B, cfg: IndexConfig) -> Result<Self, IndexError> {
        match backend.get(Space::Meta, META_VERSION).map_err(store_err)? {
            None => backend
                .commit(vec![Op::Put {
                    space: Space::Meta,
                    key: META_VERSION.to_vec(),
                    value: codec::encode_u64(STORE_VERSION),
                }])
                .map_err(store_err)?,
            Some(v) => {
                let version = codec::decode_u64(&v, "decode store version")?;
                if version != STORE_VERSION {
                    return Err(IndexError::Store {
                        op: "open",
                        reason: format!("the store is of layout version {version}"),
                    });
                }
            }
        }
        let index = Self {
            backend,
            cfg,
            ready: AtomicBool::new(false),
            write: Mutex::new(()),
            dropped: RwLock::new(BTreeSet::new()),
            boots: RwLock::new(BTreeMap::new()),
        };
        index.load_memory()?;
        Ok(index)
    }

    /// Reads the dropped blocks and the boot epochs into memory.
    fn load_memory(&self) -> Result<(), IndexError> {
        let dropped = self.dropped_blocks()?;
        *self.dropped.write().unwrap_or_else(PoisonError::into_inner) = dropped;
        let mut boots = BTreeMap::new();
        for (k, v) in self.scan_prefix(Space::Meta, META_BOOT)? {
            let node = NodeId(*k.last().unwrap_or(&0));
            boots.insert(node, codec::decode_u64(&v, "decode boot epoch")?);
        }
        *self.boots.write().unwrap_or_else(PoisonError::into_inner) = boots;
        Ok(())
    }

    /// The store under the index.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The index's configuration.
    pub fn config(&self) -> IndexConfig {
        self.cfg
    }

    /// Whether the index answers reads (it is built), or `indexing`.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Marks the index built (`true`: replay and the tombstone pull are done) or `indexing`.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    fn check_ready(&self) -> Result<(), IndexError> {
        if self.is_ready() {
            Ok(())
        } else {
            Err(IndexError::Indexing)
        }
    }

    pub(crate) fn lock_write(&self) -> MutexGuard<'_, ()> {
        lock(&self.write)
    }

    pub(crate) fn txn(&self) -> Txn<'_> {
        Txn::new(&self.backend)
    }

    /// Every pair of `space` whose key starts with `prefix`, in order.
    pub(crate) fn scan_prefix(&self, space: Space, prefix: &[u8]) -> Result<Pairs, IndexError> {
        let mut out = Vec::new();
        let mut from = prefix.to_vec();
        let mut bound = Bound::Included(from.as_slice());
        loop {
            let page = self
                .backend
                .scan(space, bound, SCAN_PAGE)
                .map_err(store_err)?;
            let full = page.len() == SCAN_PAGE;
            let mut done = !full;
            for (k, v) in page {
                if !k.starts_with(prefix) {
                    done = true;
                    break;
                }
                from.clone_from(&k);
                out.push((k, v));
            }
            if done {
                return Ok(out);
            }
            bound = Bound::Excluded(from.as_slice());
        }
    }

    /// The boot epoch recorded for `node`'s promoted copies. A change of epoch drops every hint of
    /// that node (10.5): the hints stay in the entries and are no longer returned.
    pub fn set_boot(&self, node: NodeId, boot: u64) -> Result<(), IndexError> {
        let _w = self.lock_write();
        let mut key = META_BOOT.to_vec();
        key.push(node.0);
        self.backend
            .commit(vec![Op::Put {
                space: Space::Meta,
                key,
                value: codec::encode_u64(boot),
            }])
            .map_err(store_err)?;
        self.boots
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(node, boot);
        Ok(())
    }

    /// `entry` as a reader sees it: its promoted copy only while the promoting node's boot epoch
    /// is the one recorded.
    fn visible(&self, mut entry: IndexEntry) -> IndexEntry {
        if let Some(p) = entry.promoted {
            let boots = self.boots.read().unwrap_or_else(PoisonError::into_inner);
            if boots.get(&p.node) != Some(&p.boot) {
                entry.promoted = None;
            }
        }
        entry
    }

    /// Whether the entry's bytes are in a block a block drop removed.
    pub(crate) fn in_dropped(&self, e: &IndexEntry) -> bool {
        let dropped = self.dropped.read().unwrap_or_else(PoisonError::into_inner);
        if dropped.is_empty() {
            return false;
        }
        match e.location {
            EntryLocation::Block { block, .. } => dropped.contains(&block),
            EntryLocation::Chain { .. } => rules::contrib(self.cfg.geometry, e)
                .iter()
                .any(|(b, _)| dropped.contains(b)),
            EntryLocation::Large(_) => false,
        }
    }

    /// The tomb log cursor of `origin`: the last sequence applied, 0 for none.
    pub fn tomb_cursor(&self, origin: NodeId) -> Result<u64, IndexError> {
        self.backend
            .get(Space::Meta, &cursor_key(origin))
            .map_err(store_err)?
            .map_or(Ok(0), |v| codec::decode_u64(&v, "decode tomb cursor"))
    }

    /// Every tomb log cursor the index holds.
    pub fn tomb_cursors(&self) -> Result<Vec<(NodeId, u64)>, IndexError> {
        self.scan_prefix(Space::Meta, META_CURSOR)?
            .into_iter()
            .map(|(k, v)| {
                Ok((
                    NodeId(*k.last().unwrap_or(&0)),
                    codec::decode_u64(&v, "decode tomb cursor")?,
                ))
            })
            .collect()
    }

    /// Applies a record fact: the newest record wins, and a record older than a tombstone of the
    /// key extends that tombstone to name the record's blocks (10.2).
    pub(crate) fn apply_record(
        &self,
        txn: &mut Txn<'_>,
        key: &[u8],
        entry: &IndexEntry,
    ) -> Result<Applied, IndexError> {
        let held = txn.held(key)?;
        let geometry = self.cfg.geometry;
        self.extend_older(txn, key, entry.stamp, &rules::names_of(geometry, entry))?;
        match rules::record(geometry, held.as_ref(), entry) {
            None => Ok(Applied::Unchanged),
            Some(next) => {
                if let Some(Held::Live(cur)) = &held {
                    txn.count(geometry, cur, false)?;
                }
                txn.count(geometry, &next, true)?;
                txn.set_held(key, &Held::Live(next));
                Ok(Applied::Changed)
            }
        }
    }

    /// Applies a relocate of `(generation, stamp)` of `key` to `to`.
    pub(crate) fn apply_relocate(
        &self,
        txn: &mut Txn<'_>,
        key: &[u8],
        generation: tierstore_format::ids::Generation,
        stamp: Stamp,
        to: EntryLocation,
    ) -> Result<Applied, IndexError> {
        let held = txn.held(key)?;
        let geometry = self.cfg.geometry;
        match rules::relocate(geometry, held.as_ref(), generation, stamp, to) {
            None => Ok(Applied::Unchanged),
            Some((cur, next)) => {
                txn.count(geometry, &cur, false)?;
                txn.count(geometry, &next, true)?;
                txn.set_held(key, &Held::Live(next));
                Ok(Applied::Changed)
            }
        }
    }

    /// Applies a tombstone, at `position` in its origin's log when known.
    pub(crate) fn apply_tombstone(
        &self,
        txn: &mut Txn<'_>,
        t: &Tombstone,
        position: Option<(NodeId, u64)>,
    ) -> Result<(DeleteOutcome, bool), IndexError> {
        let held = txn.held(&t.key)?;
        let (outcome, next) = rules::delete(held.as_ref(), t.stamp);
        let mut names = t.names.clone();
        let mut changed = false;
        if let DeleteOutcome::Deleted(e) = &outcome {
            txn.count(self.cfg.geometry, e, false)?;
            names.extend(rules::names_of(self.cfg.geometry, e));
        }
        if held.as_ref() != Some(&next) {
            txn.set_held(&t.key, &next);
            changed = true;
        }
        changed |= self.record_tombstone(txn, &t.key, t.stamp, t.generation, &names, position)?;
        Ok((outcome, changed))
    }

    /// Applies one entry of a tomb log, after checking its position against the origin's cursor.
    pub(crate) fn apply_log(
        &self,
        txn: &mut Txn<'_>,
        rec: &LogRecord,
    ) -> Result<Applied, IndexError> {
        let ckey = cursor_key(rec.origin);
        let cursor = match txn.get(Space::Meta, &ckey)? {
            Some(v) => codec::decode_u64(&v, "decode tomb cursor")?,
            None => 0,
        };
        if rec.seq <= cursor {
            return Ok(Applied::Unchanged);
        }
        if cursor != 0 && rec.seq != cursor + 1 {
            return Err(IndexError::Store {
                op: "replay tomb log",
                reason: format!(
                    "origin {} jumps from sequence {cursor} to {}",
                    rec.origin, rec.seq
                ),
            });
        }
        txn.put(Space::Meta, ckey, codec::encode_u64(rec.seq));
        let changed = match &rec.entry {
            LogEntry::Tombstone(t) => self.apply_tombstone(txn, t, Some((rec.origin, rec.seq)))?.1,
            LogEntry::BlockDrop(d) => self.apply_block_drop(txn, d.block)?,
            LogEntry::TombDrop(d) => {
                self.apply_tomb_report(txn, d.target_origin, d.target_seq, rec.origin)?
            }
            LogEntry::Unknown { .. } => false,
        };
        Ok(if changed {
            Applied::Changed
        } else {
            Applied::Unchanged
        })
    }

    /// Applies facts in batches of [`REPLAY_BATCH`], each batch one commit.
    pub(crate) fn replay_facts(
        &self,
        facts: &mut dyn Iterator<Item = Fact>,
    ) -> Result<ReplayStats, IndexError> {
        let _w = self.lock_write();
        let mut stats = ReplayStats::default();
        let mut dropped_now = Vec::new();
        loop {
            let mut txn = self.txn();
            let mut n = 0;
            for fact in &mut *facts {
                stats.facts += 1;
                let applied = match &fact {
                    Fact::Record { key, entry } => self.apply_record(&mut txn, key, entry)?,
                    Fact::Relocate {
                        key,
                        generation,
                        stamp,
                        to,
                    } => self.apply_relocate(&mut txn, key, *generation, *stamp, *to)?,
                    Fact::Log(rec) => {
                        if let LogEntry::BlockDrop(d) = &rec.entry {
                            dropped_now.push(d.block);
                        }
                        self.apply_log(&mut txn, rec)?
                    }
                };
                if matches!(applied, Applied::Changed) {
                    stats.applied += 1;
                }
                n += 1;
                if n == REPLAY_BATCH {
                    break;
                }
            }
            let more = n == REPLAY_BATCH;
            txn.commit()?;
            self.note_dropped(&dropped_now);
            dropped_now.clear();
            if !more {
                return Ok(stats);
            }
        }
    }

    /// A relocate (a GC rewrite, the relocate of a pack block's entries at its descriptor commit,
    /// 10.5): the entry of `key` moves to `to` only while it holds `generation` at `stamp`; a key
    /// deleted or replaced meanwhile is skipped. Whether the entry moved.
    pub fn relocate(
        &self,
        key: &[u8],
        generation: tierstore_format::ids::Generation,
        stamp: Stamp,
        to: EntryLocation,
    ) -> Result<bool, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let applied = self.apply_relocate(&mut txn, key, generation, stamp, to)?;
        txn.commit()?;
        Ok(matches!(applied, Applied::Changed))
    }

    /// A promoted copy (10.5): the entry of `key` gets the hint `copy` only while it holds
    /// `generation` at `stamp` (a tombstone or a newer record that raced the promotion wins).
    /// Whether the hint was set.
    pub fn promote(
        &self,
        key: &[u8],
        generation: tierstore_format::ids::Generation,
        stamp: Stamp,
        copy: tierstore_store_api::index::HotClean,
    ) -> Result<bool, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        match txn.held(key)? {
            Some(Held::Live(mut e)) if e.generation == generation && e.stamp == stamp => {
                if e.promoted == Some(copy) {
                    return Ok(false);
                }
                e.promoted = Some(copy);
                txn.set_held(key, &Held::Live(e));
                txn.commit()?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Adds blocks to the in-memory set of dropped blocks once their drop is committed.
    pub(crate) fn note_dropped(&self, blocks: &[BlockId]) {
        if blocks.is_empty() {
            return;
        }
        let mut d = self.dropped.write().unwrap_or_else(PoisonError::into_inner);
        d.extend(blocks.iter().copied());
    }
}

/// The meta key of `origin`'s tomb log cursor.
fn cursor_key(origin: NodeId) -> Vec<u8> {
    let mut k = META_CURSOR.to_vec();
    k.push(origin.0);
    k
}

impl<B: Backend> Index for KvIndex<B> {
    fn get(&self, key: &[u8]) -> Result<Lookup, IndexError> {
        self.check_ready()?;
        let held = self
            .backend
            .get(Space::Entries, key)
            .map_err(store_err)?
            .map(|v| codec::decode_held(&v))
            .transpose()?;
        Ok(match held {
            None => Lookup::Absent,
            Some(Held::Tomb(t)) => Lookup::Deleted(t),
            Some(Held::Live(e)) if self.in_dropped(&e) => Lookup::Absent,
            Some(Held::Live(e)) => Lookup::Live(self.visible(e)),
        })
    }

    fn put_if_absent(&self, key: &[u8], entry: IndexEntry) -> Result<PutOutcome, IndexError> {
        self.check_ready()?;
        let _w = self.lock_write();
        let mut txn = self.txn();
        let held = txn.held(key)?;
        let visible = match &held {
            Some(Held::Live(e)) if self.in_dropped(e) => None,
            other => other.as_ref(),
        };
        let outcome = rules::put(visible, &entry);
        if outcome == PutOutcome::Created {
            if let Some(Held::Live(cur)) = &held {
                txn.count(self.cfg.geometry, cur, false)?;
            }
            txn.count(self.cfg.geometry, &entry, true)?;
            txn.set_held(key, &Held::Live(entry));
            txn.commit()?;
        }
        Ok(match outcome {
            PutOutcome::Identical(e) => PutOutcome::Identical(self.visible(e)),
            PutOutcome::Conflict(e) => PutOutcome::Conflict(self.visible(e)),
            other => other,
        })
    }

    fn delete(&self, tombstone: &Tombstone) -> Result<DeleteOutcome, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let (outcome, _) = self.apply_tombstone(&mut txn, tombstone, None)?;
        txn.commit()?;
        Ok(outcome)
    }

    fn list(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, IndexEntry)>, IndexError> {
        self.check_ready()?;
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        let page_len = SCAN_PAGE.min(limit.saturating_mul(2)).max(16);
        let mut cursor: Vec<u8>;
        let mut bound = match after {
            Some(a) if a >= prefix => {
                cursor = a.to_vec();
                Bound::Excluded(cursor.as_slice())
            }
            _ => Bound::Included(prefix),
        };
        loop {
            let page = self
                .backend
                .scan(Space::Entries, bound, page_len)
                .map_err(store_err)?;
            let len = page.len();
            let Some(last) = page.last().map(|(k, _)| k.clone()) else {
                return Ok(out);
            };
            for (k, v) in page {
                if !k.starts_with(prefix) {
                    return Ok(out);
                }
                if let Held::Live(e) = codec::decode_held(&v)?
                    && !self.in_dropped(&e)
                {
                    out.push((k, self.visible(e)));
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
            if len < page_len {
                return Ok(out);
            }
            cursor = last;
            bound = Bound::Excluded(cursor.as_slice());
        }
    }

    fn flush_mark(&self, mark: &ReplayMark) -> Result<(), IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        txn.put(Space::Meta, META_MARK.to_vec(), codec::encode_mark(mark));
        txn.commit()?;
        self.backend.persist().map_err(store_err)
    }

    fn replay_mark(&self) -> Result<Option<ReplayMark>, IndexError> {
        let Some(v) = self
            .backend
            .get(Space::Meta, META_MARK)
            .map_err(store_err)?
        else {
            return Ok(None);
        };
        let mut mark = codec::decode_mark(&v)?;
        let mut cursors: BTreeMap<NodeId, u64> = mark.tomb_cursors.iter().copied().collect();
        for (origin, seq) in self.tomb_cursors()? {
            let c = cursors.entry(origin).or_insert(0);
            *c = (*c).max(seq);
        }
        mark.tomb_cursors = cursors.into_iter().collect();
        Ok(Some(mark))
    }

    fn replay(&self, facts: &mut dyn Iterator<Item = Fact>) -> Result<ReplayStats, IndexError> {
        self.replay_facts(facts)
    }

    fn live(&self, block: BlockId) -> Result<LiveCount, IndexError> {
        self.backend
            .get(Space::Live, &codec::block_key(block))
            .map_err(store_err)?
            .map_or(Ok(LiveCount::default()), |v| codec::decode_live(&v))
    }
}
