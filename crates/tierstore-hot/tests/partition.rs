//! Section 14 row "Two sealers under a partition (deterministic simulation)": the writer A (the
//! first node) and a replica B (the second) of one segment are cut off from each other, B's acknowledgement of
//! A's last batch is lost, and each tries to seal (A early, B with its own ballot through
//! `HotStore::seal`), with every ordering of their prepare and accept messages to the third node
//! C (a depth-first search over the held messages that rebuilds the simulation for each prefix),
//! and with symmetric and asymmetric partitions. In every run at most one seal commits, every
//! copy that can be reached converges to the committed length, a later sealer adopts the
//! committed proposal unchanged, every acknowledged record is within the committed length, and
//! the block table says `sealed` only after the commit. Also with `min_acks = 2` and one
//! acknowledging copy silent, with one copy damaged inside its acknowledged prefix, and for a
//! node alone in its partition, which seals nothing and answers 503.
//!
//! Catches: two sealers committing different lengths; a sealer that ignores an accepted or
//! committed proposal it is told of; a block marked sealed before its seal committed; an
//! acknowledged record cut off by a seal; a copy left longer than, or different from, the
//! committed bytes; a damaged copy counted in Choose; a minority that commits or acknowledges.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::collections::HashSet;

use support::{Handle, Kind, Sim, config};
use tierstore_format::crc32c;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::record::record_len;
use tierstore_format::seal::{Ballot, SealProposal, SealState};
use tierstore_index::blocks::BlockState;
use tierstore_store_api::hot::{
    Acked, Committed, HotError, HotStore, SealReason, SealRefused, Unavailable,
};
use tierstore_store_api::index::{EntryLocation, Index, Lookup};

type Appended = Handle<Result<Acked, HotError>>;
type Sealed = Handle<Result<Committed, HotError>>;

/// Directed links that are cut: what the first node sends to the second is lost.
type Cuts = &'static [(u8, u8)];

const A_B: Cuts = &[(1, 2), (2, 1)];
/// A and B apart, and B cut off from C too: A reaches C, B does not.
const B_OFF_C: Cuts = &[(1, 2), (2, 1), (2, 3), (3, 2)];
/// A and B apart, and A cut off from C too: B reaches C, A does not.
const A_OFF_C: Cuts = &[(1, 2), (2, 1), (1, 3), (3, 1)];
/// A and B apart; C hears both but its answers reach only A.
const C_ANSWERS_A: Cuts = &[(1, 2), (2, 1), (3, 2)];
/// A and B apart; C hears both but its answers reach only B.
const C_ANSWERS_B: Cuts = &[(1, 2), (2, 1), (3, 1)];
/// Every node alone.
const ALL_APART: Cuts = &[(1, 2), (2, 1), (1, 3), (3, 1), (2, 3), (3, 2)];

const B_HIGHER: Ballot = Ballot {
    round: 3,
    node: NodeId(2),
};
/// Above the opening ballot of A's writer, below the ballot A's own seal takes.
const B_LOWER: Ballot = Ballot {
    round: 1,
    node: NodeId(2),
};

/// How the last batch of the segment ends up.
#[derive(Debug, Clone, Copy)]
enum Stage {
    /// Records 1 and 2 are written; B makes record 2 durable but its acknowledgement is lost, and
    /// C either has record 2 too or never got it.
    LostAck { c_has_last: bool },
    /// Only record 1, acknowledged by A and C; B never got a batch (it is silent), and A seals
    /// when its link to B stalls (by `tick` on the clock) or breaks.
    Silent { by_tick: bool },
}

/// What B's own seal must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BSeal {
    Any,
    Minority,
    Commits,
}

#[derive(Debug, Clone, Copy)]
struct Scn {
    min_acks: u8,
    stage: Stage,
    cuts: Cuts,
    b_ballot: Ballot,
    /// A node whose copy has a byte flipped inside record 1, after it was acknowledged.
    damage: Option<u8>,
    b_seal: BSeal,
    /// Whether node 1 has the seal committed and applied before the partition heals.
    a_sealed: Option<bool>,
}

struct Rec {
    key: String,
    data: Vec<u8>,
    offset: u64,
    /// The append of the record, if its answer is still to be read.
    done: Option<Appended>,
    /// Acknowledged before the partition.
    acked: bool,
}

impl Rec {
    fn end(&self) -> u64 {
        self.offset + record_len(self.key.len() as u16, self.data.len() as u64)
    }
}

struct World {
    sim: Sim,
    seg: BlockId,
    recs: Vec<Rec>,
    b_seal: Sealed,
    /// The bytes of record 1 as written, before any damage.
    first_bytes: Vec<u8>,
}

fn path_of(seg: BlockId) -> String {
    HotName::Segment(seg).path()
}

fn data(tag: u8) -> Vec<u8> {
    (0..200u32).map(|i| (i as u8) ^ tag).collect()
}

fn find_held(sim: &Sim, from: u8, to: u8, kind: Kind) -> usize {
    sim.net
        .held()
        .iter()
        .position(|(f, t, k)| *f == NodeId(from) && *t == NodeId(to) && *k == kind)
        .expect("the message is held")
}

/// Loses every held message on a cut link, as its delivery would.
fn drop_cut(sim: &Sim, cuts: Cuts) {
    while let Some(i) = sim
        .net
        .held()
        .iter()
        .position(|(f, t, _)| cuts.contains(&(f.0, t.0)))
    {
        sim.net.drop_held(i);
        sim.run();
    }
}

fn flip_byte(sim: &Sim, node: u8, seg: BlockId, at: usize) {
    let disk = sim.disk(node);
    let path = path_of(seg);
    let old = disk.read_at(&path, at, 1).unwrap()[0];
    disk.write_at(&path, at, &[old ^ 0xff]).unwrap();
    disk.fdatasync(&path).unwrap();
}

/// The simulation of `scn` up to the point where the two sealers have sent their prepares to C:
/// the messages to C are held, A's seal is under way, B's seal is started.
fn build(scn: &Scn) -> World {
    let mut sim = Sim::new(3, config(3, scn.min_acks));
    let Stage::LostAck { c_has_last } = scn.stage else {
        unreachable!("the silent stage is built by build_silent")
    };
    let k1 = "first".to_string();
    let d1 = data(1);
    let h1 = sim.append(1, &k1, &d1);
    sim.run();
    assert!(h1.take().unwrap().is_ok(), "record 1 is acknowledged");
    let seg = sim.node(1).open_segments()[0].id;
    let mut recs = vec![Rec {
        offset: 0,
        done: None,
        acked: true,
        key: k1,
        data: d1,
    }];
    let first_bytes = sim.disk(1).read_all(&path_of(seg)).unwrap();
    if let Some(node) = scn.damage {
        flip_byte(&sim, node, seg, first_bytes.len() / 2);
    }
    sim.net.hold(Some(1), 2, Kind::Replicate);
    if !c_has_last {
        sim.net.hold(Some(1), 3, Kind::Replicate);
    }
    for kind in [Kind::Prepare, Kind::Accept] {
        sim.net.hold(None, 2, kind);
        sim.net.hold(None, 3, kind);
    }
    let k2 = "second".to_string();
    let d2 = data(2);
    let offset = recs[0].end();
    let h2 = sim.append(1, &k2, &d2);
    sim.run();
    if !c_has_last {
        let i = find_held(&sim, 1, 3, Kind::Replicate);
        sim.net.drop_held(i);
        sim.run();
    }
    // B makes record 2 durable; its reply to A is lost.
    sim.net.lose_replies(2, 1, Kind::Replicate, 1);
    let i = find_held(&sim, 1, 2, Kind::Replicate);
    sim.net.release(i);
    sim.run();
    recs.push(Rec {
        key: k2,
        data: d2,
        offset,
        done: Some(h2),
        acked: false,
    });
    partition(&sim, scn);
    let b_seal = start_b(&sim, scn, seg);
    World {
        sim,
        seg,
        recs,
        b_seal,
        first_bytes,
    }
}

/// The silent-copy stage: `min_acks = 2`, record 1 acknowledged by A and C only.
fn build_silent(scn: &Scn) -> World {
    let Stage::Silent { by_tick } = scn.stage else {
        unreachable!("built above")
    };
    let mut sim = Sim::new(3, config(3, scn.min_acks));
    sim.net.hold(Some(1), 2, Kind::Replicate);
    for kind in [Kind::Prepare, Kind::Accept] {
        sim.net.hold(None, 3, kind);
    }
    let k1 = "first".to_string();
    let d1 = data(1);
    let h1 = sim.append(1, &k1, &d1);
    sim.run();
    assert!(
        h1.take().unwrap().is_ok(),
        "A and C are two copies of min_acks = 2"
    );
    let seg = sim.node(1).open_segments()[0].id;
    let first_bytes = sim.disk(1).read_all(&path_of(seg)).unwrap();
    let rec = Rec {
        key: k1,
        data: d1,
        offset: 0,
        done: None,
        acked: true,
    };
    for c in scn.cuts {
        sim.net.cut(c.0, c.1);
    }
    if by_tick {
        // The batch to B is in flight longer than `replica_stall`: the tick closes the segment
        // and seals it early.
        sim.clock.advance(sim.cfg.replica_stall_ms + 1);
        let report = sim.node(1).tick();
        assert_eq!(
            report.early,
            vec![seg],
            "the stalled link seals the segment early"
        );
        sim.run();
    }
    drop_cut(&sim, scn.cuts);
    let b_seal = start_b(&sim, scn, seg);
    World {
        sim,
        seg,
        recs: vec![rec],
        b_seal,
        first_bytes,
    }
}

/// Lifts the staging holds, cuts the links of `scn`, and holds prepares and accepts to C again.
fn partition(sim: &Sim, scn: &Scn) {
    sim.net.unhold();
    for c in scn.cuts {
        sim.net.cut(c.0, c.1);
    }
    for kind in [Kind::Prepare, Kind::Accept] {
        sim.net.hold(None, 3, kind);
    }
    drop_cut(sim, scn.cuts);
}

fn start_b(sim: &Sim, scn: &Scn, seg: BlockId) -> Sealed {
    let node = sim.node(2);
    let ballot = scn.b_ballot;
    let started = sim
        .exec
        .start(async move { node.seal(seg, ballot, SealReason::TakeOver).await });
    sim.run();
    started
}

fn world(scn: &Scn) -> World {
    match scn.stage {
        Stage::LostAck { .. } => build(scn),
        Stage::Silent { .. } => build_silent(scn),
    }
}

// ---------------------------------------------------------------------------------------------
// What the nodes durably say.

fn seal_state(sim: &Sim, node: u8, seg: BlockId) -> Option<SealState> {
    let path = HotName::Seal(seg).path();
    let disk = sim.disk(node);
    if !disk.exists(&path).unwrap() {
        return None;
    }
    Some(SealState::decode(&disk.read_all(&path).unwrap()).unwrap())
}

fn block_state(sim: &Sim, node: u8, seg: BlockId) -> Option<BlockState> {
    sim.node(node).index().block(seg).unwrap().map(|b| b.state)
}

fn file(sim: &Sim, node: u8, seg: BlockId) -> Option<Vec<u8>> {
    let disk = sim.disk(node);
    let path = path_of(seg);
    disk.exists(&path)
        .unwrap()
        .then(|| disk.read_all(&path).unwrap())
}

/// The invariants of any moment: at most one committed proposal, a majority behind it, a block
/// `sealed` only on a node that recorded the commit, and the sealed copies identical. The
/// committed proposal, if there is one.
fn check(sim: &Sim, seg: BlockId) -> Option<SealProposal> {
    let states: Vec<Option<SealState>> = (1..=3).map(|n| seal_state(sim, n, seg)).collect();
    let mut committed: HashSet<SealProposal> = HashSet::new();
    for s in states.iter().flatten() {
        committed.extend(s.committed);
    }
    assert!(committed.len() <= 1, "two seals committed: {committed:?}");
    let chosen = committed.into_iter().next();
    if let Some(p) = chosen {
        let behind = states
            .iter()
            .flatten()
            .filter(|s| s.committed == Some(p) || s.accepted.is_some_and(|(_, a)| a == p))
            .count();
        assert!(behind >= 2, "{p:?} committed without a majority behind it");
    }
    let mut sealed_bytes: Vec<(u8, Vec<u8>)> = Vec::new();
    for n in 1..=3u8 {
        let state = block_state(sim, n, seg);
        let recorded = states[usize::from(n) - 1].is_some_and(|s| s.committed.is_some());
        if state == Some(BlockState::Sealed) {
            assert!(
                recorded,
                "node {n}: the block is sealed before the commit is recorded"
            );
            let p = chosen.expect("sealed implies committed");
            let bytes = file(sim, n, seg).expect("a sealed block has its file");
            assert!(
                bytes.len() as u64 > p.length,
                "node {n}: no trailer after the length"
            );
            assert_eq!(
                crc32c(&bytes[..p.length as usize]),
                p.running_crc,
                "node {n}: the sealed bytes are not the committed ones"
            );
            sealed_bytes.push((n, bytes));
        } else {
            assert!(
                matches!(state, None | Some(BlockState::Open)),
                "node {n}: {state:?}"
            );
        }
    }
    for pair in sealed_bytes.windows(2) {
        assert!(
            pair[0].1 == pair[1].1,
            "the sealed copies of nodes {} and {} differ",
            pair[0].0,
            pair[1].0
        );
    }
    chosen
}

// ---------------------------------------------------------------------------------------------
// The search.

/// Explores every order in which the held messages are delivered; the number of complete runs.
fn explore(scn: &Scn) -> usize {
    let mut leaves = 0;
    let mut stack: Vec<Vec<usize>> = vec![Vec::new()];
    while let Some(prefix) = stack.pop() {
        assert!(prefix.len() < 60, "the seal does not settle: {prefix:?}");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let w = world(scn);
            check(&w.sim, w.seg);
            for &i in &prefix {
                w.sim.net.release(i);
                w.sim.run();
                check(&w.sim, w.seg);
            }
            let held = w.sim.net.held().len();
            if held == 0 {
                finish(scn, w, &prefix);
            }
            held
        }));
        let held = match outcome {
            Ok(held) => held,
            Err(e) => {
                let why = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                panic!("{why}\nin {scn:?} after delivering the held messages {prefix:?}")
            }
        };
        if held == 0 {
            leaves += 1;
        } else {
            stack.extend((0..held).map(|i| {
                let mut p = prefix.clone();
                p.push(i);
                p
            }));
        }
    }
    leaves
}

/// The checks of a run whose messages are all delivered: what the partition left, then the heal
/// and the later sealers.
fn finish(scn: &Scn, mut w: World, order: &[usize]) {
    let seg = w.seg;
    let before = check(&w.sim, seg);
    let b = w
        .b_seal
        .take()
        .unwrap_or_else(|| panic!("B's seal did not finish (order {order:?})"));
    match (scn.b_seal, &b) {
        (BSeal::Any, _) | (BSeal::Commits, Ok(_)) => {}
        (
            BSeal::Minority,
            Err(HotError::Unavailable {
                reason: Unavailable::Minority,
                ..
            }),
        ) => {}
        (_, other) => panic!("B's seal answered {other:?} (order {order:?})"),
    }
    if let Ok(c) = &b {
        assert_eq!(
            Some(c.proposal),
            before,
            "B reports a seal that is not the committed one"
        );
        if file(&w.sim, 2, seg).is_some() {
            assert_eq!(block_state(&w.sim, 2, seg), Some(BlockState::Sealed));
        }
    }
    if let Some(want) = scn.a_sealed {
        assert_eq!(
            block_state(&w.sim, 1, seg) == Some(BlockState::Sealed),
            want,
            "node 1 sealed or not (order {order:?})"
        );
    }
    // The partition heals; every node, in turn, seals with a ballot above all earlier ones.
    w.sim.net.heal();
    w.sim.net.unhold();
    w.sim.run();
    let mut answers = Vec::new();
    for (round, n) in [(50u32, 3u8), (60, 1), (70, 2)] {
        let node = w.sim.node(n);
        let ballot = Ballot {
            round,
            node: NodeId(n),
        };
        let got = w
            .sim
            .exec
            .block(async move { node.seal(seg, ballot, SealReason::TakeOver).await });
        let c =
            got.unwrap_or_else(|e| panic!("node {n}'s later seal failed: {e} (order {order:?})"));
        answers.push(c.proposal);
    }
    assert!(
        answers.windows(2).all(|p| p[0] == p[1]),
        "later sealers disagree: {answers:?}"
    );
    if let Some(p) = before {
        assert_eq!(
            answers[0], p,
            "a later sealer changed the committed proposal (order {order:?})"
        );
    }
    let p = answers[0];
    assert_eq!(check(&w.sim, seg), Some(p));
    let mut final_bytes: Vec<Vec<u8>> = Vec::new();
    for n in 1..=3u8 {
        let Some(bytes) = file(&w.sim, n, seg) else {
            // A node that never held a copy has nothing to converge.
            assert_eq!(n, 2, "only the silent replica may hold no copy");
            continue;
        };
        assert_eq!(
            block_state(&w.sim, n, seg),
            Some(BlockState::Sealed),
            "node {n} did not converge (order {order:?})"
        );
        let own = w.sim.node(n);
        let probe = w.sim.exec.block(async move {
            own.promise(
                seg,
                Ballot {
                    round: 900,
                    node: NodeId(n),
                },
            )
            .await
        });
        assert_eq!(
            probe.err(),
            Some(SealRefused::Committed(p)),
            "node {n} reports another seal"
        );
        final_bytes.push(bytes);
    }
    assert!(
        final_bytes.windows(2).all(|x| x[0] == x[1]),
        "the copies differ after the seal (order {order:?})"
    );
    assert_eq!(
        &final_bytes[0][..w.first_bytes.len().min(p.length as usize)],
        &w.first_bytes[..w.first_bytes.len().min(p.length as usize)],
        "the acknowledged bytes of record 1 changed"
    );
    // Records: acknowledged ones are within the length; every record is visible exactly when it
    // is.
    let recs = std::mem::take(&mut w.recs);
    for (i, rec) in recs.iter().enumerate() {
        // (acknowledged here, acknowledged in another segment after the append moved on)
        let (acked_here, elsewhere) = if rec.acked {
            (true, false)
        } else {
            match rec.done.as_ref().and_then(Handle::take) {
                Some(Ok(a)) => {
                    let here =
                        matches!(a.location, EntryLocation::Block { block, .. } if block == seg);
                    (here, !here)
                }
                Some(Err(_)) => (false, false),
                None => panic!("the append of record {i} did not finish"),
            }
        };
        if acked_here {
            assert!(
                rec.end() <= p.length,
                "acknowledged record {i} ends at {} beyond the seal at {}",
                rec.end(),
                p.length
            );
        }
        for n in 1..=3u8 {
            if elsewhere || file(&w.sim, n, seg).is_none() {
                continue;
            }
            let got = w.sim.node(n).index().get(rec.key.as_bytes()).unwrap();
            if rec.end() <= p.length {
                assert!(
                    matches!(got, Lookup::Live(_)),
                    "record {i} on node {n}: {got:?}"
                );
            } else {
                // Beyond the seal it is not in this segment; a retry of the append may have
                // written it to another one, which a seal there made visible.
                let in_seg = matches!(
                    &got,
                    Lookup::Live(e) if matches!(e.location, EntryLocation::Block { block, .. } if block == seg)
                );
                assert!(
                    !in_seg,
                    "record {i} beyond the seal is visible on node {n}: {got:?}"
                );
            }
        }
    }
}

fn lost_ack(min_acks: u8, c_has_last: bool, cuts: Cuts, b_ballot: Ballot) -> Scn {
    Scn {
        min_acks,
        stage: Stage::LostAck { c_has_last },
        cuts,
        b_ballot,
        damage: None,
        b_seal: BSeal::Any,
        a_sealed: None,
    }
}

fn run_all(scns: &[Scn]) {
    for scn in scns {
        let leaves = explore(scn);
        assert!(leaves >= 1);
    }
}

// ---------------------------------------------------------------------------------------------
// Partitions, `min_acks = replicas`.

#[test]
fn two_sealers_with_a_symmetric_partition_commit_at_most_one_seal_in_every_message_order() {
    let mut all = Vec::new();
    for c_has_last in [true, false] {
        for b_ballot in [B_HIGHER, B_LOWER] {
            all.push(lost_ack(3, c_has_last, A_B, b_ballot));
        }
    }
    run_all(&all);
}

#[test]
fn the_sealers_race_visits_more_than_one_order() {
    let leaves = explore(&lost_ack(3, true, A_B, B_HIGHER));
    assert!(leaves > 2, "the search found only {leaves} complete runs");
}

#[test]
fn a_replica_cut_off_from_the_third_node_cannot_seal_and_the_writer_and_c_commit() {
    let mut all = Vec::new();
    for c_has_last in [true, false] {
        for b_ballot in [B_HIGHER, B_LOWER] {
            let mut s = lost_ack(3, c_has_last, B_OFF_C, b_ballot);
            s.b_seal = BSeal::Minority;
            s.a_sealed = Some(true);
            all.push(s);
        }
    }
    run_all(&all);
}

#[test]
fn a_writer_cut_off_from_the_third_node_stays_open_and_the_replica_and_c_commit() {
    let mut all = Vec::new();
    for c_has_last in [true, false] {
        for b_ballot in [B_HIGHER, B_LOWER] {
            let mut s = lost_ack(3, c_has_last, A_OFF_C, b_ballot);
            s.b_seal = BSeal::Commits;
            s.a_sealed = Some(false);
            all.push(s);
        }
    }
    run_all(&all);
}

#[test]
fn the_third_node_answering_only_the_writer_still_leaves_one_committed_seal() {
    let mut all = Vec::new();
    for c_has_last in [true, false] {
        for b_ballot in [B_HIGHER, B_LOWER] {
            let mut s = lost_ack(3, c_has_last, C_ANSWERS_A, b_ballot);
            // B's prepare reaches C and takes effect, but B never hears: B has no majority.
            s.b_seal = BSeal::Minority;
            all.push(s);
        }
    }
    run_all(&all);
}

#[test]
fn the_third_node_answering_only_the_replica_still_leaves_one_committed_seal() {
    let mut all = Vec::new();
    for c_has_last in [true, false] {
        for b_ballot in [B_HIGHER, B_LOWER] {
            all.push(lost_ack(3, c_has_last, C_ANSWERS_B, b_ballot));
        }
    }
    run_all(&all);
}

#[test]
fn with_every_node_alone_nothing_commits_until_the_partition_heals_and_then_one_seal_does() {
    for c_has_last in [true, false] {
        let mut s = lost_ack(3, c_has_last, ALL_APART, B_HIGHER);
        s.b_seal = BSeal::Minority;
        s.a_sealed = Some(false);
        let leaves = explore(&s);
        assert_eq!(
            leaves, 1,
            "nothing is held when no message can be delivered"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Partitions, `min_acks = 2`.

#[test]
fn with_min_acks_two_and_a_lost_acknowledgement_two_sealers_commit_at_most_one_seal() {
    let mut all = Vec::new();
    for cuts in [A_B, B_OFF_C, A_OFF_C, C_ANSWERS_A, C_ANSWERS_B] {
        for c_has_last in [true, false] {
            all.push(lost_ack(2, c_has_last, cuts, B_HIGHER));
        }
    }
    run_all(&all);
}

#[test]
fn with_min_acks_two_and_a_silent_copy_a_stalled_link_seals_early_and_one_seal_commits() {
    let mut all = Vec::new();
    for by_tick in [true, false] {
        for cuts in [A_B, B_OFF_C, A_OFF_C, C_ANSWERS_A, C_ANSWERS_B] {
            for b_ballot in [B_HIGHER, B_LOWER] {
                all.push(Scn {
                    min_acks: 2,
                    stage: Stage::Silent { by_tick },
                    cuts,
                    b_ballot,
                    damage: None,
                    b_seal: BSeal::Any,
                    a_sealed: None,
                });
            }
        }
    }
    run_all(&all);
}

// ---------------------------------------------------------------------------------------------
// A damaged copy.

#[test]
fn a_copy_damaged_in_its_acknowledged_prefix_is_excluded_and_repaired_whichever_node_it_is() {
    let mut all = Vec::new();
    for node in [1u8, 2, 3] {
        for min_acks in [3u8, 2] {
            for cuts in [A_B, B_OFF_C, A_OFF_C] {
                let mut s = lost_ack(min_acks, false, cuts, B_HIGHER);
                s.damage = Some(node);
                all.push(s);
            }
        }
    }
    run_all(&all);
}

#[test]
fn a_damaged_copy_reports_verified_below_committed_and_is_replaced_by_the_committed_bytes() {
    let mut sim = Sim::new(3, config(3, 3));
    let h = sim.append(1, "kept", &data(7));
    sim.run();
    assert!(h.take().unwrap().is_ok());
    let seg = sim.node(1).open_segments()[0].id;
    let original = sim.disk(1).read_all(&path_of(seg)).unwrap();
    flip_byte(&sim, 3, seg, original.len() / 2);
    let healthy = sim
        .exec
        .block({
            let node = sim.node(2);
            async move {
                node.promise(
                    seg,
                    Ballot {
                        round: 5,
                        node: NodeId(2),
                    },
                )
                .await
            }
        })
        .unwrap();
    let copy = healthy.copy.expect("B holds a copy");
    assert_eq!(
        (copy.verified, copy.committed),
        (original.len() as u64, original.len() as u64)
    );
    let damaged = sim
        .exec
        .block({
            let node = sim.node(3);
            async move {
                node.promise(
                    seg,
                    Ballot {
                        round: 5,
                        node: NodeId(2),
                    },
                )
                .await
            }
        })
        .unwrap();
    let copy = damaged.copy.expect("C holds a copy");
    assert_eq!(copy.committed, original.len() as u64);
    assert!(
        copy.verified < copy.committed,
        "the damage is not reported: {copy:?}"
    );
    // B seals: the damaged copy does not take part in the choice, so the proposal is the one
    // the healthy copies have; C then replaces its bytes.
    let node = sim.node(2);
    let sealed = sim
        .exec
        .block(async move {
            node.seal(
                seg,
                Ballot {
                    round: 6,
                    node: NodeId(2),
                },
                SealReason::TakeOver,
            )
            .await
        })
        .unwrap();
    assert_eq!(sealed.proposal.length, original.len() as u64);
    assert_eq!(sealed.proposal.running_crc, crc32c(&original));
    let on_a = file(&sim, 1, seg).unwrap();
    assert_eq!(file(&sim, 2, seg).unwrap(), on_a);
    assert_eq!(file(&sim, 3, seg).unwrap(), on_a, "C was not repaired");
    assert_eq!(&on_a[..original.len()], &original[..]);
    assert_eq!(block_state(&sim, 3, seg), Some(BlockState::Sealed));
    assert!(matches!(
        sim.node(3).index().get(b"kept").unwrap(),
        Lookup::Live(_)
    ));
}

// ---------------------------------------------------------------------------------------------
// The adopted seal, and a node alone.

#[test]
fn a_later_sealer_adopts_the_commit_and_cuts_its_own_unacknowledged_tail() {
    // A and C commit at A's last acknowledged offset while B holds one more, unacknowledged,
    // record; B then seals with a higher ballot and gets the same proposal.
    let mut scn = lost_ack(3, false, B_OFF_C, B_HIGHER);
    scn.b_seal = BSeal::Minority;
    let w = world(&scn);
    while !w.sim.net.held().is_empty() {
        w.sim.net.release(0);
        w.sim.run();
    }
    let first_end = w.recs[0].end();
    let committed = check(&w.sim, w.seg).expect("A and C committed");
    assert_eq!(
        committed.length, first_end,
        "the seal is at the last acknowledged offset"
    );
    assert_eq!(block_state(&w.sim, 1, w.seg), Some(BlockState::Sealed));
    assert_eq!(block_state(&w.sim, 3, w.seg), Some(BlockState::Sealed));
    assert_eq!(
        block_state(&w.sim, 2, w.seg),
        Some(BlockState::Open),
        "B has not heard of the commit"
    );
    assert!(
        file(&w.sim, 2, w.seg).unwrap().len() as u64 > first_end,
        "B holds the unacknowledged record"
    );
    w.sim.net.heal();
    w.sim.net.unhold();
    let node = w.sim.node(2);
    let seg = w.seg;
    let again = w
        .sim
        .exec
        .block(async move {
            node.seal(
                seg,
                Ballot {
                    round: 9,
                    node: NodeId(2),
                },
                SealReason::TakeOver,
            )
            .await
        })
        .unwrap();
    assert_eq!(
        again.proposal, committed,
        "the later sealer changed the committed proposal"
    );
    assert_eq!(block_state(&w.sim, 2, w.seg), Some(BlockState::Sealed));
    assert_eq!(file(&w.sim, 2, w.seg), file(&w.sim, 1, w.seg));
    for n in 1..=3u8 {
        let got = w.sim.node(n).index().get(w.recs[1].key.as_bytes()).unwrap();
        assert_eq!(
            got,
            Lookup::Absent,
            "the unacknowledged record is visible on node {n}"
        );
        let kept = w.sim.node(n).index().get(w.recs[0].key.as_bytes()).unwrap();
        assert!(matches!(kept, Lookup::Live(_)), "record 1 on node {n}");
    }
    let lost = w.recs[1]
        .done
        .as_ref()
        .unwrap()
        .take()
        .expect("record 2's append finished");
    assert!(lost.is_err(), "record 2 was never acknowledged: {lost:?}");
}

#[test]
fn the_block_stays_open_until_the_commit_and_is_sealed_only_after_it() {
    let scn = lost_ack(3, true, A_B, B_HIGHER);
    let w = world(&scn);
    for n in 1..=3 {
        assert_eq!(
            block_state(&w.sim, n, w.seg),
            Some(BlockState::Open),
            "node {n} before any prepare is answered"
        );
        assert!(seal_state(&w.sim, n, w.seg).is_none_or(|s| s.committed.is_none()));
    }
    // Deliver the prepares one at a time; the accepts come after a majority promised.
    let mut committed_seen = false;
    while !w.sim.net.held().is_empty() {
        let kind = w.sim.net.held()[0].2;
        w.sim.net.release(0);
        w.sim.run();
        let chosen = check(&w.sim, w.seg);
        if chosen.is_none() {
            assert!(
                (1..=3).all(|n| block_state(&w.sim, n, w.seg) != Some(BlockState::Sealed)),
                "a block is sealed after a {kind:?} and before a commit"
            );
        } else {
            committed_seen = true;
        }
    }
    assert!(committed_seen, "no seal committed");
}

#[test]
fn a_node_alone_in_its_partition_seals_nothing_and_its_puts_answer_503() {
    for min_acks in [3u8, 2] {
        let mut sim = Sim::new(3, config(3, min_acks));
        for (a, b) in ALL_APART {
            sim.net.cut(*a, *b);
        }
        let first = sim.append(1, "alone", &data(3));
        sim.run();
        let err = first.take().unwrap().unwrap_err();
        assert!(matches!(err, HotError::Unavailable { .. }), "{err:?}");
        let again = sim.append(1, "alone-too", &data(4));
        sim.run();
        let err = again.take().unwrap().unwrap_err();
        assert!(matches!(err, HotError::Unavailable { .. }), "{err:?}");
        let blocks = sim.node(1).index().blocks(None, 10).unwrap();
        assert_eq!(blocks.len(), 1, "one segment was opened");
        let seg = blocks[0].0;
        let node = sim.node(1);
        let sealed = sim.exec.block(async move {
            node.seal(
                seg,
                Ballot {
                    round: 2,
                    node: NodeId(1),
                },
                SealReason::Early,
            )
            .await
        });
        assert!(
            matches!(
                sealed,
                Err(HotError::Unavailable {
                    reason: Unavailable::Minority,
                    ..
                })
            ),
            "{sealed:?}"
        );
        assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Open));
        assert!(
            seal_state(&sim, 1, seg).is_none_or(|s| s.committed.is_none() && s.accepted.is_none())
        );
        for key in ["alone", "alone-too"] {
            assert_eq!(
                sim.node(1).index().get(key.as_bytes()).unwrap(),
                Lookup::Absent
            );
        }
    }
}
