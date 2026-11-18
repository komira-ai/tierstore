//! The tombstones a node holds until their drop, and the one drop rule (10.2).
//!
//! Every node keeps every tombstone, whether or not it indexed the key. A tombstone names every
//! block or large file that may still list an older record of its key; the index adds to that
//! list whenever it learns of one (the entry it deletes and the blocks it was relocated from; a
//! record older than the tombstone met in a replay, a sync or a newly sealed or committed block).
//!
//! **The drop rule.** A node reports a tombstone once everything it names is freed on that node
//! (a stripe free applied, a hot block or large file unlinked, a block drop): the caller appends a
//! `tomb-drop(origin, seq)` of [`KvIndex::pending_reports`] to its own log, and the report comes
//! back to every node as a log entry. A node deletes the tombstone only when every node not `out`
//! has reported it ([`KvIndex::droppable`], [`KvIndex::drop_tombstone`]). A name of a kind this
//! release does not know is never freed here, so a tombstone that carries one is kept.

use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::Named;
use tierstore_format::trailer::TrailerEntry;
use tierstore_store_api::index::IndexError;

use crate::backend::{Backend, Space};
use crate::codec::{self, Held, PosRow, TombRow};
use crate::kv::{KvIndex, SCAN_PAGE, Txn, store_err};

/// A tombstone the index holds, for inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TombInfo {
    /// The key deleted.
    pub key: Vec<u8>,
    /// The tombstone's stamp.
    pub stamp: Stamp,
    /// The generation the deleting writer found.
    pub generation: Generation,
    /// What it names, sorted.
    pub names: Vec<Named>,
    /// Its position in its origin's log, once known.
    pub position: Option<(NodeId, u64)>,
    /// The nodes that reported its drop.
    pub reports: Vec<NodeId>,
}

fn rows_of(txn: &Txn<'_>, key: &[u8]) -> Result<Vec<(Stamp, TombRow)>, IndexError> {
    txn.get(Space::Tombs, key)?
        .map_or(Ok(Vec::new()), |v| codec::decode_tombs(&v))
}

fn put_rows(txn: &mut Txn<'_>, key: &[u8], rows: &[(Stamp, TombRow)]) {
    if rows.is_empty() {
        txn.delete(Space::Tombs, key.to_vec());
    } else {
        txn.put(Space::Tombs, key.to_vec(), codec::encode_tombs(rows));
    }
}

/// Adds `names` to `row`'s names; whether any was new.
fn add_names(row: &mut TombRow, names: &[Named]) -> bool {
    let mut added = false;
    for n in names {
        if let Err(at) = row.names.binary_search(n) {
            row.names.insert(at, n.clone());
            added = true;
        }
    }
    added
}

fn pos_row(txn: &Txn<'_>, origin: NodeId, seq: u64) -> Result<PosRow, IndexError> {
    txn.get(Space::TombPos, &codec::pos_key(origin, seq))?
        .map_or(Ok(PosRow::default()), |v| codec::decode_pos(&v))
}

impl<B: Backend> KvIndex<B> {
    /// Records the tombstone `(key, stamp)` with `names` (added to any it holds) and, when given,
    /// its log position. Whether anything changed.
    pub(crate) fn record_tombstone(
        &self,
        txn: &mut Txn<'_>,
        key: &[u8],
        stamp: Stamp,
        generation: Generation,
        names: &[Named],
        position: Option<(NodeId, u64)>,
    ) -> Result<bool, IndexError> {
        let mut rows = rows_of(txn, key)?;
        let at = match rows.binary_search_by_key(&stamp, |(s, _)| *s) {
            Ok(at) => at,
            Err(at) => {
                rows.insert(
                    at,
                    (
                        stamp,
                        TombRow {
                            generation,
                            names: Vec::new(),
                            position: None,
                        },
                    ),
                );
                at
            }
        };
        let fresh = rows[at].1.names.is_empty() && rows[at].1.position.is_none();
        let mut changed = add_names(&mut rows[at].1, names) || fresh;
        if let (Some((origin, seq)), None) = (position, rows[at].1.position) {
            rows[at].1.position = Some((origin, seq));
            let mut p = pos_row(txn, origin, seq)?;
            p.target = Some((key.to_vec(), stamp));
            txn.put(
                Space::TombPos,
                codec::pos_key(origin, seq),
                codec::encode_pos(&p),
            );
            changed = true;
        }
        put_rows(txn, key, &rows);
        Ok(changed)
    }

    /// Extends every tombstone of `key` above `stamp` (a record at `stamp` is older than it) to
    /// name `names`. Whether any was extended.
    pub(crate) fn extend_older(
        &self,
        txn: &mut Txn<'_>,
        key: &[u8],
        stamp: Stamp,
        names: &[Named],
    ) -> Result<bool, IndexError> {
        let mut rows = rows_of(txn, key)?;
        let mut changed = false;
        for (_, row) in rows.iter_mut().filter(|(t, _)| *t > stamp) {
            changed |= add_names(row, names);
        }
        if changed {
            put_rows(txn, key, &rows);
        }
        Ok(changed)
    }

    /// Records `reporter`'s drop report of the tombstone at `(origin, seq)`.
    pub(crate) fn apply_tomb_report(
        &self,
        txn: &mut Txn<'_>,
        origin: NodeId,
        seq: u64,
        reporter: NodeId,
    ) -> Result<bool, IndexError> {
        let mut p = pos_row(txn, origin, seq)?;
        if !p.add(reporter) {
            return Ok(false);
        }
        txn.put(
            Space::TombPos,
            codec::pos_key(origin, seq),
            codec::encode_pos(&p),
        );
        Ok(true)
    }

    /// At a seal or a descriptor commit (10.2): every tombstone of a key that `entries` (the new
    /// block's trailer) lists with an older stamp is extended to name `block`. The number of
    /// tombstones extended.
    pub fn extend_tombstones(
        &self,
        block: BlockId,
        entries: &[TrailerEntry],
    ) -> Result<u64, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let mut n = 0;
        for e in entries {
            if self.extend_older(&mut txn, &e.key, e.stamp, &[Named::Block(block)])? {
                n += 1;
            }
        }
        txn.commit()?;
        Ok(n)
    }

    /// A tombstone applied late, checked against the trailers and `blk/*.idx` files this node
    /// holds (10.2): the caller found a record of `key` at `record_stamp` in `names`; every
    /// tombstone of the key above that stamp is extended to name them. Whether any was extended.
    pub fn extend_tombstone(
        &self,
        key: &[u8],
        record_stamp: Stamp,
        names: &[Named],
    ) -> Result<bool, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let changed = self.extend_older(&mut txn, key, record_stamp, names)?;
        txn.commit()?;
        Ok(changed)
    }

    /// Records that this node has applied the free of `name`: a stripe free applied (each of its
    /// data blocks), a hot block or large file unlinked, or a block drop.
    pub fn apply_free(&self, name: &Named) -> Result<(), IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        txn.put(Space::Freed, codec::named_key(name), Vec::new());
        if let Named::Block(b) = name {
            txn.delete(Space::Live, codec::block_key(*b));
        }
        txn.commit()
    }

    /// Whether this node has applied the free of `name`.
    pub fn is_freed(&self, name: &Named) -> Result<bool, IndexError> {
        if matches!(name, Named::Unknown { .. }) {
            return Ok(false);
        }
        Ok(self
            .backend
            .get(Space::Freed, &codec::named_key(name))
            .map_err(store_err)?
            .is_some())
    }

    /// Every tombstone, in key and stamp order, with the drop reports of its position.
    pub fn tombstones(&self) -> Result<Vec<TombInfo>, IndexError> {
        let mut out = Vec::new();
        for (key, v) in self.scan_prefix(Space::Tombs, &[])? {
            for (stamp, row) in codec::decode_tombs(&v)? {
                let reports = match row.position {
                    None => Vec::new(),
                    Some((o, s)) => {
                        let p = self
                            .backend
                            .get(Space::TombPos, &codec::pos_key(o, s))
                            .map_err(store_err)?
                            .map_or(Ok(PosRow::default()), |v| codec::decode_pos(&v))?;
                        (0..=u8::MAX).map(NodeId).filter(|n| p.has(*n)).collect()
                    }
                };
                out.push(TombInfo {
                    key: key.clone(),
                    stamp,
                    generation: row.generation,
                    names: row.names,
                    position: row.position,
                    reports,
                });
            }
        }
        Ok(out)
    }

    /// The tombstones whose names this node has all freed and whose drop it has not reported yet
    /// (its own report is not among the reports of the position): the `tomb-drop` records the
    /// caller appends to this node's log. A position whose report comes back as a log entry is
    /// no longer listed, so asking twice before that gives the same list.
    pub fn pending_reports(&self) -> Result<Vec<(NodeId, u64)>, IndexError> {
        let mut out = Vec::new();
        for t in self.tombstones()? {
            let Some(pos) = t.position else { continue };
            if t.reports.contains(&self.cfg.node) {
                continue;
            }
            let mut all = true;
            for n in &t.names {
                if !self.is_freed(n)? {
                    all = false;
                    break;
                }
            }
            if all {
                out.push(pos);
            }
        }
        Ok(out)
    }

    /// The positions of the tombstones every node of `not_out` has reported: they may be dropped.
    pub fn droppable(&self, not_out: &[NodeId]) -> Result<Vec<(NodeId, u64)>, IndexError> {
        let mut out = Vec::new();
        let mut from: Vec<u8> = Vec::new();
        let mut first = true;
        loop {
            let bound = if first {
                std::ops::Bound::Unbounded
            } else {
                std::ops::Bound::Excluded(from.as_slice())
            };
            let page = self
                .backend
                .scan(Space::TombPos, bound, SCAN_PAGE)
                .map_err(store_err)?;
            let n = page.len();
            for (k, v) in page {
                let p = codec::decode_pos(&v)?;
                if p.target.is_some() && not_out.iter().all(|node| p.has(*node)) {
                    out.push(codec::pos_of(&k)?);
                }
                from = k;
            }
            first = false;
            if n < SCAN_PAGE {
                return Ok(out);
            }
        }
    }

    /// Deletes the tombstone at `(origin, seq)`: its row, its names, its position, and the key's
    /// tombstone entry when it is this tombstone's (the key is then absent). Whether a tombstone
    /// was there.
    pub fn drop_tombstone(&self, origin: NodeId, seq: u64) -> Result<bool, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let p = pos_row(&txn, origin, seq)?;
        let Some((key, stamp)) = p.target else {
            return Ok(false);
        };
        let mut rows = rows_of(&txn, &key)?;
        if let Ok(at) = rows.binary_search_by_key(&stamp, |(s, _)| *s) {
            rows.remove(at);
        }
        put_rows(&mut txn, &key, &rows);
        txn.delete(Space::TombPos, codec::pos_key(origin, seq));
        if txn.held(&key)? == Some(Held::Tomb(stamp)) {
            txn.delete(Space::Entries, key.clone());
        }
        txn.commit()?;
        Ok(true)
    }
}
