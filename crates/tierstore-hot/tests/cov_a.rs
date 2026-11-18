//! The writer's side of a write segment (`src/node/writer.rs`) on the paths the suites of the
//! section 14 rows "Group commit", "Replication protocol under scripted crashes and message
//! loss" and "Seal and roll" do not reach: where a segment is placed when this node ranks low,
//! how an append ends when every segment it tries loses a replica, what a record's
//! acknowledgement says when the index changed under the key lock, a promise or an accepted
//! proposal that fences the writer, a segment that fills inside a batch, a replica that lags
//! behind the writer, and the answers of a replica that are not an acknowledgement.
//!
//! Catches: a writer that places a segment on a replica set without itself; an append that
//! answers 200 or loops forever when three segments in a row lose their replicas; an
//! acknowledgement that hides a conflict, a tombstone or an index failure that arrived while the
//! batch was in flight; a fenced writer that acknowledges; a full segment that is not rolled; a
//! lagging replica fed from the wrong offset or a failed read of the local copy that stops the
//! writer; a replica that is acknowledged on an answer that is not the length sent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;
use support::{Kind, Rendezvous, Sim, config};
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::crc32c_append;
use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::RecordFlags;
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::tomb::Tombstone;
use tierstore_harness::disk::{Fault, OpKind};
use tierstore_hot::peers::{HotPeers, LargeAnswer, LargeChunk, LargeOffer};
use tierstore_index::backend::{Backend, Op, Space};
use tierstore_store_api::hot::{Acked, HotError, HotStore, ReplicaBatch, Unavailable};
use tierstore_store_api::index::{EntryLocation, Index, IndexEntry, Lookup, PutOutcome};
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::placement::Placement;
use tierstore_store_api::proto::{Envelope, ReplicaAnswer, SealAnswer, SealMessage};

type Append = support::Handle<Result<Acked, HotError>>;

/// Releases every held message, one at a time, until nothing is held.
fn drain(sim: &Sim) {
    sim.net.unhold();
    while !sim.net.held().is_empty() {
        sim.net.release(0);
        sim.run();
    }
}

fn block_of(acked: &Acked) -> BlockId {
    match acked.location {
        EntryLocation::Block { block, .. } => block,
        ref other => panic!("not in a block: {other:?}"),
    }
}

fn is_too_few_copies(r: &Result<Acked, HotError>) -> bool {
    matches!(
        r,
        Err(HotError::Unavailable {
            reason: Unavailable::TooFewCopies,
            ..
        })
    )
}

/// Starts an append of `data` under `key` on node 1 whose replicate message to node 2 is held,
/// and returns the handle with the segment it opened.
fn held_append(sim: &mut Sim, key: &str, data: &[u8]) -> (Append, BlockId) {
    sim.net.hold(Some(1), 2, Kind::Replicate);
    let h = sim.append(1, key, data);
    sim.run();
    assert!(!h.done(), "acknowledged before the replica made it durable");
    let seg = sim.node(1).open_segments()[0].id;
    (h, seg)
}

/// An index entry of `data` for `key` in `seg`, as another writer would have published it.
fn foreign_entry(sim: &mut Sim, seg: BlockId, data: &[u8]) -> IndexEntry {
    IndexEntry {
        generation: Generation::of(data),
        location: EntryLocation::Block {
            block: seg,
            offset: 0,
        },
        length: data.len() as u64,
        stamp: sim.stamp(),
        flags: RecordFlags::default(),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

/// Puts bytes in the index of `node` under `key` that no entry decodes from.
fn corrupt_entry(sim: &Sim, node: u8, key: &str) {
    sim.node(node)
        .index()
        .backend()
        .commit(vec![Op::Put {
            space: Space::Entries,
            key: key.as_bytes().to_vec(),
            value: vec![0xff; 3],
        }])
        .unwrap();
}

fn is_index_failure(r: &Result<Acked, HotError>) -> bool {
    matches!(r, Err(HotError::Io { op: "index", .. }))
}

#[test]
fn a_node_that_ranks_low_for_a_key_writes_it_to_itself_and_the_best_ranked_other() {
    let mut sim = Sim::new(3, config(2, 2));
    let key = (0..10_000)
        .map(|i| format!("low-{i}"))
        .find(|k| Rendezvous.rank(k.as_bytes(), &sim.view)[2] == NodeId(1))
        .expect("a key node 1 ranks last for");
    let first = Rendezvous.rank(key.as_bytes(), &sim.view)[0];
    let h = sim.append(1, &key, b"low");
    sim.run();
    assert_eq!(h.take().unwrap().unwrap().copies, 2);
    let mut want = vec![NodeId(1), first];
    want.sort_unstable();
    assert_eq!(sim.node(1).open_segments()[0].set, want);
    assert!(!sim.net.batches(1, first.0).is_empty());
    let other = Rendezvous.rank(key.as_bytes(), &sim.view)[1];
    assert!(
        sim.net.batches(1, other.0).is_empty(),
        "the second ranked node got a copy"
    );
}

#[test]
fn an_append_whose_three_segments_each_lose_their_replica_answers_503_too_few_copies() {
    let mut sim = Sim::new(4, config(2, 2));
    for to in 2..=4 {
        sim.net.cut(1, to);
    }
    let key = sim.key_on(1, "lost");
    let h = sim.append(1, &key, b"never acknowledged");
    sim.run();
    assert!(is_too_few_copies(&h.take().unwrap()));
    for to in 2..=4 {
        assert_eq!(
            sim.net.sent(1, to, Kind::Replicate),
            1,
            "one segment, so one try, per replica"
        );
    }
    assert_eq!(
        sim.node(1).index().get(key.as_bytes()).unwrap(),
        Lookup::Absent
    );
}

#[test]
fn an_entry_the_index_cannot_decode_fails_the_append_before_anything_is_written() {
    let mut sim = Sim::new(1, config(1, 1));
    corrupt_entry(&sim, 1, "bad");
    let h = sim.append(1, "bad", b"data");
    sim.run();
    assert!(is_index_failure(&h.take().unwrap()));
    assert!(sim.node(1).open_segments().is_empty());
}

#[test]
fn an_index_that_fails_while_the_batch_is_in_flight_fails_that_record_not_the_segment() {
    let mut sim = Sim::new(2, config(2, 2));
    let (h, _) = held_append(&mut sim, "flaky", b"data");
    corrupt_entry(&sim, 1, "flaky");
    drain(&sim);
    assert!(is_index_failure(&h.take().unwrap()));
}

#[test]
fn a_different_object_published_under_the_key_while_the_batch_is_in_flight_answers_412() {
    let mut sim = Sim::new(2, config(2, 2));
    let (h, seg) = held_append(&mut sim, "contested", b"mine");
    let theirs = foreign_entry(&mut sim, seg, b"theirs");
    let put = sim
        .node(1)
        .index()
        .put_if_absent(b"contested", theirs)
        .unwrap();
    assert_eq!(put, PutOutcome::Created);
    drain(&sim);
    match h.take().unwrap() {
        Err(HotError::Conflict { existing }) => assert_eq!(existing, Generation::of(b"theirs")),
        other => panic!("expected 412, got {other:?}"),
    }
}

#[test]
fn a_tombstone_above_the_record_that_arrives_while_the_batch_is_in_flight_answers_503() {
    let mut sim = Sim::new(2, config(2, 2));
    let (h, _) = held_append(&mut sim, "doomed", b"data");
    let stamp = sim.stamp();
    sim.node(1)
        .index()
        .delete(&Tombstone {
            key: b"doomed".to_vec(),
            generation: Generation::of(b"data"),
            stamp,
            names: Vec::new(),
        })
        .unwrap();
    drain(&sim);
    assert!(matches!(
        h.take().unwrap(),
        Err(HotError::Unavailable {
            reason: Unavailable::NotDefinitive,
            ..
        })
    ));
}

#[test]
fn the_record_that_does_not_fit_after_a_batch_sizes_the_segment_off_and_goes_to_the_next() {
    let mut cfg = config(1, 1);
    cfg.seal_bytes = 7000;
    cfg.seal_min_bytes = 1000;
    let mut sim = Sim::new(1, cfg);
    let data = vec![5u8; 3000];
    let hs: Vec<_> = (0..3)
        .map(|i| sim.append(1, &format!("fill-{i}"), &data))
        .collect();
    sim.run();
    let blocks: Vec<BlockId> = hs
        .into_iter()
        .map(|h| block_of(&h.take().unwrap().unwrap()))
        .collect();
    assert_eq!(blocks[0], blocks[1], "two records fit below hot.seal_bytes");
    assert_ne!(blocks[1], blocks[2], "the third went to a new segment");
    let open = sim.node(1).open_segments();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, blocks[2]);
    assert_eq!(open[0].records, 1);
}

#[test]
fn a_promise_for_this_nodes_own_segment_fences_its_writer_and_the_record_moves_on() {
    let mut sim = Sim::new(2, config(2, 2));
    let (h, seg) = held_append(&mut sim, "fenced", b"data");
    let n1 = sim.node(1);
    let promised = sim.exec.block(async move {
        n1.promise(
            seg,
            Ballot {
                round: 5,
                node: NodeId(2),
            },
        )
        .await
    });
    assert!(promised.is_ok());
    sim.run();
    drain(&sim);
    let acked = h.take().unwrap().unwrap();
    assert_ne!(block_of(&acked), seg, "acknowledged in the fenced segment");
}

#[test]
fn an_accepted_proposal_above_the_writers_ballot_fences_it_at_its_next_acknowledgement() {
    let mut sim = Sim::new(2, config(2, 2));
    let (h, seg) = held_append(&mut sim, "accepted", b"data");
    let n1 = sim.node(1);
    let accepted = sim.exec.block(async move {
        n1.accept(
            seg,
            Ballot {
                round: 5,
                node: NodeId(2),
            },
            SealProposal {
                length: 0,
                running_crc: 0,
            },
        )
        .await
    });
    assert!(accepted.is_ok());
    drain(&sim);
    let acked = h.take().unwrap().unwrap();
    assert_ne!(block_of(&acked), seg, "acknowledged in the fenced segment");
}

#[test]
fn a_replica_that_failed_is_not_sent_the_next_batch_and_the_others_still_acknowledge() {
    let mut sim = Sim::new(3, config(3, 2));
    sim.net.cut(1, 2);
    let key = sim.key_on(1, "two-of-three");
    let h = sim.append(1, &key, b"data");
    sim.run();
    assert_eq!(h.take().unwrap().unwrap().copies, 2);
    assert_eq!(sim.net.sent(1, 2, Kind::Replicate), 1);
    assert!(!sim.net.batches(1, 3).is_empty());
}

/// Three records acknowledged by node 2 while node 3 holds its first batch: when that batch
/// arrives, node 3 is behind. Returns the sim and the handle of the third record when `hold_last`
/// keeps it in flight.
fn lagging_replica(hold_last: bool) -> (Sim, Option<Append>) {
    let mut sim = Sim::new(3, config(3, 2));
    let key = |sim: &Sim, tag: &str| sim.key_on(1, tag);
    sim.net.hold(Some(1), 3, Kind::Replicate);
    let (k1, k2, k3) = (key(&sim, "a"), key(&sim, "b"), key(&sim, "c"));
    for (k, d) in [(&k1, &b"first"[..]), (&k2, &b"second"[..])] {
        let h = sim.append(1, k, d);
        sim.run();
        assert_eq!(h.take().unwrap().unwrap().copies, 2);
    }
    let last = if hold_last {
        sim.net.hold(Some(1), 2, Kind::Replicate);
        let h = sim.append(1, &k3, b"third");
        sim.run();
        assert!(!h.done());
        Some(h)
    } else {
        let h = sim.append(1, &k3, b"third");
        sim.run();
        assert_eq!(h.take().unwrap().unwrap().copies, 2);
        None
    };
    (sim, last)
}

fn release_to(sim: &Sim, node: u8) {
    let i = sim
        .net
        .held()
        .iter()
        .position(|(_, to, _)| *to == NodeId(node))
        .expect("a message held for the node");
    sim.net.release(i);
    sim.run();
}

#[test]
fn a_lagging_replica_is_sent_the_acknowledged_bytes_it_misses_read_back_from_the_local_copy() {
    let (sim, _) = lagging_replica(false);
    release_to(&sim, 3);
    drain(&sim);
    let seg = sim.node(1).open_segments()[0].id;
    let path = HotName::Segment(seg).path();
    let writer = sim.disk(1).read_all(&path).unwrap();
    assert_eq!(
        sim.disk(3).read_all(&path).unwrap(),
        writer,
        "the lagging replica's copy equals the writer's"
    );
    assert_eq!(sim.net.batches(1, 3).len(), 2);
}

#[test]
fn a_lagging_replica_is_sent_the_missed_bytes_and_then_the_batch_in_flight_in_one_message() {
    let (sim, last) = lagging_replica(true);
    release_to(&sim, 3);
    drain(&sim);
    let batches = sim.net.batches(1, 3);
    assert_eq!(batches.len(), 2, "{batches:?}");
    let first_len = batches[0].1;
    assert_eq!(
        batches[1].0, first_len,
        "the second message resumes at the first's end"
    );
    assert!(batches[1].1 > 0);
    assert!(last.unwrap().take().unwrap().is_ok());
    let seg = sim.node(1).open_segments()[0].id;
    let path = HotName::Segment(seg).path();
    assert_eq!(
        sim.disk(3).read_all(&path).unwrap(),
        sim.disk(1).read_all(&path).unwrap()
    );
}

#[test]
fn a_local_read_that_fails_while_catching_a_replica_up_drops_that_replica_not_the_write() {
    let (sim, last) = lagging_replica(true);
    let seg = sim.node(1).open_segments()[0].id;
    sim.disk(1).inject(OpKind::ReadAt, 0, Fault::Eio);
    release_to(&sim, 3);
    drain(&sim);
    let third = last.unwrap().take().unwrap().unwrap();
    assert_ne!(
        block_of(&third),
        seg,
        "the segment that lost a replica took the record"
    );
    assert!(sim.node(1).open_segments().iter().all(|s| s.id != seg));
    assert_eq!(
        sim.net.batches(1, 3).len(),
        1,
        "node 3 got only its first batch"
    );
}

#[test]
fn a_writer_that_cannot_stamp_its_batch_sends_nothing_and_answers_503() {
    let mut sim = Sim::new(2, config(2, 2));
    let key = sim.key_on(1, "stamp");
    let h = sim.append(1, &key, b"data");
    // Past the last millisecond a stamp holds.
    sim.clock.advance(1 << 48);
    sim.run();
    assert!(is_too_few_copies(&h.take().unwrap()));
    assert_eq!(sim.net.sent(1, 2, Kind::Replicate), 0);
}

/// A replica that already holds `junk_len` other bytes of the writer's segment when the writer's
/// first batch arrives: the append of one record, with the replica's answer.
fn append_against_a_diverged_copy(junk_len: usize) -> Result<Acked, HotError> {
    let mut sim = Sim::new(2, config(2, 2));
    let (h, seg) = held_append(&mut sim, "diverged", b"data");
    let junk = vec![0x5a_u8; junk_len];
    let batch = ReplicaBatch {
        segment: seg,
        ballot: Ballot::opening(NodeId(1)),
        start: 0,
        running_crc: crc32c_append(0, &junk),
        stamp: sim.stamp(),
        bytes: Bytes::from(junk),
    };
    let n2 = sim.node(2);
    let ack = sim.exec.block(async move { n2.replicate(batch).await });
    assert_eq!(ack.unwrap().committed, junk_len as u64);
    drain(&sim);
    h.take().unwrap()
}

#[test]
fn a_replica_whose_copy_is_longer_than_the_writers_segment_fails_the_batch() {
    assert!(is_too_few_copies(&append_against_a_diverged_copy(100_000)));
}

#[test]
fn a_replica_whose_copy_ends_inside_a_batch_of_the_writer_fails_the_batch() {
    assert!(is_too_few_copies(&append_against_a_diverged_copy(10)));
}

#[test]
fn a_replica_that_refuses_at_the_start_it_was_sent_fails_the_batch() {
    let mut sim = Sim::new(2, config(2, 2));
    sim.disk(2).inject(OpKind::WriteAt, 0, Fault::Eio);
    let key = sim.key_on(1, "refused");
    let h = sim.append(1, &key, b"data");
    sim.run();
    assert!(is_too_few_copies(&h.take().unwrap()));
    assert_eq!(sim.net.sent(1, 2, Kind::Replicate), 1);
}

/// Answers every acknowledgement of a replica with a length one above the one it made durable.
struct Overstating(Arc<dyn HotPeers>);

#[async_trait::async_trait]
impl HotPeers for Overstating {
    async fn replicate(
        &self,
        node: NodeId,
        batch: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError> {
        let mut env = self.0.replicate(node, batch).await?;
        if let ReplicaAnswer::Acked { committed } = &mut env.body {
            *committed += 1;
        }
        Ok(env)
    }

    async fn seal(
        &self,
        node: NodeId,
        msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError> {
        self.0.seal(node, msg).await
    }

    async fn put_large(
        &self,
        node: NodeId,
        offer: Envelope<LargeOffer>,
        body: BoxStream<'static, LargeChunk>,
    ) -> Result<Envelope<LargeAnswer>, PeerError> {
        self.0.put_large(node, offer, body).await
    }

    async fn fetch(
        &self,
        node: NodeId,
        file: HotName,
        range: ByteRange,
    ) -> Result<Bytes, PeerError> {
        self.0.fetch(node, file, range).await
    }
}

#[test]
fn an_acknowledgement_for_another_length_than_was_sent_does_not_count_as_a_copy() {
    let mut sim = Sim::new(2, config(2, 2));
    sim.peers_wrap = Some(Arc::new(|inner| Arc::new(Overstating(inner))));
    sim.crash(1);
    sim.restart(1);
    let key = sim.key_on(1, "overstated");
    let h = sim.append(1, &key, b"data");
    sim.run();
    assert!(is_too_few_copies(&h.take().unwrap()));
    assert_eq!(sim.net.sent(1, 2, Kind::Replicate), 1);
}
