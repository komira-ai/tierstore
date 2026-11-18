//! Section 14 rows "Large object PUT" and the large-file part of "Create-only": the writer tees
//! the body to its `.up` and to each replica, every copy ends as the same
//! `big/<hh>/<khash>.<gen>.<size>.obj`; a body is written in chunks of at most 4 MiB and never
//! buffered whole; a generation trailer mismatch unlinks that copy's `.up` and abandons it; a
//! replica lagging for `put_copy_stall` is replaced by re-sending the finished local file; a
//! client pause abandons nothing; a crash leaves only `.up` files, swept at start; too few
//! copies answer 503; a body that is not its Content-Length fails; the same key and bytes again
//! answer the same generation, other bytes answer 412, and neither leaves a file behind.
//!
//! Catches: a corrupt copy committed after a trailer mismatch; a giant buffer for the whole
//! body; a slow client failing its PUT (or a pause counted as a lagging replica); a lagging
//! replica holding the PUT forever or leaving the copy count short; an `.up` left behind by a
//! failed PUT; a PUT that hangs when its body ends short; a second file for the same bytes; a
//! different body under a used key answered 503 instead of 412.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::stream::{self, BoxStream};
use support::{FakeDev, Handle, Kind, Rendezvous, Sim, config};
use tierstore_fetch_api::clock::Clock;
use tierstore_format::ids::{ExpiryClass, Generation, KeyHash, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::{RECORD_HEADER_LEN, RecordHeader};
use tierstore_format::tomb::LargeFile;
use tierstore_harness::disk::OpKind;
use tierstore_hot::peers::{HotPeers, LargeAnswer, LargeChunk, LargeOffer};
use tierstore_hot::sched::HotSched;
use tierstore_hot::{HotNode, HotParts};
use tierstore_index::backend::mem::MemBackend;
use tierstore_store_api::hot::{Acked, HotError, HotStore, LargePut, ReplicaBatch, Unavailable};
use tierstore_store_api::index::{EntryLocation, Index, Lookup};
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::placement::Placement;
use tierstore_store_api::proto::{Envelope, ReplicaAnswer, SealAnswer, SealMessage};

const MIB: usize = 1 << 20;

type Body = BoxStream<'static, Result<Bytes, io::Error>>;

/// Deterministic bytes that differ per `seed`.
fn data(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(12_345);
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

/// `bytes` cut into pieces of `piece` bytes.
fn pieces(bytes: &[u8], piece: usize) -> Vec<Bytes> {
    bytes.chunks(piece).map(Bytes::copy_from_slice).collect()
}

fn body_of(pieces: Vec<Bytes>) -> Body {
    stream::iter(pieces.into_iter().map(Ok)).boxed()
}

/// A body the test feeds piece by piece: it yields `Pending` until the test sends the next
/// piece or closes it.
struct Gate {
    tx: mpsc::UnboundedSender<Result<Bytes, io::Error>>,
}

impl Gate {
    fn new() -> (Self, Body) {
        let (tx, rx) = mpsc::unbounded();
        (Self { tx }, rx.boxed())
    }
    fn send(&self, bytes: &[u8]) {
        self.tx
            .unbounded_send(Ok(Bytes::copy_from_slice(bytes)))
            .unwrap();
    }
    fn fail(&self) {
        self.tx
            .unbounded_send(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "client gone",
            )))
            .unwrap();
    }
    fn close(self) {
        drop(self);
    }
}

type Put = Handle<Result<Acked, HotError>>;

fn start_put(sim: &mut Sim, id: u8, key: &str, size: Option<u64>, body: Body) -> Put {
    let stamp = sim.stamp();
    let node = sim.node(id);
    let key = key.as_bytes().to_vec();
    sim.exec.start(async move {
        node.put_file(LargePut {
            key,
            size,
            expiry: ExpiryClass(0),
            stamp,
            body,
        })
        .await
    })
}

/// The whole object in one go; its result.
fn put_all(
    sim: &mut Sim,
    id: u8,
    key: &str,
    bytes: &[u8],
    piece: usize,
) -> Result<Acked, HotError> {
    let size = Some(bytes.len() as u64);
    let h = start_put(sim, id, key, size, body_of(pieces(bytes, piece)));
    sim.run();
    h.take().expect("the PUT did not finish")
}

fn large_file(key: &str, bytes: &[u8]) -> LargeFile {
    LargeFile {
        key_hash: KeyHash::of(key.as_bytes()),
        generation: Generation::of(bytes),
        size: bytes.len() as u64,
    }
}

fn obj_path(key: &str, bytes: &[u8]) -> String {
    HotName::Large(large_file(key, bytes)).path()
}

/// The files under `big/` of node `id` with the extension `ext`.
fn files(sim: &Sim, id: u8, ext: &str) -> Vec<String> {
    sim.disk(id)
        .list("big/")
        .unwrap()
        .into_iter()
        .filter(|p| p.ends_with(ext))
        .collect()
}

fn big_is_empty(sim: &Sim, id: u8) -> bool {
    sim.disk(id).list("big/").unwrap().is_empty()
}

/// The bytes of a committed copy: header, key, data.
fn assert_copy(sim: &Sim, id: u8, key: &str, bytes: &[u8]) {
    let path = obj_path(key, bytes);
    let raw = sim
        .disk(id)
        .read_all(&path)
        .unwrap_or_else(|e| panic!("node {id} has no {path}: {e}"));
    let head = RECORD_HEADER_LEN;
    assert_eq!(
        raw.len(),
        head + key.len() + bytes.len(),
        "node {id}: size of the file"
    );
    let header = RecordHeader::decode(&raw[..head]).unwrap();
    assert_eq!(
        header.generation,
        Generation::of(bytes),
        "node {id}: header generation"
    );
    assert_eq!(header.data_len, bytes.len() as u64);
    assert_eq!(usize::from(header.key_len), key.len());
    assert_eq!(
        &raw[head..head + key.len()],
        key.as_bytes(),
        "node {id}: key bytes"
    );
    assert!(
        raw[head + key.len()..] == *bytes,
        "node {id}: data bytes differ"
    );
    assert_eq!(
        files(sim, id, ".obj"),
        vec![path],
        "node {id}: exactly this object"
    );
    assert!(
        files(sim, id, ".up").is_empty(),
        "node {id}: an .up is left"
    );
    match sim.node(id).index().get(key.as_bytes()).unwrap() {
        Lookup::Live(e) => {
            assert_eq!(
                e.location,
                EntryLocation::Large(large_file(key, bytes)),
                "node {id}"
            );
            assert_eq!(e.generation, Generation::of(bytes));
            assert_eq!(e.length, bytes.len() as u64);
        }
        other => panic!("node {id}: index says {other:?}"),
    }
}

fn assert_nothing(sim: &Sim, id: u8, key: &str) {
    assert!(
        big_is_empty(sim, id),
        "node {id}: {:?}",
        sim.disk(id).list("big/").unwrap()
    );
    assert_eq!(
        sim.node(id).index().get(key.as_bytes()).unwrap(),
        Lookup::Absent,
        "node {id}"
    );
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

#[test]
fn the_body_is_teed_to_every_replica_and_all_copies_are_the_same_file() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "tee");
    let bytes = data(200 << 10, 1);
    let acked = put_all(&mut sim, 1, &key, &bytes, 29_000).unwrap();
    assert_eq!(acked.copies, 3);
    assert_eq!(acked.generation, Generation::of(&bytes));
    assert_eq!(
        acked.location,
        EntryLocation::Large(large_file(&key, &bytes))
    );
    assert_eq!(acked.length, bytes.len() as u64);
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
    assert_eq!(sim.net.sent(1, 2, Kind::Large), 1);
    assert_eq!(sim.net.sent(1, 3, Kind::Large), 1);
    // The three copies are byte for byte the same file.
    let path = obj_path(&key, &bytes);
    let first = sim.disk(1).read_all(&path).unwrap();
    assert!(first == sim.disk(2).read_all(&path).unwrap());
    assert!(first == sim.disk(3).read_all(&path).unwrap());
}

#[test]
fn a_put_without_content_length_is_stored_the_same() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(2, "nolen");
    let bytes = data(100 << 10, 2);
    let h = start_put(&mut sim, 2, &key, None, body_of(pieces(&bytes, 10_000)));
    sim.run();
    let acked = h.take().unwrap().unwrap();
    assert_eq!(acked.generation, Generation::of(&bytes));
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
}

#[test]
fn a_body_in_many_small_pieces_is_written_in_chunks_not_buffered_whole() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "chunks");
    let bytes = data(9 * MIB, 3);
    let (gate, body) = Gate::new();
    let put = start_put(&mut sim, 1, &key, None, body);
    sim.run();
    let base = (RECORD_HEADER_LEN + key.len()) as u64;
    let up_len = |sim: &Sim, id: u8| {
        let up = files(sim, id, ".up");
        assert_eq!(up.len(), 1, "node {id}: one upload in progress");
        sim.disk(id).len(&up[0]).unwrap() as u64
    };
    let piece = MIB / 2;
    for (i, p) in bytes.chunks(piece).enumerate() {
        gate.send(p);
        sim.run();
        let fed = ((i + 1) * piece).min(bytes.len()) as u64;
        // The bytes on the device are what the 4 MiB chunks flushed so far: nothing is held
        // back beyond one chunk, and no write is larger than one.
        let flushed = fed / (4 * MIB as u64) * (4 * MIB as u64);
        for id in 1..=3 {
            assert_eq!(
                up_len(&sim, id),
                base + flushed,
                "node {id} after {fed} bytes fed"
            );
        }
    }
    assert!(!put.done(), "the PUT ends with the body");
    gate.close();
    sim.run();
    let acked = put.take().unwrap().unwrap();
    assert_eq!(acked.copies, 3);
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
        // The key, three data chunks (4 MiB, 4 MiB, 1 MiB) and the header: five writes.
        let writes = sim
            .disk(id)
            .ops()
            .iter()
            .filter(|(k, p)| *k == OpKind::WriteAt && p.ends_with(".up"))
            .count();
        assert_eq!(writes, 5, "node {id}: writes of the upload");
    }
}

/// The nodes of one cluster reached by a wire that can corrupt what one node receives.
struct Wire {
    nodes: Mutex<BTreeMap<NodeId, HotNode<MemBackend>>>,
    corrupt: Mutex<BTreeSet<NodeId>>,
}

struct WirePeers {
    wire: Arc<Wire>,
}

fn unreachable(node: NodeId) -> PeerError {
    PeerError::Unreachable {
        node,
        reason: "not wired".into(),
    }
}

#[async_trait::async_trait]
impl HotPeers for WirePeers {
    async fn replicate(
        &self,
        node: NodeId,
        _: Envelope<ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError> {
        Err(unreachable(node))
    }
    async fn seal(
        &self,
        node: NodeId,
        _: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError> {
        Err(unreachable(node))
    }
    async fn put_large(
        &self,
        node: NodeId,
        offer: Envelope<LargeOffer>,
        body: BoxStream<'static, LargeChunk>,
    ) -> Result<Envelope<LargeAnswer>, PeerError> {
        let n = self
            .wire
            .nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&node)
            .cloned()
            .ok_or_else(|| unreachable(node))?;
        let flip = self
            .wire
            .corrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&node);
        let body = if flip {
            // One bit of every data chunk flips on the way: the replica's hash differs.
            body.map(|c| match c {
                LargeChunk::Data(b) => {
                    let mut v = b.to_vec();
                    if let Some(x) = v.first_mut() {
                        *x ^= 1;
                    }
                    LargeChunk::Data(Bytes::from(v))
                }
                trailer => trailer,
            })
            .boxed()
        } else {
            body
        };
        let answer = n.receive_large(offer, body).await;
        Ok(Envelope {
            from: node,
            stamp: n.last_stamp(),
            mem_epoch: 1,
            body: answer,
        })
    }
    async fn fetch(
        &self,
        node: NodeId,
        _: HotName,
        _: tierstore_fetch_api::ids::ByteRange,
    ) -> Result<Bytes, PeerError> {
        Err(unreachable(node))
    }
}

/// Nodes `1..=count` over the disks and indexes of `sim`, joined by a wire that carries large
/// streams only.
fn wired(sim: &Sim, count: u8) -> Arc<Wire> {
    let wire = Arc::new(Wire {
        nodes: Mutex::new(BTreeMap::new()),
        corrupt: Mutex::new(BTreeSet::new()),
    });
    for i in 1..=count {
        let id = NodeId(i);
        let p = &sim.parts[&id];
        let clock: Arc<dyn Clock> = Arc::new(sim.clock.clone());
        let node = HotNode::new(HotParts {
            id,
            config: sim.cfg.clone(),
            dev: Arc::new(FakeDev(p.disk.clone(), Arc::new(true.into()))),
            sched: HotSched::inline(sim.cfg.stall_after_ms, Arc::clone(&clock)),
            index: Arc::clone(&p.index),
            peers: Arc::new(WirePeers {
                wire: Arc::clone(&wire),
            }),
            placement: Arc::new(Rendezvous),
            spawn: Arc::new(sim.exec.clone()),
            clock: Arc::clone(&clock),
            wall: clock,
            metrics: Arc::clone(&p.metrics),
            view: sim.view.clone(),
        });
        let n = node.clone();
        sim.exec
            .block(async move { n.start(None).await })
            .expect("start");
        node.index().set_ready(true);
        wire.nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, node);
    }
    wire
}

fn wire_node(wire: &Wire, id: u8) -> HotNode<MemBackend> {
    wire.nodes.lock().unwrap_or_else(PoisonError::into_inner)[&NodeId(id)].clone()
}

fn offer(sim: &mut Sim, key: &str) -> Envelope<LargeOffer> {
    let stamp = sim.stamp();
    Envelope {
        from: NodeId(1),
        stamp,
        mem_epoch: 1,
        body: LargeOffer {
            key: key.as_bytes().to_vec(),
            size: None,
            expiry: ExpiryClass(0),
            stamp,
        },
    }
}

#[test]
fn a_replica_whose_hash_differs_from_the_trailer_unlinks_its_up_and_commits_nothing() {
    let mut sim = Sim::new(3, config(3, 3));
    let wire = wired(&sim, 3);
    let bytes = data(50 << 10, 4);
    let o = offer(&mut sim, "mismatch");
    let node = wire_node(&wire, 3);
    let wrong = Generation::of(b"not these bytes");
    let chunks = vec![
        LargeChunk::Data(Bytes::copy_from_slice(&bytes[..20_000])),
        LargeChunk::Data(Bytes::copy_from_slice(&bytes[20_000..])),
        LargeChunk::Trailer(wrong),
    ];
    let answer = sim
        .exec
        .block(async move { node.receive_large(o, stream::iter(chunks).boxed()).await });
    assert_eq!(
        answer,
        LargeAnswer::Mismatch {
            computed: Generation::of(&bytes)
        }
    );
    assert_nothing(&sim, 3, "mismatch");
}

#[test]
fn a_stream_that_ends_without_its_trailer_is_abandoned_and_leaves_nothing() {
    let mut sim = Sim::new(3, config(3, 3));
    let wire = wired(&sim, 3);
    let o = offer(&mut sim, "cut-short");
    let node = wire_node(&wire, 2);
    let chunks = vec![LargeChunk::Data(Bytes::from_static(b"some of the body"))];
    let answer = sim
        .exec
        .block(async move { node.receive_large(o, stream::iter(chunks).boxed()).await });
    assert_eq!(answer, LargeAnswer::Abandoned);
    assert_nothing(&sim, 2, "cut-short");
}

#[test]
fn a_corrupted_copy_is_abandoned_and_the_put_is_acknowledged_by_the_others() {
    let mut sim = Sim::new(3, config(3, 2));
    let wire = wired(&sim, 3);
    wire.corrupt.lock().unwrap().insert(NodeId(3));
    let key = sim.key_on(1, "corrupt");
    let bytes = data(300 << 10, 5);
    let node = wire_node(&wire, 1);
    let stamp = sim.stamp();
    let (k, size, b) = (
        key.as_bytes().to_vec(),
        bytes.len() as u64,
        body_of(pieces(&bytes, 50_000)),
    );
    let acked = sim
        .exec
        .block(async move {
            node.put_file(LargePut {
                key: k,
                size: Some(size),
                expiry: ExpiryClass(0),
                stamp,
                body: b,
            })
            .await
        })
        .unwrap();
    assert_eq!(acked.copies, 2, "the corrupt copy is not counted");
    assert_copy(&sim, 1, &key, &bytes);
    assert_copy(&sim, 2, &key, &bytes);
    assert_nothing(&sim, 3, &key);
}

#[test]
fn a_corrupted_copy_with_every_copy_needed_answers_503_and_leaves_no_up_on_the_writer() {
    let mut sim = Sim::new(3, config(3, 3));
    let wire = wired(&sim, 3);
    wire.corrupt.lock().unwrap().insert(NodeId(2));
    let key = sim.key_on(1, "corrupt3");
    let bytes = data(64 << 10, 6);
    let node = wire_node(&wire, 1);
    let stamp = sim.stamp();
    let (k, b) = (key.as_bytes().to_vec(), body_of(pieces(&bytes, 9_000)));
    let r = sim.exec.block(async move {
        node.put_file(LargePut {
            key: k,
            size: None,
            expiry: ExpiryClass(0),
            stamp,
            body: b,
        })
        .await
    });
    assert!(is_too_few_copies(&r), "{r:?}");
    assert!(files(&sim, 1, ".up").is_empty());
    assert!(files(&sim, 1, ".obj").is_empty());
    assert_nothing(&sim, 2, &key);
    assert_eq!(
        sim.node(1).index().get(key.as_bytes()).unwrap(),
        Lookup::Absent
    );
}

/// A 4-node cluster, a key written by node 1, and its replica set split into the nodes that
/// matter: (the other replica, the replica that will lag, the node outside the set).
fn lag_setup() -> (Sim, String, u8, u8, u8) {
    let sim = Sim::new(4, config(3, 3));
    let key = sim.key_on(1, "lag");
    let rank = Rendezvous.rank(key.as_bytes(), &sim.view);
    (sim, key, rank[2].0, rank[1].0, rank[3].0)
}

#[test]
fn a_replica_lagging_for_put_copy_stall_is_replaced_by_the_finished_local_file() {
    let (mut sim, key, other, lagged, spare) = lag_setup();
    let stall = sim.cfg.put_copy_stall_ms;
    sim.net.hold(Some(1), lagged, Kind::Large);
    let bytes = data(8 * 1024, 7);
    let (gate, body) = Gate::new();
    let put = start_put(&mut sim, 1, &key, Some(bytes.len() as u64), body);
    sim.run();
    for p in bytes.chunks(1024).take(5) {
        gate.send(p);
        sim.run();
    }
    assert_eq!(
        sim.net.held(),
        vec![(NodeId(1), NodeId(lagged), Kind::Large)]
    );
    // One millisecond short of the stall: nothing is abandoned.
    sim.clock.advance(stall - 1);
    assert!(sim.node(1).tick().abandoned.is_empty());
    sim.run();
    sim.clock.advance(1);
    assert_eq!(sim.node(1).tick().abandoned, vec![NodeId(lagged)]);
    sim.run();
    for p in bytes.chunks(1024).skip(5) {
        gate.send(p);
        sim.run();
    }
    gate.close();
    sim.run();
    // The abandoned replica's connection finally delivers what it was sent: no trailer.
    sim.net.release(0);
    sim.run();
    let acked = put
        .take()
        .expect("the PUT is waiting for something")
        .unwrap();
    assert_eq!(acked.copies, 3);
    assert_eq!(acked.generation, Generation::of(&bytes));
    for id in [1, other, spare] {
        assert_copy(&sim, id, &key, &bytes);
    }
    assert_nothing(&sim, lagged, &key);
    assert_eq!(sim.net.sent(1, lagged, Kind::Large), 1);
    assert_eq!(
        sim.net.sent(1, spare, Kind::Large),
        1,
        "the finished file is sent again once"
    );
    assert_eq!(sim.net.sent(1, other, Kind::Large), 1);
}

#[test]
fn a_lagging_replica_with_no_spare_node_is_sent_the_finished_file_again() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "lag3");
    let rank = Rendezvous.rank(key.as_bytes(), &sim.view);
    let lagged = rank[1].0;
    sim.net.hold(Some(1), lagged, Kind::Large);
    let bytes = data(6 * 1024, 8);
    let (gate, body) = Gate::new();
    let put = start_put(&mut sim, 1, &key, None, body);
    sim.run();
    for p in bytes.chunks(1024) {
        gate.send(p);
        sim.run();
    }
    sim.clock.advance(sim.cfg.put_copy_stall_ms);
    assert_eq!(sim.node(1).tick().abandoned, vec![NodeId(lagged)]);
    sim.run();
    gate.close();
    sim.run();
    // The second stream to the lagging node is not held.
    sim.net.unhold();
    sim.net.release(0);
    sim.run();
    // No node is outside the set: the lagging one is sent the finished file again and takes it,
    // so the PUT holds all three copies.
    let acked = put.take().unwrap().unwrap();
    assert_eq!(acked.copies, 3);
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
    assert_eq!(sim.net.sent(1, lagged, Kind::Large), 2);
}

#[test]
fn a_client_that_pauses_longer_than_put_copy_stall_abandons_nothing() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "pause");
    let stall = sim.cfg.put_copy_stall_ms;
    let bytes = data(40 << 10, 9);
    let (gate, body) = Gate::new();
    let put = start_put(&mut sim, 1, &key, Some(bytes.len() as u64), body);
    sim.run();
    gate.send(&bytes[..10_000]);
    sim.run();
    for _ in 0..3 {
        sim.clock.advance(stall * 4);
        for id in 1..=3 {
            assert!(
                sim.node(id).tick().abandoned.is_empty(),
                "a pause is not a lagging replica"
            );
        }
        sim.run();
    }
    assert!(!put.done());
    gate.send(&bytes[10_000..]);
    gate.close();
    sim.run();
    let acked = put.take().unwrap().unwrap();
    assert_eq!(acked.copies, 3);
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
    assert_eq!(sim.net.sent(1, 2, Kind::Large), 1);
    assert_eq!(sim.net.sent(1, 3, Kind::Large), 1);
}

#[test]
fn a_crash_mid_put_leaves_only_up_files_and_the_startup_sweep_removes_them() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "crash");
    let bytes = data(30 << 10, 10);
    let (gate, body) = Gate::new();
    let put = start_put(&mut sim, 1, &key, Some(bytes.len() as u64), body);
    sim.run();
    gate.send(&bytes[..12_000]);
    sim.run();
    assert!(!put.done());
    let mut ups = BTreeMap::new();
    for id in 1..=3u8 {
        assert!(
            files(&sim, id, ".obj").is_empty(),
            "node {id}: nothing is committed yet"
        );
        let up = files(&sim, id, ".up");
        assert_eq!(up.len(), 1, "node {id}");
        // The directory entry of the `.up` reached the device before the crash.
        sim.disk(id).fsync_dir("big").unwrap();
        ups.insert(id, up);
    }
    for id in 1..=3 {
        sim.crash(id);
    }
    drop(gate);
    for id in 1..=3 {
        let report = sim.restart(id);
        assert_eq!(
            report.swept, ups[&id],
            "node {id}: the sweep removes the upload"
        );
        assert!(big_is_empty(&sim, id), "node {id}");
        assert_eq!(
            sim.node(id).index().get(key.as_bytes()).unwrap(),
            Lookup::Absent
        );
    }
    assert!(
        put.take().is_none_or(|r| r.is_err()),
        "a crashed PUT is never acknowledged"
    );
}

#[test]
fn too_few_copies_answer_503_and_leave_no_up_behind() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "few");
    sim.net.cut_both(1, 2);
    sim.net.cut_both(1, 3);
    let bytes = data(50 << 10, 11);
    let r = put_all(&mut sim, 1, &key, &bytes, 7_000);
    assert!(is_too_few_copies(&r), "{r:?}");
    for id in 1..=3 {
        assert_nothing(&sim, id, &key);
    }
}

#[test]
fn one_unreachable_replica_still_meets_min_acks_and_the_rest_hold_the_file() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "one-down");
    sim.net.cut_both(1, 3);
    let bytes = data(50 << 10, 12);
    let acked = put_all(&mut sim, 1, &key, &bytes, 7_000).unwrap();
    assert_eq!(acked.copies, 2);
    assert_copy(&sim, 1, &key, &bytes);
    assert_copy(&sim, 2, &key, &bytes);
    assert_nothing(&sim, 3, &key);
}

#[test]
fn a_body_shorter_than_its_content_length_fails_and_leaves_nothing() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "short");
    let bytes = data(60 << 10, 13);
    let h = start_put(
        &mut sim,
        1,
        &key,
        Some(100 << 10),
        body_of(pieces(&bytes, 8_000)),
    );
    sim.run();
    let r = h.take().expect("the PUT hangs on a body that ended short");
    assert!(matches!(r, Err(HotError::Io { .. })), "{r:?}");
    for id in 1..=3 {
        assert_nothing(&sim, id, &key);
    }
}

#[test]
fn a_body_longer_than_its_content_length_fails_and_leaves_nothing() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "long");
    let bytes = data(60 << 10, 14);
    let h = start_put(
        &mut sim,
        1,
        &key,
        Some(10 << 10),
        body_of(pieces(&bytes, 8_000)),
    );
    sim.run();
    let r = h.take().expect("the PUT hangs on a body that ran long");
    assert!(matches!(r, Err(HotError::Io { .. })), "{r:?}");
    for id in 1..=3 {
        assert_nothing(&sim, id, &key);
    }
}

#[test]
fn a_client_that_drops_the_connection_mid_body_fails_the_put_and_leaves_nothing() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "reset");
    let (gate, body) = Gate::new();
    let h = start_put(&mut sim, 1, &key, None, body);
    sim.run();
    gate.send(&data(20 << 10, 15));
    sim.run();
    assert!(!h.done());
    gate.fail();
    sim.run();
    let r = h.take().expect("the PUT hangs on a failed body");
    assert!(matches!(r, Err(HotError::Io { .. })), "{r:?}");
    for id in 1..=3 {
        assert_nothing(&sim, id, &key);
    }
}

#[test]
fn a_second_put_of_the_same_key_and_bytes_is_identical_and_leaves_one_file() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "twice");
    let bytes = data(120 << 10, 16);
    let first = put_all(&mut sim, 1, &key, &bytes, 30_000).unwrap();
    let second = put_all(&mut sim, 1, &key, &bytes, 17_000).unwrap();
    assert_eq!(second.generation, first.generation);
    assert_eq!(second.location, first.location);
    assert_eq!(second.copies, 3);
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
    // The same bytes through another front of the set are the same object too.
    let third = put_all(&mut sim, 2, &key, &bytes, 30_000).unwrap();
    assert_eq!(third.generation, first.generation);
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
}

#[test]
fn a_put_of_other_bytes_under_a_used_key_is_a_conflict_and_leaves_nothing_behind() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "used");
    let bytes = data(120 << 10, 17);
    let first = put_all(&mut sim, 1, &key, &bytes, 30_000).unwrap();
    let other = data(120 << 10, 18);
    let r = put_all(&mut sim, 1, &key, &other, 30_000);
    assert!(
        matches!(r, Err(HotError::Conflict { existing }) if existing == first.generation),
        "{r:?}"
    );
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
    // And the first object is still what a PUT of its bytes finds.
    let again = put_all(&mut sim, 1, &key, &bytes, 30_000).unwrap();
    assert_eq!(again.generation, first.generation);
}

#[test]
fn a_conflicting_put_from_a_replica_front_is_a_conflict_too() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "used2");
    let bytes = data(70 << 10, 19);
    let first = put_all(&mut sim, 1, &key, &bytes, 30_000).unwrap();
    let r = put_all(&mut sim, 3, &key, &data(70 << 10, 20), 30_000);
    assert!(
        matches!(r, Err(HotError::Conflict { existing }) if existing == first.generation),
        "{r:?}"
    );
    for id in 1..=3 {
        assert_copy(&sim, id, &key, &bytes);
    }
}
