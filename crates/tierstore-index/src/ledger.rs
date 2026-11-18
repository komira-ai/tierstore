//! What the index does for the majority-ledger coordination model of 10.2 as written: the
//! per-origin cursors of the replicated tombstone logs, applying a log entry at its position, and
//! the cursors the replay mark reports. Everything that assumes those logs is here or in
//! [`crate::tomblog`]; the rest of the index (entries, the liveness rule, tombstones as facts, the
//! block table, live counts, LIST, sync and the rebuild from trailers) does not use them, so another
//! coordination model replaces this module and the tomb log files and keeps the rest.

use std::collections::BTreeMap;

use tierstore_format::ids::NodeId;
use tierstore_format::tomb::{LogEntry, LogRecord};
use tierstore_store_api::index::IndexError;

use crate::backend::{Backend, Space};
use crate::codec;
use crate::kv::{Applied, KvIndex, Txn, store_err};

const META_CURSOR: &[u8] = b"cursor/";

/// The meta key of `origin`'s tomb log cursor.
fn cursor_key(origin: NodeId) -> Vec<u8> {
    let mut k = META_CURSOR.to_vec();
    k.push(origin.0);
    k
}

impl<B: Backend> KvIndex<B> {
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

    /// The cursors a replay mark reports: per origin, the larger of the mark's and the index's own.
    pub(crate) fn merged_cursors(
        &self,
        marked: &[(NodeId, u64)],
    ) -> Result<Vec<(NodeId, u64)>, IndexError> {
        let mut cursors: BTreeMap<NodeId, u64> = marked.iter().copied().collect();
        for (origin, seq) in self.tomb_cursors()? {
            let c = cursors.entry(origin).or_insert(0);
            *c = (*c).max(seq);
        }
        Ok(cursors.into_iter().collect())
    }
}
