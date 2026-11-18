//! `ColdStore`: the cold tier of one storage node (10.4, 10.5). T-S7 implements it for the
//! replicas mode, T-S8 for erasure coding.
//!
//! Only sealed, immutable blocks reach the cold tier; every write is a whole shard file written
//! sequentially, every delete the unlink of a whole shard file after a committed stripe free.

use bytes::Bytes;
use futures::stream::BoxStream;
use tierstore_fetch_api::ids::{ByteRange, JobId, Rank};
use tierstore_fetch_api::upstream::DriveSignals;
use tierstore_format::footer::ShardFooter;
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, DriveId, NodeId};
use tierstore_format::names::ColdName;

use crate::sched::DriveClass;

/// One shard (or copy) file to write on one of this node's drives.
pub struct ShardWrite {
    /// The drive.
    pub drive: DriveId,
    /// The file's final name.
    pub name: ColdName,
    /// The data region's bytes, streamed once (teed from the NVMe and the encoder).
    pub body: BoxStream<'static, Result<Bytes, std::io::Error>>,
    /// The footer the coordinator computed; the writer checks its crc table and blake3 against
    /// what it received before it writes the footer and renames.
    pub footer: ShardFooter,
    /// `write` for destage and rebuild; `prepare` for the rebuild of a stripe with no spare.
    pub class: DriveClass,
}

impl std::fmt::Debug for ShardWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardWrite")
            .field("drive", &self.drive)
            .field("name", &self.name)
            .field("footer", &self.footer)
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

/// A shard file that is durable and renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardWritten {
    /// The drive.
    pub drive: DriveId,
    /// The file.
    pub name: ColdName,
    /// The bytes of its data region.
    pub data_len: u64,
}

/// A range read of one shard (or copy) file on one of this node's drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeRead {
    /// The drive.
    pub drive: DriveId,
    /// The file.
    pub name: ColdName,
    /// The bytes of the file's data region.
    pub range: ByteRange,
    /// The class.
    pub class: DriveClass,
    /// The job.
    pub job: JobId,
    /// The rank.
    pub rank: Rank,
}

/// One source of a degraded read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DegradedSource {
    /// The node holding the shard.
    pub node: NodeId,
    /// The drive.
    pub drive: DriveId,
    /// The shard's index in the stripe.
    pub index: u8,
}

/// A degraded read: the range of a block decoded from `k` other shards (10.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedRead {
    /// The block.
    pub block: BlockId,
    /// The bytes of the block wanted (rounded to 4 KiB by the reconstructor).
    pub range: ByteRange,
    /// The `k` shards to read, live data siblings first.
    pub sources: Vec<DegradedSource>,
    /// The class.
    pub class: DriveClass,
    /// The job.
    pub job: JobId,
    /// The rank.
    pub rank: Rank,
}

/// A read's response.
pub struct ColdRead {
    /// The serving drive's signals (`x-ts-queue-wait-ms`, `x-ts-service-ms`, state).
    pub signals: DriveSignals,
    /// The bytes.
    pub body: BoxStream<'static, Result<Bytes, ColdError>>,
}

impl std::fmt::Debug for ColdRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColdRead")
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

/// A stripe free applied on this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeApplied {
    /// Shard files unlinked.
    pub unlinked: u32,
}

/// Why a cold operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ColdError {
    /// The drive refused the request (queue full or its state): `503`.
    #[error("drive {drive}: {refused}")]
    Refused {
        /// The drive.
        drive: DriveId,
        /// Why.
        refused: crate::sched::Refused,
    },
    /// The drive failed it after admission.
    #[error("drive {drive}: {error}")]
    Drive {
        /// The drive.
        drive: DriveId,
        /// What happened.
        error: crate::sched::DriveError,
    },
    /// The received bytes do not match the coordinator's footer (crc table or blake3): the `.up`
    /// is unlinked.
    #[error("drive {drive}: {name:?} does not match its footer")]
    FooterMismatch {
        /// The drive.
        drive: DriveId,
        /// The file.
        name: ColdName,
    },
    /// A read's bytes failed their cell crc32c (bitrot): the shard is quarantined.
    #[error("drive {drive}: {name:?} failed its cell crc at {offset}")]
    Corrupt {
        /// The drive.
        drive: DriveId,
        /// The file.
        name: ColdName,
        /// The offset of the bad cell.
        offset: u64,
    },
    /// No such file (a freed stripe): `404`, which the engine treats as a lost location.
    #[error("drive {drive}: no {name:?}")]
    NotFound {
        /// The drive.
        drive: DriveId,
        /// The file.
        name: ColdName,
    },
    /// Fewer than `k` sources could be read: the range is unreadable for now.
    #[error("block {block}: fewer than k shards readable")]
    Unreadable {
        /// The block.
        block: BlockId,
    },
    /// The free is refused: the stripe's descriptor is at a later revision than the free counted.
    #[error("stale free of {0:?}")]
    StaleFree(StripeFree),
}

/// The cold store of one node.
///
/// Section 14 rows: "Block, stripe and footer"; "Destage commit is atomic under a crash at every
/// step"; "Pack assembly and stripe formation"; "Replicated-to-EC conversion"; "Degraded read";
/// "Rebuild of a lost drive ... of a lost node"; "Cold-copy re-replication"; "Scrub"; "Delete and
/// GC" (the stripe free and its unlink after the commit); "Pack write on a cold drive: crash
/// before rename leaves only `.up`"; "`digest=none` object: the record or cell crc detects a
/// flipped cell on the serving node".
#[async_trait::async_trait]
pub trait ColdStore: Send + Sync {
    /// Writes one shard or copy file: `.up`, the bytes, the checks, the footer, `fsync`, rename,
    /// directory `fsync`. A crash leaves only the `.up`.
    async fn write_shard(&self, write: ShardWrite) -> Result<ShardWritten, ColdError>;

    /// Reads a range of one shard or copy file through the drive's scheduler.
    async fn read_range(&self, read: RangeRead) -> Result<ColdRead, ColdError>;

    /// Serves a degraded read as the reconstructor: `k` range reads, admitted as `k` stream
    /// charges on the source drives' schedulers, decoded.
    async fn degraded_read(&self, read: DegradedRead) -> Result<ColdRead, ColdError>;

    /// Applies a committed stripe free: unlinks this node's shard files of the stripe and the
    /// `blk/*.idx` of its blocks. Only ever called after the commit.
    async fn free_stripe(&self, commit: StripeFree) -> Result<FreeApplied, ColdError>;
}
