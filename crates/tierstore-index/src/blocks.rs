//! The block table (10.2), in the same store as the index: `block_id -> {state, length, hot
//! copies, stripe, shard index}` and `stripe_id -> {descriptor, free}`.
//!
//! The majority commits that decide a seal, a descriptor or a free are the hot and cold tiers'
//! (10.3, 10.5); the table records their outcome on this node. A descriptor is kept at its highest
//! revision; once a stripe's free is recorded no descriptor of it is taken again (a rebuild commit
//! for a freed stripe is refused, 10.5 step 5).

use std::ops::Bound;

use tierstore_format::descriptor::StripeDescriptor;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, NodeId, StripeId};
use tierstore_format::tomb::Named;
use tierstore_store_api::index::IndexError;

use crate::backend::{Backend, Space};
use crate::codec::{self, Dec, Enc};
use crate::kv::{KvIndex, Txn, store_err};

/// The state of a block (10.2, 10.3, 10.5, 10.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockState {
    /// An open segment.
    Open,
    /// A sealed segment or block on the hot tier, not yet committed to the cold tier.
    Sealed,
    /// Committed to the cold tier (its hot copies, if any, are clean).
    Cold,
    /// Its stripe's free is committed.
    Freed,
    /// Dropped by a block drop (hot-only mode): its records are misses.
    Dropped,
}

impl BlockState {
    fn code(self) -> u8 {
        match self {
            Self::Open => 1,
            Self::Sealed => 2,
            Self::Cold => 3,
            Self::Freed => 4,
            Self::Dropped => 5,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Open,
            2 => Self::Sealed,
            3 => Self::Cold,
            4 => Self::Freed,
            5 => Self::Dropped,
            _ => return None,
        })
    }
}

/// A row of the block table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInfo {
    /// The block's state.
    pub state: BlockState,
    /// Its length in bytes (records and trailer once sealed).
    pub length: u64,
    /// The nodes holding a hot copy (`seg/<block_id>.seg` on each).
    pub hot_copies: Vec<NodeId>,
    /// The stripe it was destaged in, once committed.
    pub stripe: Option<StripeId>,
    /// Its shard index in that stripe.
    pub shard: Option<u8>,
}

impl BlockInfo {
    /// A row of `state` with nothing else known.
    #[must_use]
    pub fn new(state: BlockState) -> Self {
        Self {
            state,
            length: 0,
            hot_copies: Vec::new(),
            stripe: None,
            shard: None,
        }
    }
}

fn encode_block(b: &BlockInfo) -> Vec<u8> {
    let mut e = Enc::new();
    e.u8(b.state.code()).u64(b.length);
    e.u32(b.hot_copies.len() as u32);
    for n in &b.hot_copies {
        e.u8(n.0);
    }
    match (b.stripe, b.shard) {
        (Some(s), Some(i)) => {
            e.u8(1).u64(s.raw()).u8(i);
        }
        (Some(s), None) => {
            e.u8(2).u64(s.raw());
        }
        (None, _) => {
            e.u8(0);
        }
    }
    e.finish()
}

fn decode_block(buf: &[u8]) -> Result<BlockInfo, IndexError> {
    let mut d = Dec::new(buf, "decode block row")?;
    let state = BlockState::from_code(d.u8()?).ok_or_else(|| d.bad("unknown block state"))?;
    let length = d.u64()?;
    let mut hot_copies = Vec::new();
    for _ in 0..d.u32()? {
        hot_copies.push(NodeId(d.u8()?));
    }
    let (stripe, shard) = match d.u8()? {
        0 => (None, None),
        1 => (Some(StripeId::from_raw(d.u64()?)), Some(d.u8()?)),
        2 => (Some(StripeId::from_raw(d.u64()?)), None),
        _ => return Err(d.bad("unknown stripe tag")),
    };
    d.end()?;
    Ok(BlockInfo {
        state,
        length,
        hot_copies,
        stripe,
        shard,
    })
}

/// A stripe's row: its descriptor at the highest revision taken, and its free once committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRow {
    /// The descriptor.
    pub descriptor: StripeDescriptor,
    /// The committed free, if any.
    pub free: Option<StripeFree>,
}

fn encode_stripe(r: &StripeRow) -> Result<Vec<u8>, IndexError> {
    let desc = r.descriptor.encode().map_err(|e| IndexError::Store {
        op: "encode stripe row",
        reason: e.to_string(),
    })?;
    let mut e = Enc::new();
    e.blob(&desc);
    match &r.free {
        None => {
            e.u8(0);
        }
        Some(f) => {
            e.u8(1).blob(&f.encode());
        }
    }
    Ok(e.finish())
}

fn decode_stripe(buf: &[u8]) -> Result<StripeRow, IndexError> {
    let mut d = Dec::new(buf, "decode stripe row")?;
    let bad = |e: tierstore_format::FormatError| IndexError::Store {
        op: "decode stripe row",
        reason: e.to_string(),
    };
    let descriptor = StripeDescriptor::decode(d.blob()?).map_err(bad)?;
    let free = match d.u8()? {
        0 => None,
        1 => Some(StripeFree::decode(d.blob()?).map_err(bad)?),
        _ => return Err(d.bad("unknown free tag")),
    };
    d.end()?;
    Ok(StripeRow { descriptor, free })
}

/// What [`KvIndex::put_stripe`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripePut {
    /// The descriptor is the stripe's row now.
    Stored,
    /// The table holds this revision or a later one.
    Stale {
        /// The revision held.
        held: u32,
    },
    /// The stripe's free is committed: the descriptor is refused.
    Freed,
}

impl<B: Backend> KvIndex<B> {
    /// The blocks the table marks dropped.
    pub(crate) fn dropped_blocks(&self) -> Result<std::collections::BTreeSet<BlockId>, IndexError> {
        let mut out = std::collections::BTreeSet::new();
        for (b, info) in self.all_blocks()? {
            if info.state == BlockState::Dropped {
                out.insert(b);
            }
        }
        Ok(out)
    }

    fn all_blocks(&self) -> Result<Vec<(BlockId, BlockInfo)>, IndexError> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self.blocks(after, crate::kv::SCAN_PAGE)?;
            let full = page.len() == crate::kv::SCAN_PAGE;
            after = page.last().map(|(b, _)| *b);
            out.extend(page);
            if !full {
                return Ok(out);
            }
        }
    }

    /// Sets the row of `block`.
    pub fn put_block(&self, block: BlockId, info: &BlockInfo) -> Result<(), IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        txn.put(Space::Blocks, codec::block_key(block), encode_block(info));
        txn.commit()?;
        let mut d = self
            .dropped
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if info.state == BlockState::Dropped {
            d.insert(block);
        } else {
            d.remove(&block);
        }
        Ok(())
    }

    /// The row of `block`.
    pub fn block(&self, block: BlockId) -> Result<Option<BlockInfo>, IndexError> {
        self.backend
            .get(Space::Blocks, &codec::block_key(block))
            .map_err(store_err)?
            .map(|v| decode_block(&v))
            .transpose()
    }

    /// Up to `limit` rows after `after`, in block id order.
    pub fn blocks(
        &self,
        after: Option<BlockId>,
        limit: usize,
    ) -> Result<Vec<(BlockId, BlockInfo)>, IndexError> {
        let key = after.map(codec::block_key);
        let bound = key.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        self.backend
            .scan(Space::Blocks, bound, limit)
            .map_err(store_err)?
            .into_iter()
            .map(|(k, v)| Ok((BlockId::from_raw(codec::id_of(&k)?), decode_block(&v)?)))
            .collect()
    }

    /// Applies a block drop (hot-only mode, 10.14): the block is `Dropped`, its records are misses,
    /// and the drop counts as this node's free of the block. Whether it changed the table.
    pub(crate) fn apply_block_drop(
        &self,
        txn: &mut Txn<'_>,
        block: BlockId,
    ) -> Result<bool, IndexError> {
        let key = codec::block_key(block);
        let mut info = match txn.get(Space::Blocks, &key)? {
            Some(v) => decode_block(&v)?,
            None => BlockInfo::new(BlockState::Dropped),
        };
        let was = txn.get(Space::Blocks, &key)?.is_some() && info.state == BlockState::Dropped;
        info.state = BlockState::Dropped;
        txn.put(Space::Blocks, key.clone(), encode_block(&info));
        txn.put(
            Space::Freed,
            codec::named_key(&Named::Block(block)),
            Vec::new(),
        );
        txn.delete(Space::Live, key);
        Ok(!was)
    }

    /// Takes `desc` as the stripe's descriptor unless the table holds its revision or a later one,
    /// or the stripe's free; the data blocks then point at the stripe, `Cold`.
    pub fn put_stripe(&self, desc: &StripeDescriptor) -> Result<StripePut, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let key = codec::stripe_key(desc.stripe);
        let held = txn
            .get(Space::Stripes, &key)?
            .map(|v| decode_stripe(&v))
            .transpose()?;
        if let Some(row) = &held {
            if row.free.is_some() {
                return Ok(StripePut::Freed);
            }
            if row.descriptor.revision >= desc.revision {
                return Ok(StripePut::Stale {
                    held: row.descriptor.revision,
                });
            }
        }
        let row = StripeRow {
            descriptor: desc.clone(),
            free: None,
        };
        txn.put(Space::Stripes, key, encode_stripe(&row)?);
        for (shard, (block, length)) in desc.blocks.iter().enumerate() {
            let bkey = codec::block_key(*block);
            let mut info = match txn.get(Space::Blocks, &bkey)? {
                Some(v) => decode_block(&v)?,
                None => BlockInfo::new(BlockState::Cold),
            };
            info.state = BlockState::Cold;
            info.length = *length;
            info.stripe = Some(desc.stripe);
            info.shard = u8::try_from(shard).ok();
            txn.put(Space::Blocks, bkey, encode_block(&info));
        }
        txn.commit()?;
        Ok(StripePut::Stored)
    }

    /// The row of `stripe`.
    pub fn stripe(&self, stripe: StripeId) -> Result<Option<StripeRow>, IndexError> {
        self.backend
            .get(Space::Stripes, &codec::stripe_key(stripe))
            .map_err(store_err)?
            .map(|v| decode_stripe(&v))
            .transpose()
    }

    /// Records a committed stripe free: the stripe's data blocks are `Freed`. Whether the table
    /// held the stripe (a free of a stripe it never saw is recorded once its descriptor arrives:
    /// the descriptor is then refused, so the caller keeps the free and retries). The unlink of
    /// each shard is the caller's, which then calls [`KvIndex::apply_free`] for each data block.
    pub fn apply_stripe_free(&self, free: &StripeFree) -> Result<bool, IndexError> {
        let _w = self.lock_write();
        let mut txn = self.txn();
        let key = codec::stripe_key(free.stripe);
        let Some(mut row) = txn
            .get(Space::Stripes, &key)?
            .map(|v| decode_stripe(&v))
            .transpose()?
        else {
            return Ok(false);
        };
        for (block, _) in &row.descriptor.blocks {
            let bkey = codec::block_key(*block);
            let mut info = match txn.get(Space::Blocks, &bkey)? {
                Some(v) => decode_block(&v)?,
                None => BlockInfo::new(BlockState::Freed),
            };
            info.state = BlockState::Freed;
            txn.put(Space::Blocks, bkey, encode_block(&info));
        }
        row.free = Some(*free);
        txn.put(Space::Stripes, key, encode_stripe(&row)?);
        txn.commit()?;
        Ok(true)
    }
}
