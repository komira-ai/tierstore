//! Section 14 rows "Replication protocol under scripted crashes and message loss" and "Weaker
//! acknowledgement is documented behaviour": a replica rejects a batch with a start offset other
//! than its committed length, a wrong running crc or a ballot below its promise, and a batch after
//! its seal committed, and a refused batch changes nothing; with `min_acks = replicas` every
//! acknowledged record is on every copy after a crash of the writer at each step (before the local
//! sync, after it and before a replica ack, after the replicas synced and before the client ack);
//! unacknowledged records are never visible before a seal and, after a seal, only within the
//! sealed length; a replica taking over seals by majority and serves every acknowledged record;
//! the returned old writer adopts the committed seal and its old-ballot batches are rejected; a
//! lost acknowledgement is resynced by the `Offset` answer; with `min_acks = 1` a writer lost
//! between its local sync and the replica's receipt loses the acknowledged record, and with the
//! default it does not.
//!
//! Catches: a replica that appends at the wrong offset, takes bytes with a wrong crc, or accepts
//! a fenced writer or a batch after the seal (split brain, resurrected tail); a refusal that
//! still moves the copy; an acknowledgement sent before every copy is durable; an unacknowledged
//! record indexed before a seal covers it, or kept beyond the sealed length; a take-over that
//! loses or invents a record; a returned writer that does not adopt the committed seal; a lost
//! acknowledgement that stalls the PUT or duplicates the record; a durability claim stronger than
//! the configuration.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{Kind, Rendezvous, Sim, config};
use tierstore_format::crc32c_append;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::seal::Ballot;
use tierstore_harness::disk::{Fault, OpKind};
use tierstore_store_api::hot::{
    Acked, Committed, HotError, HotStore, ReplicaAck, ReplicaBatch, ReplicaReject, SealReason,
    SealRefused, Unavailable,
};
use tierstore_store_api::index::{EntryLocation, Index, Lookup};
use tierstore_store_api::placement::Placement;

/// A record the test acknowledged: its key, data and the answer.
struct Done {
    key: String,
    data: Vec<u8>,
    acked: Acked,
}

/// Appends `n` records on node 1 and runs until each is acknowledged.
fn ack_records(sim: &mut Sim, n: usize) -> Vec<Done> {
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let key = format!("acked-{i}");
            let data = format!("the acknowledged record {i}").into_bytes();
            (key.clone(), data.clone(), sim.append(1, &key, &data))
        })
        .collect();
    sim.run();
    handles
        .into_iter()
        .map(|(key, data, h)| Done {
            acked: h.take().expect("finished").expect("acknowledged"),
            key,
            data,
        })
        .collect()
}

fn lookup(sim: &Sim, node: u8, key: &str) -> Lookup {
    sim.node(node).index().get(key.as_bytes()).unwrap()
}

fn seg_path(seg: BlockId) -> String {
    HotName::Segment(seg).path()
}

/// The bytes of node `node`'s file of `seg`.
fn seg_bytes(sim: &Sim, node: u8, seg: BlockId) -> Vec<u8> {
    let (disk, path) = (sim.disk(node), seg_path(seg));
    let len = disk.len(&path).unwrap();
    disk.read_at(&path, 0, len).unwrap()
}

fn seg_len(sim: &Sim, node: u8, seg: BlockId) -> u64 {
    sim.disk(node).len(&seg_path(seg)).unwrap() as u64
}

/// The one open segment of node 1.
fn open_segment(sim: &Sim) -> BlockId {
    let open = sim.node(1).open_segments();
    assert_eq!(open.len(), 1, "{open:?}");
    open[0].id
}

fn block_of(acked: &Acked) -> (BlockId, u64) {
    match acked.location {
        EntryLocation::Block { block, offset } => (block, offset),
        ref other => panic!("not a segment record: {other:?}"),
    }
}

/// Asserts that every record of `done` is indexed where it was acknowledged and read back
/// through `read_record` on each of `nodes`.
fn assert_served_on(sim: &Sim, nodes: &[u8], done: &[Done]) {
    for &n in nodes {
        for d in done {
            match lookup(sim, n, &d.key) {
                Lookup::Live(e) => assert_eq!(e.location, d.acked.location, "node {n} {}", d.key),
                other => panic!("node {n} lacks {}: {other:?}", d.key),
            }
            let (block, offset) = block_of(&d.acked);
            let node = sim.node(n);
            let (key, data) = sim
                .exec
                .block(async move { node.read_record(block, offset).await })
                .unwrap();
            assert_eq!(key, d.key.as_bytes(), "node {n}");
            assert_eq!(&data[..], &d.data[..], "node {n}");
        }
    }
}

fn assert_absent_on(sim: &Sim, nodes: &[u8], key: &str) {
    for &n in nodes {
        assert_eq!(
            lookup(sim, n, key),
            Lookup::Absent,
            "node {n} indexed {key}"
        );
    }
}

/// `HotStore::seal` on node `node` under `ballot`, run to its end.
fn seal_on(sim: &Sim, node: u8, seg: BlockId, ballot: Ballot) -> Result<Committed, HotError> {
    let n = sim.node(node);
    sim.exec
        .block(async move { n.seal(seg, ballot, SealReason::TakeOver).await })
}

fn ballot(round: u32, node: u8) -> Ballot {
    Ballot {
        round,
        node: NodeId(node),
    }
}

/// `HotStore::replicate` called directly on node `node`.
fn replicate_on(sim: &Sim, node: u8, batch: ReplicaBatch) -> Result<ReplicaAck, ReplicaReject> {
    let n = sim.node(node);
    sim.exec.block(async move { n.replicate(batch).await })
}

fn batch(
    sim: &mut Sim,
    seg: BlockId,
    ballot: Ballot,
    start: u64,
    crc: u32,
    bytes: &[u8],
) -> ReplicaBatch {
    ReplicaBatch {
        segment: seg,
        ballot,
        start,
        running_crc: crc,
        stamp: sim.stamp(),
        bytes: bytes.to_vec().into(),
    }
}

// ---------------------------------------------------------------------------------------------
// A replica's refusals.

/// Three nodes at the default ack rule, one acknowledged record: every node holds `len` bytes of
/// the segment, and the replicas hold them without having promised anything.
struct Mirrored {
    sim: Sim,
    seg: BlockId,
    bytes: Vec<u8>,
    crc: u32,
}

fn mirrored() -> Mirrored {
    let mut sim = Sim::new(3, config(3, 3));
    ack_records(&mut sim, 1);
    let seg = open_segment(&sim);
    let bytes = seg_bytes(&sim, 1, seg);
    let crc = crc32c_append(0, &bytes);
    assert_eq!(
        seg_bytes(&sim, 2, seg),
        bytes,
        "the replica mirrors the writer"
    );
    Mirrored {
        sim,
        seg,
        bytes,
        crc,
    }
}

impl Mirrored {
    /// Asserts that node 2's copy is as the acknowledged record left it, and still accepts the
    /// next batch (an empty one at its committed length with the right crc).
    fn assert_copy_untouched(&mut self) {
        assert_eq!(
            seg_bytes(&self.sim, 2, self.seg),
            self.bytes,
            "the file changed"
        );
        let next = batch(
            &mut self.sim,
            self.seg,
            Ballot::opening(NodeId(1)),
            self.bytes.len() as u64,
            self.crc,
            &[],
        );
        assert_eq!(
            replicate_on(&self.sim, 2, next),
            Ok(ReplicaAck {
                committed: self.bytes.len() as u64
            })
        );
    }
}

#[test]
fn a_replica_refuses_a_batch_that_does_not_start_at_its_committed_length() {
    let mut m = mirrored();
    let len = m.bytes.len() as u64;
    // Ahead of the copy: a gap.
    let gap = batch(
        &mut m.sim,
        m.seg,
        Ballot::opening(NodeId(1)),
        len + 1,
        m.crc,
        &[],
    );
    assert_eq!(
        replicate_on(&m.sim, 2, gap),
        Err(ReplicaReject::Offset {
            start: len + 1,
            committed: len
        })
    );
    // Behind the copy: bytes it already holds, sent again from the start.
    let again = batch(
        &mut m.sim,
        m.seg,
        Ballot::opening(NodeId(1)),
        0,
        m.crc,
        &m.bytes.clone(),
    );
    assert_eq!(
        replicate_on(&m.sim, 2, again),
        Err(ReplicaReject::Offset {
            start: 0,
            committed: len
        })
    );
    m.assert_copy_untouched();
}

#[test]
fn a_replica_refuses_a_batch_whose_running_crc_is_wrong() {
    let mut m = mirrored();
    let len = m.bytes.len() as u64;
    let right = crc32c_append(m.crc, &m.bytes);
    let wrong = right ^ 1;
    let bad = batch(
        &mut m.sim,
        m.seg,
        Ballot::opening(NodeId(1)),
        len,
        wrong,
        &m.bytes.clone(),
    );
    assert_eq!(
        replicate_on(&m.sim, 2, bad),
        Err(ReplicaReject::Crc {
            sent: wrong,
            computed: right
        })
    );
    m.assert_copy_untouched();
}

#[test]
fn a_replica_refuses_a_batch_under_a_ballot_below_its_promise_and_keeps_its_copy() {
    let mut m = mirrored();
    let len = m.bytes.len() as u64;
    let promised = ballot(5, 3);
    let n2 = m.sim.node(2);
    let seg = m.seg;
    let promise = m
        .sim
        .exec
        .block(async move { n2.promise(seg, promised).await })
        .unwrap();
    assert_eq!(promise.copy.unwrap().committed, len);
    // Otherwise a good batch: the right offset and crc.
    let more = m.bytes.clone();
    let crc = crc32c_append(m.crc, &more);
    let fenced = batch(
        &mut m.sim,
        m.seg,
        Ballot::opening(NodeId(1)),
        len,
        crc,
        &more,
    );
    assert_eq!(
        replicate_on(&m.sim, 2, fenced),
        Err(ReplicaReject::Ballot {
            ballot: Ballot::opening(NodeId(1)),
            promised
        })
    );
    assert_eq!(
        seg_bytes(&m.sim, 2, m.seg),
        m.bytes,
        "the fenced batch was written"
    );
    // The ballot the node promised is still accepted: its sealer is not fenced by the refusal.
    let at_promise = batch(&mut m.sim, m.seg, promised, len, m.crc, &[]);
    assert_eq!(
        replicate_on(&m.sim, 2, at_promise),
        Ok(ReplicaAck { committed: len })
    );
}

#[test]
fn a_replica_refuses_a_batch_once_the_seal_is_committed_and_keeps_its_copy() {
    let mut m = mirrored();
    let len = m.bytes.len() as u64;
    let sealed = seal_on(&m.sim, 1, m.seg, ballot(2, 1)).unwrap();
    assert_eq!(sealed.proposal.length, len);
    let sealed_bytes = seg_bytes(&m.sim, 2, m.seg);
    assert!(sealed_bytes.len() as u64 >= len);
    let late = m.bytes.clone();
    let crc = crc32c_append(m.crc, &late);
    // Even a batch that is right in every other way, under the opening ballot of the old writer.
    let b = batch(
        &mut m.sim,
        m.seg,
        Ballot::opening(NodeId(1)),
        len,
        crc,
        &late,
    );
    assert_eq!(replicate_on(&m.sim, 2, b), Err(ReplicaReject::Sealed));
    // And one under a ballot above the promise.
    let b = batch(&mut m.sim, m.seg, ballot(9, 1), len, crc, &late);
    assert_eq!(replicate_on(&m.sim, 2, b), Err(ReplicaReject::Sealed));
    assert_eq!(
        seg_bytes(&m.sim, 2, m.seg),
        sealed_bytes,
        "the sealed file changed"
    );
}

// ---------------------------------------------------------------------------------------------
// A crash of the writer at each step, at the default ack rule.

/// Node 2 takes over the segment of the dead node 1 by sealing it under a ballot of its own, the
/// writer returns (restart) and adopts the seal, and every acknowledged record is then indexed
/// and readable on all three nodes, whose files agree through the sealed length.
fn take_over_then_return_writer(sim: &mut Sim, seg: BlockId, done: &[Done]) -> Committed {
    let sealed = seal_on(sim, 2, seg, ballot(2, 2)).unwrap();
    assert_served_on(sim, &[2, 3], done);
    sim.restart(1);
    assert_served_on(sim, &[1, 2, 3], done);
    let len = sealed.proposal.length as usize;
    let reference = seg_bytes(sim, 2, seg);
    assert_eq!(
        crc32c_append(0, &reference[..len]),
        sealed.proposal.running_crc
    );
    for n in [1, 3] {
        assert_eq!(
            &seg_bytes(sim, n, seg)[..len],
            &reference[..len],
            "node {n}"
        );
    }
    sealed
}

/// Whether the record of `key` is indexed on `node` inside the sealed `length`, which is where a
/// seal makes an unacknowledged record visible.
fn visible_within(sim: &Sim, node: u8, key: &str, length: u64) -> bool {
    match lookup(sim, node, key) {
        Lookup::Live(e) => match e.location {
            EntryLocation::Block { offset, .. } => offset < length,
            ref other => panic!("not a segment record: {other:?}"),
        },
        Lookup::Absent => false,
        other => panic!("node {node}: {other:?}"),
    }
}

#[test]
fn a_writer_lost_before_its_local_sync_loses_no_acknowledged_record_and_shows_no_unacknowledged_one_early()
 {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    sim.disk(1).inject(OpKind::Fdatasync, 0, Fault::CrashBefore);
    let lost = sim.append(1, "unacked", b"never acknowledged");
    sim.run();
    assert!(
        !matches!(lost.take(), Some(Ok(_))),
        "acknowledged without a local sync"
    );
    sim.crash(1);
    // The replicas received the batch before the writer synced it; nobody indexed it.
    assert!(seg_len(&sim, 2, seg) > before && seg_len(&sim, 3, seg) > before);
    assert_absent_on(&sim, &[2, 3], "unacked");
    let sealed = take_over_then_return_writer(&mut sim, seg, &done);
    // Both replicas hold the record, so the seal covers it and it becomes visible, everywhere.
    assert!(sealed.proposal.length > before);
    for n in [1, 2, 3] {
        assert!(
            visible_within(&sim, n, "unacked", sealed.proposal.length),
            "node {n}"
        );
    }
}

#[test]
fn a_writer_lost_after_its_sync_and_before_any_replica_receives_the_batch_leaves_no_trace_of_it() {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let lost = sim.append(1, "unacked", b"never acknowledged");
    sim.run();
    assert!(
        !lost.done(),
        "acknowledged before a replica made it durable"
    );
    assert_eq!(sim.net.held().len(), 2);
    assert!(
        seg_len(&sim, 1, seg) > before,
        "the writer synced its own copy"
    );
    sim.crash(1);
    sim.net.drop_held(0);
    sim.net.drop_held(0);
    sim.run();
    assert!(!matches!(lost.take(), Some(Ok(_))));
    assert_eq!(seg_len(&sim, 2, seg), before);
    assert_absent_on(&sim, &[2, 3], "unacked");
    // The writer's disk still holds the synced tail.
    sim.disk(1).restart();
    assert!(seg_len(&sim, 1, seg) > before);
    let sealed = take_over_then_return_writer(&mut sim, seg, &done);
    assert_eq!(
        sealed.proposal.length, before,
        "the seal cuts at what the replicas hold"
    );
    // The returned writer dropped its tail: the record is nowhere.
    assert_absent_on(&sim, &[1, 2, 3], "unacked");
    assert_eq!(
        seg_len(&sim, 1, seg),
        seg_len(&sim, 2, seg),
        "the tail was converged away"
    );
}

#[test]
fn a_batch_that_reaches_the_replicas_after_the_writer_died_is_visible_only_through_the_seal() {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let lost = sim.append(1, "unacked", b"never acknowledged");
    sim.run();
    sim.crash(1);
    sim.net.release(0);
    sim.net.release(0);
    sim.run();
    assert!(!matches!(lost.take(), Some(Ok(_))));
    assert!(seg_len(&sim, 2, seg) > before && seg_len(&sim, 3, seg) > before);
    assert_absent_on(&sim, &[2, 3], "unacked");
    let sealed = take_over_then_return_writer(&mut sim, seg, &done);
    for n in [1, 2, 3] {
        assert!(
            visible_within(&sim, n, "unacked", sealed.proposal.length),
            "node {n}"
        );
    }
}

#[test]
fn a_writer_lost_after_the_replicas_synced_and_before_the_client_ack_loses_no_acknowledged_record()
{
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let lost = sim.append(1, "unacked", b"never acknowledged");
    sim.run();
    // Both replicas receive the batch and make it durable; the writer dies before their answers.
    sim.net.release(0);
    sim.net.release(0);
    sim.crash(1);
    assert!(
        !matches!(lost.take(), Some(Ok(_))),
        "the client heard from a dead writer"
    );
    assert!(seg_len(&sim, 2, seg) > before && seg_len(&sim, 3, seg) > before);
    assert_absent_on(&sim, &[2, 3], "unacked");
    let sealed = take_over_then_return_writer(&mut sim, seg, &done);
    assert!(
        sealed.proposal.length > before,
        "the seal covers what every copy holds"
    );
    for n in [1, 2, 3] {
        assert!(
            visible_within(&sim, n, "unacked", sealed.proposal.length),
            "node {n}"
        );
    }
}

#[test]
fn an_unacknowledged_record_held_by_one_replica_only_is_cut_by_the_seal_and_never_visible() {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let lost = sim.append(1, "unacked", b"held by node 2 alone");
    sim.run();
    // Node 2 receives the batch; the one to node 3 is lost, and the writer dies.
    let held = sim.net.held();
    assert_eq!(held.len(), 2);
    let to_2 = held.iter().position(|(_, to, _)| to.0 == 2).unwrap();
    let to_3 = held.iter().position(|(_, to, _)| to.0 == 3).unwrap();
    sim.net.release(to_2);
    sim.net.drop_held(if to_3 > to_2 { to_3 - 1 } else { to_3 });
    sim.crash(1);
    assert!(!matches!(lost.take(), Some(Ok(_))));
    assert!(
        seg_len(&sim, 2, seg) > before,
        "node 2 holds the unacknowledged tail"
    );
    assert_eq!(seg_len(&sim, 3, seg), before);
    assert_absent_on(&sim, &[2, 3], "unacked");
    // The shortest of the two answering copies is chosen: the tail is beyond the sealed length.
    let sealed = take_over_then_return_writer(&mut sim, seg, &done);
    assert_eq!(sealed.proposal.length, before);
    assert_absent_on(&sim, &[1, 2, 3], "unacked");
}

// ---------------------------------------------------------------------------------------------
// The old writer.

#[test]
fn the_returned_writer_adopts_the_committed_seal_and_its_old_ballot_batches_are_rejected() {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    let bytes = seg_bytes(&sim, 1, seg);
    let crc = crc32c_append(0, &bytes);
    sim.crash(1);
    let sealed = take_over_then_return_writer(&mut sim, seg, &done);
    assert_eq!(sealed.proposal.length, before);
    // The returned node answers a prepare with the committed seal, which never changes.
    let n1 = sim.node(1);
    let refused = sim
        .exec
        .block(async move { n1.promise(seg, ballot(9, 3)).await })
        .unwrap_err();
    assert_eq!(refused, SealRefused::Committed(sealed.proposal));
    // Batches of the old writer's ballot are rejected by every copy, the returned one included.
    for n in [1, 2, 3] {
        let old = batch(
            &mut sim,
            seg,
            Ballot::opening(NodeId(1)),
            before,
            crc32c_append(crc, &bytes),
            &bytes,
        );
        assert_eq!(
            replicate_on(&sim, n, old),
            Err(ReplicaReject::Sealed),
            "node {n}"
        );
    }
    // The returned writer writes again, to a new segment, at full replication.
    let again = sim.append(1, "after-return", b"written after the return");
    sim.run();
    let acked = again.take().unwrap().unwrap();
    assert_eq!(acked.copies, 3);
    assert_ne!(
        block_of(&acked).0,
        seg,
        "a sealed segment is never written again"
    );
}

#[test]
fn a_writer_that_missed_the_seal_is_fenced_by_the_replicas_and_its_record_goes_to_a_new_segment() {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 2, seg);
    // Node 1 is cut off; nodes 2 and 3 are a majority and seal the segment without it.
    sim.net.cut_both(1, 2);
    sim.net.cut_both(1, 3);
    let sealed = seal_on(&sim, 2, seg, ballot(2, 2)).unwrap();
    assert_eq!(sealed.proposal.length, before);
    sim.net.heal();
    let reference = seg_bytes(&sim, 2, seg);
    // Node 1 did not hear of the seal and writes on under its old ballot.
    let late = sim.append(1, "late", b"written by the fenced writer");
    sim.run();
    let acked = late.take().unwrap().unwrap();
    assert_ne!(
        block_of(&acked).0,
        seg,
        "acknowledged in the sealed segment"
    );
    assert_eq!(acked.copies, 3);
    // The replicas refused the old ballot's batch: their sealed file is as the seal left it.
    for n in [2, 3] {
        assert_eq!(seg_bytes(&sim, n, seg), reference, "node {n}");
        match lookup(&sim, n, "late") {
            Lookup::Live(e) => assert_eq!(e.location, acked.location, "node {n}"),
            other => panic!("node {n}: {other:?}"),
        }
    }
    assert_served_on(&sim, &[2, 3], &done);
    assert!(
        sim.net
            .batches(1, 2)
            .iter()
            .any(|(start, _)| *start == before),
        "the fenced writer's batch did reach node 2 and was refused there"
    );
}

// ---------------------------------------------------------------------------------------------
// A lost acknowledgement.

#[test]
fn a_replica_ahead_of_the_writer_is_resynced_by_the_offset_answer_and_the_put_completes() {
    let mut sim = Sim::new(3, config(3, 3));
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let put = sim.append(1, "resynced", b"its first acknowledgement is lost");
    sim.run();
    assert!(!put.done());
    let seg = open_segment(&sim);
    // Node 2 made the writer's first batch durable, but the writer never heard: the same bytes
    // reach it again from offset 0 and its answer names its longer committed length.
    let bytes = seg_bytes(&sim, 1, seg);
    let len = bytes.len() as u64;
    let first = batch(
        &mut sim,
        seg,
        Ballot::opening(NodeId(1)),
        0,
        crc32c_append(0, &bytes),
        &bytes,
    );
    assert_eq!(
        replicate_on(&sim, 2, first),
        Ok(ReplicaAck { committed: len })
    );
    while !sim.net.held().is_empty() {
        sim.net.release(0);
        sim.run();
    }
    let acked = put.take().unwrap().unwrap();
    assert_eq!(acked.copies, 3);
    assert_eq!(
        block_of(&acked),
        (seg, 0),
        "no roll: the record stays in its segment"
    );
    assert_eq!(
        sim.node(1).open_segments()[0].id,
        seg,
        "the segment is still open"
    );
    assert_eq!(
        seg_bytes(&sim, 2, seg),
        bytes,
        "the bytes are on node 2 once"
    );
    assert_eq!(seg_bytes(&sim, 3, seg), bytes);
    let done = [Done {
        key: "resynced".into(),
        data: b"its first acknowledgement is lost".to_vec(),
        acked,
    }];
    assert_served_on(&sim, &[1, 2, 3], &done);
}

/// A key whose replica set on `sim` (the first three live nodes of its rank) is `want`.
fn key_with_set(sim: &Sim, tag: &str, want: [u8; 3]) -> String {
    (0..10_000)
        .map(|i| format!("{tag}-{i}"))
        .find(|k| {
            let mut top: Vec<u8> = Rendezvous
                .rank(k.as_bytes(), &sim.view)
                .into_iter()
                .take(3)
                .map(|n| n.0)
                .collect();
            top.sort_unstable();
            top == want
        })
        .expect("a key with that replica set")
}

/// Asserts that the writer (node 1) indexes `key` where `acked` says, and that every node of
/// `nodes` that indexes it serves `data` at the place its own index names (the nodes of an early
/// sealed segment and of its successor may name different segments for the same record).
fn assert_one_copy_of(sim: &Sim, nodes: &[u8], key: &str, data: &[u8], acked: &Acked) {
    let mut seen = 0;
    for &n in nodes {
        if let Lookup::Live(e) = lookup(sim, n, key) {
            seen += 1;
            if n == 1 {
                assert_eq!(
                    e.location, acked.location,
                    "the writer's index differs from the answer"
                );
            }
            let (block, offset) = match e.location {
                EntryLocation::Block { block, offset } => (block, offset),
                ref other => panic!("not a segment record: {other:?}"),
            };
            let node = sim.node(n);
            let (_, got) = sim
                .exec
                .block(async move { node.read_record(block, offset).await })
                .unwrap();
            assert_eq!(&got[..], data, "node {n}");
        }
    }
    assert!(seen >= 3, "only {seen} nodes index {key}");
}

#[test]
fn a_lost_replica_reply_rolls_the_segment_and_the_put_completes_on_the_live_nodes() {
    let mut sim = Sim::new(4, config(3, 3));
    let key = key_with_set(&sim, "lost", [1, 2, 3]);
    let data = b"node 2 syncs it and its reply is lost";
    sim.net.lose_replies(2, 1, Kind::Replicate, 1);
    let put = sim.append(1, &key, data);
    sim.run();
    // Node 2 is out of the replica set of the retry; the record is acknowledged on three copies.
    let acked = put.take().expect("finished").expect("acknowledged");
    assert_eq!(acked.copies, 3);
    // Whichever segment finally holds it, the index of each node that has it points at the same
    // place, and what is there is the record.
    let (seg, _) = block_of(&acked);
    let holders: Vec<u8> = (1..=4)
        .filter(|n| sim.disk(*n).exists(&seg_path(seg)).unwrap())
        .collect();
    assert!(holders.len() >= 3, "{holders:?}");
    assert_one_copy_of(&sim, &holders, &key, data, &acked);
}

#[test]
fn a_put_whose_replica_reply_was_lost_is_refused_while_the_replica_is_suspect_and_the_retry_returns_the_same_record()
 {
    let mut sim = Sim::new(3, config(3, 3));
    let first = ack_records(&mut sim, 1);
    let data = b"node 2 syncs it and its reply is lost";
    sim.net.lose_replies(2, 1, Kind::Replicate, 1);
    let put = sim.append(1, "reply-lost", data);
    sim.run();
    // Node 2 failed the writer, so the three-copy rule cannot be met until it is trusted again.
    let refused = put.take().expect("finished").unwrap_err();
    assert!(
        matches!(
            refused,
            HotError::Unavailable {
                reason: Unavailable::TooFewCopies,
                ..
            }
        ),
        "{refused:?}"
    );
    // The early seal covered what all three copies held, so the record is already there once.
    sim.clock.advance(sim.cfg.replica_stall_ms);
    let retry = sim.append(1, "reply-lost", data);
    sim.run();
    let acked = retry.take().expect("finished").expect("acknowledged");
    assert_eq!(acked.copies, 3);
    assert_one_copy_of(&sim, &[1, 2, 3], "reply-lost", data, &acked);
    assert_served_on(&sim, &[1, 2, 3], &first);
}

// ---------------------------------------------------------------------------------------------
// Weaker acknowledgement.

#[test]
fn with_min_acks_one_a_writer_lost_before_the_replicas_receive_the_record_loses_it() {
    let mut sim = Sim::new(3, config(3, 1));
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let put = sim.append(1, "weak", b"acknowledged on one copy");
    sim.run();
    // The writer's own sync is enough: the client is told the record is stored.
    let acked = put.take().expect("finished").expect("acknowledged");
    assert_eq!(acked.copies, 1);
    let (seg, _) = block_of(&acked);
    // The writer is lost for good between its local sync and the replicas' receipt.
    sim.crash(1);
    sim.net.drop_held(0);
    sim.net.drop_held(0);
    sim.run();
    for n in [2, 3] {
        assert!(
            !sim.disk(n).exists(&seg_path(seg)).unwrap(),
            "node {n} holds a copy"
        );
        assert_eq!(lookup(&sim, n, "weak"), Lookup::Absent, "node {n}");
    }
    // Nothing the survivors can do brings it back: no copy answers their seal.
    let sealed = seal_on(&sim, 2, seg, ballot(2, 2));
    assert!(
        matches!(
            sealed,
            Err(HotError::Unavailable {
                reason: Unavailable::TooFewCopies,
                ..
            })
        ),
        "{sealed:?}"
    );
    assert_absent_on(&sim, &[2, 3], "weak");
}

#[test]
fn with_the_default_ack_rule_a_writer_lost_before_the_replicas_receive_the_record_never_acknowledged_it()
 {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 2);
    let seg = open_segment(&sim);
    let before = seg_len(&sim, 1, seg);
    sim.net.hold(Some(1), 2, Kind::Replicate);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let put = sim.append(1, "strong", b"acknowledged only on three copies");
    sim.run();
    assert!(!put.done(), "acknowledged on the writer's copy alone");
    sim.crash(1);
    sim.net.drop_held(0);
    sim.net.drop_held(0);
    sim.run();
    assert!(
        !matches!(put.take(), Some(Ok(_))),
        "the client was never told it is stored"
    );
    // What the client was told is stored survives the loss of the writer.
    let sealed = seal_on(&sim, 2, seg, ballot(2, 2)).unwrap();
    assert_served_on(&sim, &[2, 3], &done);
    assert_absent_on(&sim, &[2, 3], "strong");
    assert_eq!(
        sealed.proposal.length, before,
        "the seal ends with the last acknowledged record"
    );
}

#[test]
fn a_replica_seals_by_the_tick_once_the_writer_is_down() {
    let mut sim = Sim::new(3, config(3, 3));
    let done = ack_records(&mut sim, 3);
    let seg = open_segment(&sim);
    sim.crash(1);
    sim.set_state(1, tierstore_store_api::view::NodeState::Down);
    sim.clock.advance(60_000);
    for n in [2, 3] {
        sim.node(n).tick();
    }
    sim.run();
    let n2 = sim.node(2);
    let committed = sim
        .exec
        .block(async move { n2.promise(seg, ballot(9, 2)).await })
        .unwrap_err();
    assert!(
        matches!(committed, SealRefused::Committed(_)),
        "{committed:?}"
    );
    assert_served_on(&sim, &[2, 3], &done);
}
