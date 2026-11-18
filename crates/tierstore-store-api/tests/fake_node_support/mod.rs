//! Shared helpers of the fake storage node's tests: ids, objects, envelopes, one call per route.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]
#![allow(
    dead_code,
    reason = "each test file of the fake node uses its own part of these helpers"
)]

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, Rank};
use tierstore_format::ids::{BlockId, DriveId, Generation, NodeId};
use tierstore_format::seal::Ballot;
use tierstore_format::stamp::Stamp;
use tierstore_store_api::fake::{FakeStorageNode, Route};
use tierstore_store_api::hot::ReplicaBatch;
use tierstore_store_api::peer::{GetTarget, PeerBody, PeerError, PeerGet};
use tierstore_store_api::proto::{
    ColdExtent, CommitRecord, Envelope, GetHeaders, HotCopy, KeyCheck, LocateRequest, SealMessage,
    TombPull,
};

/// The node `id`.
pub fn n(id: u8) -> NodeId {
    NodeId(id)
}

/// A generation of sixteen copies of `b`.
pub fn generation(b: u8) -> Generation {
    Generation([b; 16])
}

/// A drive id of sixteen copies of `b`.
pub fn drive(b: u8) -> DriveId {
    DriveId([b; 16])
}

/// The block `counter` of node `node`.
pub fn block(node: u8, counter: u64) -> BlockId {
    BlockId::new(NodeId(node), counter).unwrap()
}

/// A node `id` on a fresh clock.
pub fn node(id: u8) -> (ManualClock, Arc<FakeStorageNode>) {
    let clock = ManualClock::new();
    let node = Arc::new(FakeStorageNode::new(NodeId(id), clock.clone()));
    (clock, node)
}

/// A message from `from` carrying the raw stamp `stamp` and membership epoch 7.
pub fn env<T>(from: u8, stamp: u64, body: T) -> Envelope<T> {
    Envelope {
        from: NodeId(from),
        stamp: Stamp::from_raw(stamp),
        mem_epoch: 7,
        body,
    }
}

/// A hot copy of an object of `length` bytes at `offset` of `file`.
pub fn hot_copy(file: &str, offset: u64, length: u64) -> HotCopy {
    HotCopy {
        node: NodeId(1),
        file: file.to_owned(),
        offset,
        length,
        promoted: false,
    }
}

/// A cold extent: `length` bytes at `object_offset` of the object and `block_offset` of the block,
/// on shard `shard` of `drive`.
pub fn extent(
    block: BlockId,
    object_offset: u64,
    length: u64,
    block_offset: u64,
    drive: DriveId,
    shard: &str,
) -> ColdExtent {
    ColdExtent {
        block,
        object_offset,
        length,
        block_offset,
        node: NodeId(1),
        drive,
        shard: shard.to_owned(),
        reconstructor: NodeId(2),
        sources: Vec::new(),
        margin: 0,
    }
}

/// The headers of a GET with no `If-Match`.
pub fn headers(generation: Option<Generation>) -> GetHeaders {
    GetHeaders {
        generation,
        class: Class::Demand,
        job: JobId(1),
        rank: Rank(0),
    }
}

/// A GET of `range` of `target`.
pub fn get(target: GetTarget, offset: u64, len: u64, generation: Option<Generation>) -> PeerGet {
    PeerGet {
        target,
        range: ByteRange { offset, len },
        headers: headers(generation),
        report: None,
    }
}

/// A GET of a hot file.
pub fn hot(file: &str) -> GetTarget {
    GetTarget::Hot {
        file: file.to_owned(),
    }
}

/// A GET of a cold shard.
pub fn cold(drive: DriveId, shard: &str) -> GetTarget {
    GetTarget::Cold {
        drive,
        shard: shard.to_owned(),
    }
}

/// The bytes of a GET's body.
pub async fn body_bytes(body: PeerBody) -> Vec<u8> {
    let chunks: Vec<Result<Bytes, PeerError>> = body.body.collect().await;
    chunks.into_iter().flat_map(|c| c.unwrap()).collect()
}

/// A replication batch from writer `writer`.
pub fn batch(
    segment: BlockId,
    ballot: Ballot,
    start: u64,
    running_crc: u32,
    bytes: &[u8],
) -> Envelope<ReplicaBatch> {
    env(
        ballot.node.0,
        1,
        ReplicaBatch {
            segment,
            ballot,
            start,
            running_crc,
            stamp: Stamp::from_raw(1),
            bytes: Bytes::copy_from_slice(bytes),
        },
    )
}

/// A ballot of `round` under node `node`.
pub fn ballot(round: u32, node: u8) -> Ballot {
    Ballot {
        round,
        node: NodeId(node),
    }
}

/// One call of `route` with a minimal request; the answer is dropped, the error kept.
pub async fn call(node: &FakeStorageNode, route: Route) -> Result<(), PeerError> {
    match route {
        Route::Locate => node.locate(LocateRequest { keys: vec![] }).await.map(drop),
        Route::Get => node.get(get(hot("f"), 0, 0, None)).await.map(drop),
        Route::Replicate => node
            .replicate(batch(block(1, 1), ballot(1, 1), 0, 0, b""))
            .await
            .map(drop),
        Route::Seal => node
            .seal(env(
                1,
                1,
                SealMessage::Prepare {
                    segment: block(1, 1),
                    ballot: ballot(1, 1),
                },
            ))
            .await
            .map(drop),
        Route::KeyCheck => node
            .key_check(env(
                1,
                1,
                KeyCheck {
                    key: "k".to_owned(),
                },
            ))
            .await
            .map(drop),
        Route::Commit => node
            .commit(env(
                1,
                1,
                CommitRecord::Free {
                    bytes: tierstore_store_api::proto::HexBytes(Vec::new()),
                },
            ))
            .await
            .map(drop),
        Route::TombPull => node
            .tomb_pull(TombPull {
                origin: NodeId(1),
                after: 0,
                limit: 1,
            })
            .await
            .map(drop),
        Route::Ping => node.ping().await.map(drop),
    }
}

/// Every route of the peer service.
pub const ROUTES: [Route; 8] = [
    Route::Locate,
    Route::Get,
    Route::Replicate,
    Route::Seal,
    Route::KeyCheck,
    Route::Commit,
    Route::TombPull,
    Route::Ping,
];
