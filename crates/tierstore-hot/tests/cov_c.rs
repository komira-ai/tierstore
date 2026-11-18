//! Section 14 rows "Seal by majority" and the hot scrub, at their failure edges: a voter's seal
//! state written over a stale temporary file, a promise over a copy that is empty, cut inside a
//! record or on a record boundary, or whose length cannot be read; a converge whose peers cannot
//! supply the committed bytes, whose committed crc matches no copy, whose device cannot say how
//! long the copy is, or whose copy ends before the length the device claimed; and a scrub of a
//! truncated copy, of a copy whose repair file is stale, of a file dropped while its repair is on
//! the wire, and of a large file whose header names another size.
//!
//! Catches: a seal state that cannot be rewritten after a crash left its temporary file; a
//! promise that answers a copy's state it could not read; a verified prefix that counts a record
//! cut by the end of the file or misses the records before the cut; a converge that seals a copy
//! whose bytes no peer can confirm, accepts a peer's bytes whose crc is not the committed one,
//! or reads past the end of a copy; a scrub that takes a short file for a good one, trips on a
//! stale `.repair` file, or brings back the scrub time of a file the node no longer holds; a large
//! file whose header disagrees with its name passing the scrub.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use support::{Exec, FakeDev, Kind, Rendezvous, Sim, config, view};
use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::crc32c;
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::{
    RECORD_HEADER_LEN, RecordFlags, RecordHeader, RecordIn, encode_record, scan,
};
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::stamp::Stamp;
use tierstore_harness::disk::{FakeDisk, Fault, OpKind};
use tierstore_hot::dev::HotDev;
use tierstore_hot::peers::{HotPeers, LargeAnswer, LargeChunk, LargeOffer};
use tierstore_hot::sched::HotSched;
use tierstore_hot::{HotNode, HotParts};
use tierstore_index::backend::mem::MemBackend;
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_metrics::{HotStoreConfig, Metrics};
use tierstore_store_api::hot::{
    Acked, HotError, HotStore, LargePut, SealReason, SealRefused, Unavailable,
};
use tierstore_store_api::index::EntryLocation;
use tierstore_store_api::peer::PeerError;
use tierstore_store_api::proto::{Envelope, ReplicaAnswer, SealAnswer, SealMessage};

const WEEK_MS: u64 = 7 * 24 * 3600 * 1000;

// ---------------------------------------------------------------------------------------------
// Helpers.

fn seg(n: u64) -> BlockId {
    BlockId::new(NodeId(1), n).unwrap()
}

fn ballot(round: u32) -> Ballot {
    Ballot {
        round,
        node: NodeId(1),
    }
}

fn bytes_of(n: usize, salt: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt))
        .collect()
}

/// The bytes of record `i`: key `k<i>`, `len` data bytes.
fn record(i: u8, len: usize) -> Vec<u8> {
    let data = bytes_of(len, i);
    encode_record(&RecordIn {
        flags: RecordFlags::default(),
        key: format!("k{i}").as_bytes(),
        data: &data,
        generation: tierstore_format::ids::Generation::of(&data),
        stamp: Stamp::from_parts(1_000_000 + u64::from(i), 0).unwrap(),
        object_offset: 0,
    })
    .unwrap()
}

fn proposal(bytes: &[u8]) -> SealProposal {
    SealProposal {
        length: bytes.len() as u64,
        running_crc: crc32c(bytes),
    }
}

/// A file that survives a crash: created, written, synced, its directory synced.
fn durable_file(disk: &FakeDisk, path: &str, bytes: &[u8]) {
    disk.create(path).unwrap();
    disk.write_at(path, 0, bytes).unwrap();
    disk.fdatasync(path).unwrap();
    disk.fsync_dir(tierstore_hot::dev::dir_of(path)).unwrap();
}

fn flip(disk: &FakeDisk, path: &str, offset: usize) {
    let byte = disk.read_at(path, offset, 1).unwrap()[0];
    disk.write_at(path, offset, &[byte ^ 0xff]).unwrap();
}

fn put(sim: &mut Sim, id: u8, key: &str, data: &[u8]) -> Result<Acked, HotError> {
    let h = sim.append(id, key, data);
    sim.run();
    h.take().expect("the append did not finish")
}

/// Two records put on node 1 (replicated to 2 and 3): the open segment's id and path.
fn open_segment(sim: &mut Sim) -> (BlockId, String) {
    let k1 = sim.key_on(1, "cov");
    put(sim, 1, &k1, &bytes_of(3_000, 1)).unwrap();
    let k2 = sim.key_on(1, "cov2");
    put(sim, 1, &k2, &bytes_of(2_000, 2)).unwrap();
    let id = sim.node(1).open_segments()[0].id;
    (id, HotName::Segment(id).path())
}

/// [`open_segment`], sealed on every node: its id and path.
fn sealed_segment(sim: &mut Sim) -> (BlockId, String) {
    let (id, path) = open_segment(sim);
    let node = sim.node(1);
    sim.exec
        .block(async move { node.seal(id, ballot(2), SealReason::Size).await })
        .expect("seal");
    (id, path)
}

fn sealed(sim: &Sim, id: u8, segment: BlockId, p: SealProposal) -> Result<(), HotError> {
    let node = sim.node(id);
    sim.exec
        .block(async move { node.sealed(segment, p).await })
}

fn too_few_copies(r: &Result<(), HotError>) -> bool {
    matches!(
        r,
        Err(HotError::Unavailable {
            reason: Unavailable::TooFewCopies,
            ..
        })
    )
}

fn scrub(sim: &Sim, id: u8) -> tierstore_hot::node::ScrubReport {
    let node = sim.node(id);
    sim.exec
        .block(async move { node.scrub().await })
        .expect("scrub")
}

// ---------------------------------------------------------------------------------------------
// A voter's seal state.

#[test]
fn a_vote_is_written_over_the_temporary_file_a_crash_left_behind() {
    let mut sim = Sim::new(3, config(3, 3));
    let tmp = format!("{}.tmp", HotName::Seal(seg(1)).path());
    durable_file(&sim.disk(2), &tmp, b"half a vote, then the crash");
    let p = proposal(&record(1, 100));
    let node = sim.node(2);
    sim.exec
        .block(async move { node.accept(seg(1), ballot(2), p).await })
        .unwrap();
    assert!(!sim.disk(2).exists(&tmp).unwrap(), "renamed over the seal");
    // The vote is durable: it survives a crash and is promised back.
    sim.crash(2);
    sim.restart(2);
    let node = sim.node(2);
    let promise = sim
        .exec
        .block(async move { node.promise(seg(1), ballot(3)).await })
        .unwrap();
    assert_eq!(promise.accepted, Some((ballot(2), p)));
}

#[test]
fn a_promise_whose_copy_length_cannot_be_read_is_refused_naming_the_ballot() {
    let sim = Sim::new(3, config(3, 3));
    sim.disk(2).inject(OpKind::Len, 0, Fault::Eio);
    let node = sim.node(2);
    let got = sim
        .exec
        .block(async move { node.promise(seg(1), ballot(2)).await });
    assert!(matches!(
        got,
        Err(SealRefused::Promised { promised }) if promised == ballot(2)
    ));
}

#[test]
fn a_vote_that_cannot_create_its_temporary_file_is_refused_naming_the_ballot() {
    let sim = Sim::new(3, config(3, 3));
    sim.disk(2).inject(OpKind::Create, 0, Fault::Eio);
    let p = proposal(&record(1, 100));
    let node = sim.node(2);
    let got = sim
        .exec
        .block(async move { node.accept(seg(1), ballot(2), p).await });
    assert!(matches!(
        got,
        Err(SealRefused::Promised { promised }) if promised == ballot(2)
    ));
    // Nothing was promised: a lower ballot is still accepted.
    let node = sim.node(2);
    sim.exec
        .block(async move { node.accept(seg(1), ballot(1), p).await })
        .unwrap();
}

#[test]
fn a_promise_verifies_a_copy_followed_by_zeros_up_to_its_last_record() {
    let sim = Sim::new(3, config(3, 3));
    let rec = record(1, 500);
    let padded = [rec.clone(), vec![0u8; 64]].concat();
    durable_file(&sim.disk(2), &HotName::Segment(seg(6)).path(), &padded);
    let node = sim.node(2);
    let promise = sim
        .exec
        .block(async move { node.promise(seg(6), ballot(2)).await })
        .unwrap();
    let copy = promise.copy.unwrap();
    assert_eq!(copy.committed, padded.len() as u64);
    assert_eq!(copy.verified, rec.len() as u64);
    assert_eq!(copy.running_crc, crc32c(&rec));
}

#[test]
fn a_promise_reports_an_empty_copy_cut_on_a_record_or_inside_one_by_its_verified_prefix() {
    let mut sim = Sim::new(3, config(3, 3));
    // An empty copy: nothing to verify.
    durable_file(&sim.disk(2), &HotName::Segment(seg(7)).path(), b"");
    let node = sim.node(2);
    let promise = sim
        .exec
        .block(async move { node.promise(seg(7), ballot(2)).await })
        .unwrap();
    let copy = promise.copy.expect("an empty copy is a copy");
    assert_eq!((copy.committed, copy.verified, copy.running_crc), (0, 0, 0));

    // Two records everywhere; node 2's file is cut inside the second record, node 3's on the
    // boundary between them.
    let (id, path) = open_segment(&mut sim);
    let whole = sim.disk(1).read_all(&path).unwrap();
    let first = scan(&whole).records[1].0;
    sim.disk(2).set_len(&path, first as usize + 20).unwrap();
    sim.disk(3).set_len(&path, first as usize).unwrap();
    for n in [2u8, 3] {
        let node = sim.node(n);
        let promise = sim
            .exec
            .block(async move { node.promise(id, ballot(5)).await })
            .unwrap();
        let copy = promise.copy.unwrap();
        assert_eq!(copy.committed, whole.len() as u64, "node {n}");
        assert_eq!(copy.verified, first, "node {n}: only the first record holds");
        assert_eq!(
            copy.running_crc,
            crc32c(&whole[..first as usize]),
            "node {n}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Converge.

#[test]
fn a_sealed_notice_whose_copy_length_cannot_be_read_fails_and_seals_nothing() {
    let sim = Sim::new(3, config(3, 3));
    let bytes = [record(1, 500), record(2, 500)].concat();
    durable_file(&sim.disk(1), &HotName::Segment(seg(3)).path(), &bytes);
    sim.disk(1).inject(OpKind::Len, 0, Fault::Eio);
    let got = sealed(&sim, 1, seg(3), proposal(&bytes));
    assert!(matches!(
        got,
        Err(HotError::Io {
            op: "segment length",
            ..
        })
    ));
    assert!(
        !sim.disk(1)
            .exists(&HotName::BlockIndex(seg(3)).path())
            .unwrap()
    );
}

#[test]
fn a_short_copy_with_peers_that_hold_no_more_is_not_sealed() {
    let sim = Sim::new(3, config(3, 3));
    let first = record(1, 500);
    let all = [first.clone(), record(2, 500), record(3, 500)].concat();
    let path = HotName::Segment(seg(4)).path();
    for n in [1u8, 2, 3] {
        durable_file(&sim.disk(n), &path, &first);
    }
    let got = sealed(&sim, 1, seg(4), proposal(&all));
    assert!(too_few_copies(&got), "{got:?}");
    assert_eq!(sim.disk(1).read_all(&path).unwrap(), first, "left as it was");
    assert!(
        !sim.disk(1)
            .exists(&HotName::BlockIndex(seg(4)).path())
            .unwrap()
    );
}

#[test]
fn a_copy_whose_crc_is_not_the_committed_one_is_not_replaced_by_peers_that_disagree_too() {
    let sim = Sim::new(3, config(3, 3));
    let all = [record(1, 500), record(2, 500)].concat();
    let path = HotName::Segment(seg(5)).path();
    for n in [1u8, 2, 3] {
        durable_file(&sim.disk(n), &path, &all);
    }
    let wrong = SealProposal {
        running_crc: crc32c(&all) ^ 1,
        ..proposal(&all)
    };
    let got = sealed(&sim, 1, seg(5), wrong);
    assert!(too_few_copies(&got), "{got:?}");
    assert_eq!(sim.disk(1).read_all(&path).unwrap(), all, "left as it was");
}

// ---------------------------------------------------------------------------------------------
// A node on a device that misreports, with no peers.

/// A device whose `len` claims more than the file holds, and whose reads of the segment file
/// answer empty once `reads` of them have passed.
struct Lie {
    inner: FakeDev,
    extra_len: AtomicU64,
    reads: AtomicU64,
    watched: String,
}

impl HotDev for Lie {
    fn create(&self, path: &str) -> io::Result<()> {
        self.inner.create(path)
    }
    fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_at(path, offset, bytes)
    }
    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        if path == self.watched {
            let left = self.reads.load(Ordering::SeqCst);
            if left == 0 {
                return Ok(Vec::new());
            }
            self.reads.store(left - 1, Ordering::SeqCst);
        }
        self.inner.read_at(path, offset, len)
    }
    fn len(&self, path: &str) -> io::Result<u64> {
        Ok(self.inner.len(path)? + self.extra_len.load(Ordering::SeqCst))
    }
    fn set_len(&self, path: &str, len: u64) -> io::Result<()> {
        self.inner.set_len(path, len)
    }
    fn allocate(&self, path: &str, len: u64) -> io::Result<()> {
        self.inner.allocate(path, len)
    }
    fn fdatasync(&self, path: &str) -> io::Result<()> {
        self.inner.fdatasync(path)
    }
    fn fsync_dir(&self, dir: &str) -> io::Result<()> {
        self.inner.fsync_dir(dir)
    }
    fn rename(&self, from: &str, to: &str, noreplace: bool) -> io::Result<()> {
        self.inner.rename(from, to, noreplace)
    }
    fn unlink(&self, path: &str) -> io::Result<()> {
        self.inner.unlink(path)
    }
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        self.inner.list(dir)
    }
}

/// Peers that answer nothing.
struct Nobody;

fn gone(node: NodeId) -> PeerError {
    PeerError::Unreachable {
        node,
        reason: "no peers".into(),
    }
}

#[async_trait::async_trait]
impl HotPeers for Nobody {
    async fn replicate(
        &self,
        node: NodeId,
        _batch: Envelope<tierstore_store_api::hot::ReplicaBatch>,
    ) -> Result<Envelope<ReplicaAnswer>, PeerError> {
        Err(gone(node))
    }
    async fn seal(
        &self,
        node: NodeId,
        _msg: Envelope<SealMessage>,
    ) -> Result<Envelope<SealAnswer>, PeerError> {
        Err(gone(node))
    }
    async fn put_large(
        &self,
        node: NodeId,
        _offer: Envelope<LargeOffer>,
        _body: BoxStream<'static, LargeChunk>,
    ) -> Result<Envelope<LargeAnswer>, PeerError> {
        Err(gone(node))
    }
    async fn fetch(
        &self,
        node: NodeId,
        _file: tierstore_format::names::HotName,
        _range: ByteRange,
    ) -> Result<Bytes, PeerError> {
        Err(gone(node))
    }
}

struct Lab {
    exec: Exec,
    disk: FakeDisk,
    dev: Arc<Lie>,
    node: HotNode<MemBackend>,
}

fn lab(watched: &str) -> Lab {
    let cfg = config(1, 1);
    let exec = Exec::default();
    let disk = FakeDisk::new();
    let clock = ManualClock::new();
    clock.advance(1_000_000);
    let clk: Arc<dyn Clock> = Arc::new(clock);
    let dev = Arc::new(Lie {
        inner: FakeDev(disk.clone(), Arc::new(std::sync::atomic::AtomicBool::new(true))),
        extra_len: AtomicU64::new(0),
        reads: AtomicU64::new(u64::MAX),
        watched: watched.to_owned(),
    });
    let metrics = Metrics::new("test")
        .hot_store(&HotStoreConfig {
            device: DriveId([1; 16]),
            replicas: 1,
            peers: Vec::new(),
        })
        .hot;
    let index = Arc::new(
        KvIndex::open(
            MemBackend::new(),
            IndexConfig {
                node: NodeId(1),
                geometry: cfg.geometry,
            },
        )
        .unwrap(),
    );
    let dyn_dev: Arc<dyn HotDev> = dev.clone();
    let node = HotNode::new(HotParts {
        id: NodeId(1),
        sched: HotSched::inline(cfg.stall_after_ms, Arc::clone(&clk)),
        view: view(1, &cfg),
        config: cfg,
        dev: dyn_dev,
        index,
        peers: Arc::new(Nobody),
        placement: Arc::new(Rendezvous),
        spawn: Arc::new(exec.clone()),
        clock: Arc::clone(&clk),
        wall: clk,
        metrics,
    });
    let n = node.clone();
    exec.block(async move { n.start(None).await })
        .expect("start");
    node.index().set_ready(true);
    Lab {
        exec,
        disk,
        dev,
        node,
    }
}

impl Lab {
    fn sealed(&self, segment: BlockId, p: SealProposal) -> Result<(), HotError> {
        let node = self.node.clone();
        self.exec
            .block(async move { node.sealed(segment, p).await })
    }
}

#[test]
fn a_copy_that_ends_before_the_length_the_device_claims_is_not_taken_for_the_committed_one() {
    let path = HotName::Segment(seg(8)).path();
    let lab = lab(&path);
    let first = [record(1, 500), record(2, 500)].concat();
    let all = [first.clone(), record(3, 500)].concat();
    durable_file(&lab.disk, &path, &first);
    // `len` says the copy is long enough, its bytes end after two records of the three.
    lab.dev.extra_len.store(4_096, Ordering::SeqCst);
    let got = lab.sealed(seg(8), proposal(&all));
    assert!(too_few_copies(&got), "{got:?}");
    assert_eq!(lab.disk.read_all(&path).unwrap(), first, "left as it was");
}

#[test]
fn a_copy_that_stops_answering_reads_while_its_trailer_is_built_is_not_sealed() {
    let path = HotName::Segment(seg(9)).path();
    let lab = lab(&path);
    let all = [record(1, 500), record(2, 500)].concat();
    durable_file(&lab.disk, &path, &all);
    // The crc check reads the copy once; the trailer's read then finds nothing.
    lab.dev.reads.store(1, Ordering::SeqCst);
    let got = lab.sealed(seg(9), proposal(&all));
    assert!(too_few_copies(&got), "{got:?}");
    assert!(
        !lab.disk
            .exists(&HotName::BlockIndex(seg(9)).path())
            .unwrap()
    );
}

// ---------------------------------------------------------------------------------------------
// Scrub.

#[test]
fn a_truncated_segment_is_found_bad_and_repaired_over_a_stale_repair_file() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, path) = sealed_segment(&mut sim);
    let good = sim.disk(2).read_all(&path).unwrap();
    sim.disk(1).set_len(&path, 100).unwrap();
    let repair = format!("{path}.repair");
    durable_file(&sim.disk(1), &repair, b"a repair that never finished");
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!(
        (report.checked, report.bad, report.repaired, report.dropped),
        (1, 1, 1, 0)
    );
    assert_eq!(sim.disk(1).read_all(&path).unwrap(), good);
    assert!(!sim.disk(1).exists(&repair).unwrap());
}

#[test]
fn a_repair_that_cannot_create_its_file_fails_the_scrub_and_keeps_the_bad_copy() {
    let mut sim = Sim::new(3, config(3, 3));
    let (_, path) = sealed_segment(&mut sim);
    sim.disk(1).set_len(&path, 100).unwrap();
    sim.clock.advance(WEEK_MS);
    sim.disk(1).inject(OpKind::Create, 0, Fault::Eio);
    let node = sim.node(1);
    let got = sim.exec.block(async move { node.scrub().await });
    assert!(matches!(got, Err(HotError::Io { op: "replace copy", .. })));
    assert_eq!(sim.disk(1).len(&path).unwrap(), 100);
}

#[test]
fn a_file_dropped_while_its_repair_is_on_the_wire_does_not_fail_the_scrub() {
    let mut cfg = config(3, 3);
    cfg.keep_clean = false;
    let mut sim = Sim::new(3, cfg);
    let (id, path) = sealed_segment(&mut sim);
    flip(&sim.disk(1), &path, RECORD_HEADER_LEN + 20);
    sim.clock.advance(WEEK_MS);
    sim.net.hold(Some(1), 2, Kind::Fetch);
    let node = sim.node(1);
    let scrubbing = sim.exec.start(async move { node.scrub().await });
    sim.run();
    assert_eq!(sim.net.held().len(), 1, "the repair waits for node 2");
    // The node drops the file meanwhile.
    let node = sim.node(1);
    let dropped = sim
        .exec
        .block(async move { node.mark_clean(HotName::Segment(id)).await })
        .unwrap();
    assert!(dropped);
    sim.net.unhold();
    sim.net.release(0);
    sim.run();
    let report = scrubbing.take().expect("the scrub finished").unwrap();
    assert_eq!((report.checked, report.bad, report.repaired), (1, 1, 1));
}

#[test]
fn a_large_file_whose_header_names_another_size_is_found_bad_and_repaired() {
    let mut sim = Sim::new(3, config(3, 3));
    let data = bytes_of(100_000, 7);
    let put = LargePut {
        key: b"a-large-object".to_vec(),
        size: Some(data.len() as u64),
        expiry: ExpiryClass(0),
        stamp: sim.stamp(),
        body: futures::stream::iter(
            data.chunks(40_000)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
        .boxed(),
    };
    let node = sim.node(1);
    let h = sim.exec.start(async move { node.put_file(put).await });
    sim.run();
    let acked = h.take().expect("the put finished").unwrap();
    let EntryLocation::Large(file) = acked.location else {
        panic!("not a large file: {:?}", acked.location);
    };
    let path = HotName::Large(file).path();
    let good = sim.disk(2).read_all(&path).unwrap();
    let header = RecordHeader::decode(&good).unwrap();
    let other = RecordHeader {
        data_len: header.data_len + 1,
        ..header
    };
    sim.disk(1).write_at(&path, 0, &other.encode()).unwrap();
    sim.clock.advance(WEEK_MS);
    let report = scrub(&sim, 1);
    assert_eq!((report.checked, report.bad, report.repaired), (1, 1, 1));
    assert_eq!(sim.disk(1).read_all(&path).unwrap(), good);
}
