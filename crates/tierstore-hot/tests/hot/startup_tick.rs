//! Startup, the periodic tick and the sealer at the edges the other suites do not reach: stray
//! files in the hot directories, a restart in a minority whose seals the tick retries, a second
//! start on a running node, a stalled device, a closed segment whose seal did not commit, a
//! sealer that is not a voter, lost accepts, and a cluster stamp that cannot be assigned.
//!
//! Catches: a startup that fails on, sweeps or registers a file it does not know; a restart in
//! a minority that reports the segment sealed, runs the index recovery, or is never sealed
//! once the tick can reach a majority; two seals of one segment running at once; a start on a
//! running node that forgets a sealed segment or re-seals it; a stalled device reported as
//! anything but a hot-device error, or never recovering; a closed segment whose seal did not
//! commit and is not retried after `replica_stall`; a sealer outside the voters that counts
//! itself or never commits; an accept nobody answered reported as committed; a stamp error that
//! stops a seal from committing.

use crate::support::{Kind, Sim, config};
use tierstore_fetch_api::upstream::DriveState;
use tierstore_format::ids::{BlockId, NodeId};
use tierstore_format::seal::Ballot;
use tierstore_format::stamp::WALL_MAX;
use tierstore_harness::disk::{FakeDisk, Fault, OpKind};
use tierstore_index::blocks::BlockState;
use tierstore_store_api::hot::{Acked, Committed, HotError, HotStore, SealReason, Unavailable};
use tierstore_store_api::index::Lookup;
use tierstore_store_api::sched::DriveClass;
use tierstore_store_api::view::{Admin, NodeState};

fn put(sim: &mut Sim, id: u8, key: &str, data: &[u8]) -> Acked {
    let h = sim.append(id, key, data);
    sim.run();
    h.take()
        .expect("the append finished")
        .expect("acknowledged")
}

fn live(sim: &Sim, id: u8, key: &str) -> bool {
    matches!(
        sim.node(id).index().get(key.as_bytes()),
        Ok(Lookup::Live(_))
    )
}

fn durable_file(disk: &FakeDisk, path: &str) {
    disk.create(path).unwrap();
    disk.write_at(path, 0, &[7, 7, 7]).unwrap();
    disk.fdatasync(path).unwrap();
    disk.fsync_dir(tierstore_hot::dev::dir_of(path)).unwrap();
}

fn open_id(sim: &Sim, id: u8) -> BlockId {
    let open = sim.node(id).open_segments();
    assert_eq!(open.len(), 1);
    open[0].id
}

fn seal(
    sim: &Sim,
    id: u8,
    segment: BlockId,
    round: u32,
) -> crate::support::Handle<Result<Committed, HotError>> {
    let node = sim.node(id);
    let ballot = Ballot {
        round,
        node: NodeId(id),
    };
    sim.exec
        .start(async move { node.seal(segment, ballot, SealReason::Size).await })
}

fn block_state(sim: &Sim, id: u8, block: BlockId) -> Option<BlockState> {
    sim.node(id).index().block(block).unwrap().map(|b| b.state)
}

fn is_minority(r: &Result<Committed, HotError>) -> bool {
    matches!(
        r,
        Err(HotError::Unavailable {
            reason: Unavailable::Minority,
            ..
        })
    )
}

#[test]
fn files_startup_does_not_know_stay_and_do_not_stop_the_recovery() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "known");
    put(&mut sim, 1, &key, b"some bytes");
    let strays = ["seg/notes", "big/zzz", "big/ab/junk"];
    for path in strays {
        durable_file(&sim.disk(1), path);
    }
    sim.crash(1);
    let report = sim.restart(1);
    assert!(report.swept.is_empty(), "{report:?}");
    assert_eq!((report.sealed, report.unsealed), (1, 0));
    assert!(report.recovery.is_some());
    for path in strays {
        assert!(sim.disk(1).exists(path).unwrap(), "{path} was removed");
    }
    assert!(live(&sim, 1, &key));
}

#[test]
fn a_restart_in_a_minority_seals_nothing_and_the_tick_seals_once_a_majority_answers_and_only_once()
{
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "kept");
    put(&mut sim, 1, &key, b"acknowledged");
    let seg = open_id(&sim, 1);
    sim.crash(1);
    sim.net.cut_both(1, 2);
    sim.net.cut_both(1, 3);
    let report = sim.restart(1);
    assert_eq!((report.sealed, report.unsealed), (0, 1));
    assert!(
        report.recovery.is_none(),
        "recovery runs once all is sealed"
    );
    let node = sim.node(1);
    assert!(!node.started());
    assert!(!live(&sim, 1, &key));

    // Two ticks before the first seal can finish: the second finds the segment being sealed.
    sim.net.heal();
    let before = sim.net.sent(1, 2, Kind::Prepare);
    sim.net.hold(Some(1), 2, Kind::Prepare);
    assert_eq!(node.tick().retried, vec![seg]);
    assert_eq!(node.tick().retried, vec![seg]);
    sim.run();
    assert_eq!(
        sim.net.sent(1, 2, Kind::Prepare),
        before + 1,
        "one seal at a time"
    );
    assert!(!node.started());
    sim.net.unhold();
    sim.net.release(0);
    sim.run();
    assert!(node.started());
    assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Sealed));
    node.recover(None).unwrap();
    assert!(
        live(&sim, 1, &key),
        "the recovery indexes the sealed record"
    );
}

#[test]
fn a_second_start_on_a_running_node_keeps_its_sealed_segments_and_reseals_nothing() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"first");
    let seg = open_id(&sim, 1);
    let h = seal(&sim, 1, seg, 2);
    sim.run();
    h.take().unwrap().unwrap();
    let node = sim.node(1);
    let n = node.clone();
    let report = sim.exec.block(async move { n.start(None).await }).unwrap();
    assert_eq!((report.sealed, report.unsealed), (0, 0));
    assert!(report.swept.is_empty());
    assert!(report.recovery.is_some());
    assert!(live(&sim, 1, &key));
    assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Sealed));

    // A segment file longer than its block row says is converged, not registered: it stays
    // whole and readable.
    let path = tierstore_format::names::HotName::Segment(seg).path();
    sim.disk(1)
        .write_at(&path, sim.disk(1).len(&path).unwrap(), b"tail")
        .unwrap();
    let n = node.clone();
    let report = sim.exec.block(async move { n.start(None).await }).unwrap();
    assert_eq!(report.unsealed, 0);
    assert!(live(&sim, 1, &key));
}

#[test]
fn a_stalled_device_fails_a_start_as_a_hot_device_error_and_the_tick_ends_the_stall() {
    let sim = Sim::new(3, config(3, 3));
    let node = sim.node(1);
    let (sched, watch, clock) = (
        node.sched().clone(),
        node.sched().clone(),
        sim.clock.clone(),
    );
    let stall = sim.cfg.stall_after_ms;
    // A call outstanding for longer than `stall_after` when the watchdog looks.
    let h = sim.exec.start(async move {
        sched
            .run(DriveClass::Write, move || {
                clock.advance(stall + 1);
                watch.watchdog()
            })
            .await
    });
    sim.run();
    assert_eq!(h.take().unwrap().unwrap(), DriveState::Stalled);
    let n = node.clone();
    let err = sim
        .exec
        .block(async move { n.start(None).await })
        .unwrap_err();
    assert!(
        matches!(
            err,
            HotError::Io {
                op: "hot device",
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(node.tick().device, Some(DriveState::Up));
    let n = node.clone();
    assert!(sim.exec.block(async move { n.start(None).await }).is_ok());
}

#[test]
fn a_closed_segment_whose_seal_did_not_commit_is_sealed_again_after_replica_stall() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"on three copies");
    let seg = open_id(&sim, 1);
    sim.set_state(3, NodeState::Down);
    sim.net.cut_both(1, 2);
    sim.net.cut_both(1, 3);
    let node = sim.node(1);
    let report = node.tick();
    sim.run();
    assert_eq!(report.early, vec![seg]);
    assert!(report.retried.is_empty(), "closed this very tick");
    assert_ne!(block_state(&sim, 1, seg), Some(BlockState::Sealed));

    sim.net.heal();
    sim.clock.advance(sim.cfg.replica_stall_ms - 1);
    let report = node.tick();
    sim.run();
    assert!(report.retried.is_empty(), "{report:?}");
    sim.clock.advance(1);
    let report = node.tick();
    sim.run();
    assert!(report.early.is_empty());
    assert_eq!(report.retried, vec![seg]);
    assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Sealed));
}

#[test]
fn accepts_nobody_answers_leave_the_seal_uncommitted_and_a_later_ballot_commits_it() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"data");
    let seg = open_id(&sim, 1);
    sim.net.lose_replies(2, 1, Kind::Accept, 1);
    sim.net.lose_replies(3, 1, Kind::Accept, 1);
    let h = seal(&sim, 1, seg, 2);
    sim.run();
    assert!(is_minority(&h.take().unwrap()));
    assert_ne!(block_state(&sim, 1, seg), Some(BlockState::Sealed));
    let h = seal(&sim, 1, seg, 5);
    sim.run();
    assert!(h.take().unwrap().is_ok());
    assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Sealed));
}

#[test]
fn a_node_out_of_the_hot_tier_seals_its_segment_with_the_other_voters_alone() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"data");
    let seg = open_id(&sim, 1);
    sim.view.nodes[0].hot = Admin::Out;
    for id in 1..=3 {
        sim.node(id).set_view(sim.view.clone());
    }
    let h = seal(&sim, 1, seg, 2);
    sim.run();
    h.take().unwrap().unwrap();
    assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Sealed));
    assert_eq!(block_state(&sim, 2, seg), Some(BlockState::Sealed));
    assert_eq!(sim.net.sent(1, 2, Kind::Prepare), 1);
    assert_eq!(sim.net.sent(1, 2, Kind::Sealed), 1);
}

#[test]
fn a_node_that_cannot_assign_a_stamp_cannot_reach_a_majority_and_seals_nothing() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"data");
    let seg = open_id(&sim, 1);
    // Beyond the last wall-clock millisecond a stamp holds, no message can be signed.
    sim.clock.advance(WALL_MAX);
    let h = seal(&sim, 1, seg, 2);
    sim.run();
    assert!(is_minority(&h.take().unwrap()));
    assert_eq!(sim.net.sent_kind(Kind::Prepare), 0);
}

#[test]
fn a_commit_whose_sealed_broadcast_cannot_be_signed_still_commits_here() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"data");
    let seg = open_id(&sim, 1);
    sim.net.hold(Some(1), 3, Kind::Accept);
    let h = seal(&sim, 1, seg, 2);
    sim.run();
    assert!(!h.done(), "waiting for node 3's answer to the accept");
    sim.clock.advance(WALL_MAX);
    sim.net.unhold();
    sim.net.release(0);
    sim.run();
    h.take().unwrap().unwrap();
    assert_eq!(block_state(&sim, 1, seg), Some(BlockState::Sealed));
    assert_eq!(sim.net.sent(1, 2, Kind::Sealed), 0);
    assert_eq!(sim.net.sent(1, 3, Kind::Sealed), 0);
}

#[test]
fn a_start_converges_a_committed_seal_whose_block_row_is_still_open() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "a");
    put(&mut sim, 1, &key, b"data");
    let seg = open_id(&sim, 1);
    // Node 2's disk fails the length read that its own converge starts with, so the commit
    // reaches it and its copy stays an open block.
    sim.disk(2).inject(OpKind::Len, 0, Fault::Eio);
    let h = seal(&sim, 1, seg, 2);
    sim.run();
    h.take().unwrap().unwrap();
    assert_eq!(block_state(&sim, 2, seg), Some(BlockState::Open));
    let node = sim.node(2);
    let report = sim
        .exec
        .block(async move { node.start(None).await })
        .unwrap();
    assert_eq!((report.sealed, report.unsealed), (0, 0));
    assert_eq!(block_state(&sim, 2, seg), Some(BlockState::Sealed));
    assert!(live(&sim, 2, &key));
}
