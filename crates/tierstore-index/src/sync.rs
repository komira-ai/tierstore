//! `index-sync` (`docs/design/fetch-cache-serve.md`, 10.2 step 3 and 10.8): a node that rebuilds
//! its index, or that joins a replica set, takes the entries of the keys it should hold from a
//! peer's index, page by page.
//!
//! A page is the peer's entries in key order, live and tombstone, only for the keys the asking
//! node's replica set includes. The promoted-copy hint of an entry is a cache hint of the
//! peer's node, never synced. The asking node applies a live item as a record fact and a deleted
//! item as a tombstone with no names and no log position, so the newest fact about a key wins as
//! it does everywhere. A peer exports while its own index is being built.

use std::ops::Bound;

use tierstore_format::ids::Generation;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::Tombstone;
use tierstore_store_api::index::{IndexEntry, IndexError, ReplayStats};

use crate::backend::{Backend, Space};
use crate::codec::{self, Dec, Enc, Held};
use crate::kv::{Applied, KvIndex, SCAN_PAGE, store_err};

/// What an entry holds, as synced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncValue {
    /// A live entry (without a promoted-copy hint).
    Live(IndexEntry),
    /// A tombstone entry, with its stamp.
    Deleted(Stamp),
}

/// One entry of a sync page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncItem {
    /// The object key.
    pub key: Vec<u8>,
    /// What the index holds for it.
    pub value: SyncValue,
}

/// One page of a sync stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncPage {
    /// The entries, in key order.
    pub items: Vec<SyncItem>,
    /// The `after` of the following page: the last key scanned, when the page is full and more may
    /// follow.
    pub next: Option<Vec<u8>>,
}

impl<B: Backend> KvIndex<B> {
    /// Up to `limit` entries (live and tombstone) after `after` in key order, only for the keys
    /// `want` accepts (the keys whose replica set includes the asking node). `next` is the last
    /// key scanned when `limit` was reached. A `limit` of 0 gives an empty page. Entries in a
    /// dropped block are not exported. Works while the index is being built.
    pub fn export(
        &self,
        after: Option<&[u8]>,
        limit: usize,
        want: &dyn Fn(&[u8]) -> bool,
    ) -> Result<SyncPage, IndexError> {
        let mut page = SyncPage::default();
        if limit == 0 {
            return Ok(page);
        }
        let mut cursor = after.map(<[u8]>::to_vec);
        loop {
            let bound = cursor.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
            let pairs = self
                .backend
                .scan(Space::Entries, bound, SCAN_PAGE)
                .map_err(store_err)?;
            let full = pairs.len() == SCAN_PAGE;
            for (key, bytes) in pairs {
                if want(&key) {
                    match codec::decode_held(&bytes)? {
                        Held::Live(e) if self.in_dropped(&e) => {}
                        Held::Live(e) => page.items.push(SyncItem {
                            key: key.clone(),
                            value: SyncValue::Live(IndexEntry {
                                promoted: None,
                                ..e
                            }),
                        }),
                        Held::Tomb(stamp) => page.items.push(SyncItem {
                            key: key.clone(),
                            value: SyncValue::Deleted(stamp),
                        }),
                    }
                    if page.items.len() == limit {
                        page.next = Some(key);
                        return Ok(page);
                    }
                }
                cursor = Some(key);
            }
            if !full {
                return Ok(page);
            }
        }
    }

    /// Applies the items of one sync page in one batch: a live item as a record fact (the newest
    /// wins), a deleted item as a tombstone of its key and stamp that names nothing and has no
    /// log position. `facts` of the result counts the items, `applied` those that changed an
    /// entry.
    pub fn import(&self, items: &[SyncItem]) -> Result<ReplayStats, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let mut stats = ReplayStats::default();
        for item in items {
            stats.facts += 1;
            let changed = match &item.value {
                SyncValue::Live(e) => {
                    let entry = IndexEntry {
                        promoted: None,
                        ..e.clone()
                    };
                    matches!(
                        self.apply_record(&mut txn, &item.key, &entry)?,
                        Applied::Changed
                    )
                }
                SyncValue::Deleted(stamp) => {
                    let tombstone = Tombstone {
                        key: item.key.clone(),
                        generation: Generation([0; 16]),
                        stamp: *stamp,
                        names: Vec::new(),
                    };
                    self.apply_tombstone(&mut txn, &tombstone, None)?.1
                }
            };
            if changed {
                stats.applied += 1;
            }
        }
        txn.commit()?;
        Ok(stats)
    }
}

/// The bytes of a page: a version byte, the item count (`u32`), per item the key (`u16` length,
/// bytes) and the entry's value bytes (`u32` length, bytes), then the next cursor (a flag byte and,
/// when set, `u16` length and bytes). Integers are little-endian.
#[must_use]
pub fn encode_page(page: &SyncPage) -> Vec<u8> {
    let mut e = Enc::new();
    e.u32(page.items.len() as u32);
    for item in &page.items {
        let held = match &item.value {
            SyncValue::Live(entry) => Held::Live(entry.clone()),
            SyncValue::Deleted(stamp) => Held::Tomb(*stamp),
        };
        e.u16(item.key.len() as u16)
            .bytes(&item.key)
            .blob(&codec::encode_held(&held));
    }
    match &page.next {
        None => {
            e.u8(0);
        }
        Some(next) => {
            e.u8(1).u16(next.len() as u16).bytes(next);
        }
    }
    e.finish()
}

/// The page `bytes` hold; any malformed input is an error.
pub fn decode_page(bytes: &[u8]) -> Result<SyncPage, IndexError> {
    let mut d = Dec::new(bytes, "decode sync page")?;
    let count = d.u32()?;
    let mut items = Vec::new();
    for _ in 0..count {
        let len = usize::from(d.u16()?);
        let key = d.take(len)?.to_vec();
        let value = match codec::decode_held(d.blob()?)? {
            Held::Live(entry) => SyncValue::Live(entry),
            Held::Tomb(stamp) => SyncValue::Deleted(stamp),
        };
        items.push(SyncItem { key, value });
    }
    let next = match d.u8()? {
        0 => None,
        1 => {
            let len = usize::from(d.u16()?);
            Some(d.take(len)?.to_vec())
        }
        _ => return Err(d.bad("unknown next flag")),
    };
    d.end()?;
    Ok(SyncPage { items, next })
}
