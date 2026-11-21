//! [`HotIndex`]: the part of a node's index and block table (`tierstore-index`) the hot store
//! uses, as one object-safe trait, so the store is compiled once and holds its index as a
//! trait object, whatever ordered store is under it (`fjall`, `redb`, or the in-memory one of the
//! tests).

use tierstore_format::ids::{BlockId, NodeId};
use tierstore_index::KvIndex;
use tierstore_index::backend::Backend;
use tierstore_index::blocks::BlockInfo;
use tierstore_index::recover::{RecoverError, Recovery, RecoveryReport};
use tierstore_store_api::index::{Index, IndexError};

/// A node's index and block table.
pub trait HotIndex: Index {
    /// Marks the index built, or `indexing`.
    fn set_ready(&self, ready: bool);
    /// The block table's row of `block`.
    fn block(&self, block: BlockId) -> Result<Option<BlockInfo>, IndexError>;
    /// Up to `limit` rows after `after`, in block id order.
    fn blocks(
        &self,
        after: Option<BlockId>,
        limit: usize,
    ) -> Result<Vec<(BlockId, BlockInfo)>, IndexError>;
    /// Writes the block table's row of `block`.
    fn put_block(&self, block: BlockId, info: &BlockInfo) -> Result<(), IndexError>;
    /// The tombstone log cursor of every origin.
    fn tomb_cursors(&self) -> Result<Vec<(NodeId, u64)>, IndexError>;
    /// Runs `recovery` on this index (10.2 steps 1 to 4).
    fn recover(&self, recovery: &Recovery<'_>) -> Result<RecoveryReport, RecoverError>;
}

impl<B: Backend> HotIndex for KvIndex<B> {
    fn set_ready(&self, ready: bool) {
        KvIndex::set_ready(self, ready);
    }

    fn block(&self, block: BlockId) -> Result<Option<BlockInfo>, IndexError> {
        KvIndex::block(self, block)
    }

    fn blocks(
        &self,
        after: Option<BlockId>,
        limit: usize,
    ) -> Result<Vec<(BlockId, BlockInfo)>, IndexError> {
        KvIndex::blocks(self, after, limit)
    }

    fn put_block(&self, block: BlockId, info: &BlockInfo) -> Result<(), IndexError> {
        KvIndex::put_block(self, block, info)
    }

    fn tomb_cursors(&self) -> Result<Vec<(NodeId, u64)>, IndexError> {
        KvIndex::tomb_cursors(self)
    }

    fn recover(&self, recovery: &Recovery<'_>) -> Result<RecoveryReport, RecoverError> {
        recovery.run(self)
    }
}
