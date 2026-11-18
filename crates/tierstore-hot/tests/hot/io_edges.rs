//! Behaviours of the hot device queue, the real directory and the index's view of the device, and
//! of a replica's side of replication, that the suites of their rows leave open: a replica's
//! `pending` answer, a batch for a copy it does not hold, a replica whose disk fails while it
//! creates or extends a copy, a directory listing that is not a directory, a `DevFs` append on a
//! path below a file, and the `Debug` forms of the queue and of `DevFs`.
//!
//! Catches: a replica that answers a definitive miss for a record it holds durably but not yet
//! acknowledged, or keeps saying so after the writer noticed it; a replica that opens a copy for
//! a batch that does not start at 0, or acknowledges a batch whose bytes it failed to write, or
//! leaves a half-made copy that blocks the retry; a listing that hides a failure as an empty
//! directory; an append that treats any failure of `len` as "no file yet".

use std::io::ErrorKind;
use std::sync::Arc;

use crate::support::{Sim, config};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_format::crc32c_append;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::seal::Ballot;
use tierstore_harness::disk::{Fault, OpKind};
use tierstore_hot::dev::{DirDev, HotDev};
use tierstore_hot::devfs::DevFs;
use tierstore_hot::sched::HotSched;
use tierstore_index::fs::Fs;
use tierstore_store_api::hot::{HotStore, ReplicaAck, ReplicaBatch, ReplicaReject, SealReason};
use tierstore_testkit::dir::TestDir;

fn replicate_on(
    sim: &mut Sim,
    node: u8,
    seg: BlockId,
    start: u64,
    crc: u32,
    bytes: &[u8],
) -> Result<ReplicaAck, ReplicaReject> {
    let batch = ReplicaBatch {
        segment: seg,
        ballot: Ballot::opening(NodeId(1)),
        start,
        running_crc: crc,
        stamp: sim.stamp(),
        bytes: bytes.to_vec().into(),
    };
    let n = sim.node(node);
    sim.exec.block(async move { n.replicate(batch).await })
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// The bytes of one acknowledged record, made on a separate cluster: a replica is handed them as
/// a batch of a writer that has not yet said anything about them.
fn record_bytes(key: &str) -> (BlockId, Vec<u8>) {
    let mut other = Sim::new(3, config(3, 3));
    let k = other.key_on(1, key);
    let put = other.append(1, &k, b"payload");
    other.run();
    assert!(put.take().unwrap().is_ok());
    let seg = other.node(1).open_segments()[0].id;
    let bytes = other
        .disk(1)
        .read_all(&HotName::Segment(seg).path())
        .unwrap();
    (seg, bytes)
}

fn key_of(bytes: &[u8]) -> Vec<u8> {
    let (rec, _) = tierstore_format::record::decode_record(bytes).unwrap();
    rec.key.to_vec()
}

#[test]
fn a_replica_says_pending_for_a_durable_record_until_the_writer_notices_it() {
    let (seg, bytes) = record_bytes("p");
    let key = key_of(&bytes);
    let mut sim = Sim::new(3, config(3, 3));
    let crc = crc32c_append(0, &bytes);
    let got = replicate_on(&mut sim, 2, seg, 0, crc, &bytes);
    assert_eq!(
        got,
        Ok(ReplicaAck {
            committed: bytes.len() as u64
        })
    );
    let replica = sim.node(2);
    assert!(replica.pending(&key), "durable, not noticed yet");
    assert!(!replica.pending(b"another key"));
    // The writer's next batch says every record before its start is acknowledged.
    let next = replicate_on(&mut sim, 2, seg, bytes.len() as u64, crc, &[]);
    assert!(next.is_ok(), "{next:?}");
    assert!(!replica.pending(&key), "noticed, so indexed");
}

#[test]
fn a_replica_stops_saying_pending_once_a_seal_covers_the_record() {
    let (seg, bytes) = record_bytes("s");
    let key = key_of(&bytes);
    let mut sim = Sim::new(3, config(3, 3));
    let crc = crc32c_append(0, &bytes);
    for node in [2, 3] {
        assert!(replicate_on(&mut sim, node, seg, 0, crc, &bytes).is_ok());
    }
    let replica = sim.node(2);
    assert!(replica.pending(&key));
    let n = sim.node(2);
    let ballot = Ballot {
        round: 2,
        node: NodeId(2),
    };
    let sealed = sim
        .exec
        .block(async move { n.seal(seg, ballot, SealReason::TakeOver).await });
    assert!(sealed.is_ok(), "{sealed:?}");
    sim.run();
    assert!(!replica.pending(&key));
}

#[test]
fn a_batch_for_a_copy_the_replica_does_not_hold_that_does_not_start_at_zero_is_refused() {
    let mut sim = Sim::new(3, config(3, 3));
    let seg = BlockId::new(NodeId(1), 900).unwrap();
    let got = replicate_on(&mut sim, 2, seg, 5, 0, &[]);
    assert_eq!(
        got,
        Err(ReplicaReject::Offset {
            start: 5,
            committed: 0
        })
    );
    assert!(!sim.disk(2).exists(&HotName::Segment(seg).path()).unwrap());
}

#[test]
fn a_replica_that_cannot_create_the_copy_refuses_the_batch_and_takes_the_retry() {
    let mut sim = Sim::new(3, config(3, 3));
    let seg = BlockId::new(NodeId(1), 901).unwrap();
    let path = HotName::Segment(seg).path();
    sim.disk(2).inject(OpKind::Create, 0, Fault::Eio);
    let bytes = b"abc";
    let crc = crc32c_append(0, bytes);
    let first = replicate_on(&mut sim, 2, seg, 0, crc, bytes);
    assert_eq!(
        first,
        Err(ReplicaReject::Offset {
            start: 0,
            committed: 0
        })
    );
    assert_eq!(sim.disk(2).read_all(&path).ok(), None, "no half-made copy");
    let retry = replicate_on(&mut sim, 2, seg, 0, crc, bytes);
    assert_eq!(retry, Ok(ReplicaAck { committed: 3 }));
    assert_eq!(sim.disk(2).read_all(&path).unwrap(), bytes);
}

#[test]
fn a_replica_whose_write_fails_acknowledges_nothing_and_takes_the_retry() {
    let mut sim = Sim::new(3, config(3, 3));
    let seg = BlockId::new(NodeId(1), 902).unwrap();
    let path = HotName::Segment(seg).path();
    let bytes = b"abc";
    let crc = crc32c_append(0, bytes);
    sim.disk(2).inject(OpKind::WriteAt, 0, Fault::Eio);
    let first = replicate_on(&mut sim, 2, seg, 0, crc, bytes);
    assert_eq!(
        first,
        Err(ReplicaReject::Offset {
            start: 0,
            committed: 0
        })
    );
    assert_eq!(sim.disk(2).read_all(&path).unwrap(), b"", "nothing written");
    let retry = replicate_on(&mut sim, 2, seg, 0, crc, bytes);
    assert_eq!(retry, Ok(ReplicaAck { committed: 3 }));
}

#[test]
fn listing_a_file_as_a_directory_fails_instead_of_looking_empty() {
    let dir = scratch("cov-e-list");
    let dev = DirDev::new(dir.path());
    dev.create("seg/f").unwrap();
    let err = dev.list("seg/f").unwrap_err();
    assert_ne!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn a_devfs_append_below_a_file_fails_and_creates_nothing() {
    let dir = scratch("cov-e-append");
    let dev = Arc::new(DirDev::new(dir.path()));
    dev.create("f").unwrap();
    let fs = DevFs::new(dev);
    let err = fs.append("f/log", b"x").unwrap_err();
    assert_ne!(err.kind(), ErrorKind::NotFound);
    assert_eq!(fs.len("f").unwrap(), 0);
}

#[test]
fn the_queue_and_devfs_print_their_shape() {
    let sched = HotSched::inline(1000, Arc::new(ManualClock::new()));
    assert!(format!("{sched:?}").starts_with("HotSched { threads: 0"));
    let fs = DevFs::new(Arc::new(DirDev::new(scratch("cov-e-debug").path())));
    assert!(format!("{fs:?}").starts_with("DevFs"));
}
