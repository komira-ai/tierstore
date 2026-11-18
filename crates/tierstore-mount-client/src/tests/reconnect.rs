//! Reconnect (5.4): the epoch, re-handshake with the same holder, re-open, re-issue, backoff.
//!
//! Proves: after a server restart the client handshakes again with the same holder id, counter
//! memfd and lifeline, re-opens its packs at the same regions, swaps each pack's view for the new
//! server's, and re-issues every outstanding `wait`, which the new server answers (section 14:
//! "Server restart with a stale-epoch reply in flight: dropped; the `wait` re-issued"); a reply
//! stamped with the previous epoch on the new connection is dropped and the wait stays
//! outstanding until its own reply; a connection that flaps keeps the server's holder, its hold,
//! its handle (the server registers the holder once) and the counter of a reader that holds it
//! across the flap, which the server reads and sees given back; failed connects back off from 50 ms
//! doubling to the 2 s cap, each delay within its jitter (half the step to the step), and an
//! accepted handshake starts the steps over.

use std::sync::Arc;

use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::entry::Entry;
use tierstore_fetch_api::ids::{ByteRange, JobId};
use tierstore_fetch_api::wire::{Epoch, Message, PackHandle};

use crate::Attempt;
use crate::tests::common::{
    MIB, PACKS, Recorder, Script, ServerSide, first_region, pack_id, run, spec, stats_until, until,
    wake,
};

#[test]
fn a_server_restart_reopens_the_packs_and_reissues_their_waits_under_the_new_epoch() {
    let side = ServerSide::new("reconnect-restart");
    let clock = ManualClock::new();
    let client = side.client(1, &clock);
    let pack = client.open(spec(0)).wait().unwrap();
    let region = pack.counters().region();
    let before = pack.view();
    side.engine.park(pack_id(0));
    let wait = pack.wait(
        ByteRange {
            offset: 9 * MIB,
            len: 1,
        },
        PACKS[0].1,
        JobId(3),
    );
    side.parked(0, 1);

    side.restart(2);
    wake(&clock);
    let stats = stats_until(&client, |s| s.reopens == 1 && s.waits_reissued == 1);
    assert_eq!(stats.handshakes, 2);
    assert_eq!(stats.epoch_changes, 1);
    assert_eq!(stats.disconnects, 1);
    assert_eq!(stats.packs_opened, 1);
    // The new server registered the same holder, and its `open-pack` attached the same region.
    let server = side.wait_for(|s| s.handshakes == 1);
    assert_eq!(server.holders_registered, 1);
    assert_eq!(pack.counters().region(), region);
    assert_eq!(side.entry(0).holders(), 1);
    assert!(!Arc::ptr_eq(&before, &pack.view()));

    // The old server's wait died with its connection; the re-issued one is answered.
    side.parked(0, 2);
    assert_eq!(side.engine.complete(&pack_id(0), WaitOutcome::Ready), 1);
    assert_eq!(wait.wait(), Ok(WaitOutcome::Ready));
    let waits = side.engine.waits();
    assert_eq!(waits.len(), 2);
    assert_eq!(waits[0], waits[1]);
}

#[test]
fn a_flapping_connection_keeps_the_holder_its_hold_and_its_handle() {
    let side = ServerSide::new("reconnect-flap");
    let clock = ManualClock::new();
    let client = side.client(1, &clock);
    let pack = client.open(spec(1)).wait().unwrap();
    // A reader holds the counter of group 0 across the flap.
    side.publish(1, run(3, 1));
    let Attempt::Ready(guard) = pack.begin_read(ByteRange {
        offset: 3 * MIB,
        len: 5,
    }) else {
        panic!("cell 3 is present");
    };
    assert_eq!(side.entry(1).group_readers(0), 1);
    side.engine.park(pack_id(1));
    let wait = pack.wait(ByteRange { offset: 0, len: 1 }, PACKS[1].1, JobId(1));
    side.parked(1, 1);

    side.drop_connections();
    wake(&clock);
    let stats = stats_until(&client, |s| s.reopens == 1 && s.waits_reissued == 1);
    assert_eq!(stats.epoch_changes, 0);
    let server = side.wait_for(|s| s.handshakes == 2);
    assert_eq!(server.holders_registered, 1);
    assert_eq!(server.holders_hung_up, 0);
    assert_eq!(side.entry(1).holders(), 1);
    // The server still reads the reader's counter, and sees it given back.
    assert_eq!(side.entry(1).group_readers(0), 1);
    assert!(guard.recheck());
    drop(guard);
    assert_eq!(side.entry(1).group_readers(0), 0);

    side.parked(1, 2);
    assert_eq!(side.engine.complete(&pack_id(1), WaitOutcome::Ready), 1);
    assert_eq!(wait.wait(), Ok(WaitOutcome::Ready));
}

#[test]
fn a_reply_of_the_previous_epoch_is_dropped_and_the_reissued_wait_is_answered() {
    let script = Script::new();
    let clock = ManualClock::new();
    let client = script.client(1, Arc::new(clock.clone()));
    let peer = script.accept();
    peer.handshake(7);
    let open = client.open(spec(1));
    let (frame, _fds) = peer.open(7, 1, PACKS[1].1);
    assert_eq!(frame.epoch, Epoch(7));
    let pack = open.wait().unwrap();
    let mut wait = pack.wait(ByteRange { offset: 0, len: 1 }, PACKS[1].1, JobId(1));
    let (sent, _) = peer.next();
    assert!(matches!(
        sent.message,
        Message::Wait {
            pack: PackHandle(1),
            ..
        }
    ));
    assert_eq!(sent.epoch, Epoch(7));

    // The server restarts.
    drop(peer);
    wake(&clock);
    let peer = script.accept();
    let (handshake, _) = peer.handshake(8);
    assert_eq!(handshake.epoch, Epoch(7));
    let (reopen, _fds) = peer.open(8, 5, PACKS[1].1);
    assert_eq!(reopen.epoch, Epoch(8));
    assert!(
        matches!(reopen.message, Message::OpenPack { region, .. } if region == first_region(PACKS[1].1))
    );
    let (again, _) = peer.next();
    assert_eq!(again.epoch, Epoch(8));
    let Message::Wait {
        pack: handle,
        range,
        ..
    } = again.message
    else {
        panic!("not a wait: {again:?}");
    };
    assert_eq!(handle, PackHandle(5));
    let Message::Wait { range: first, .. } = sent.message else {
        unreachable!()
    };
    assert_eq!(range, first);

    // A reply of the previous server, under its epoch, to the re-issued request id: dropped.
    peer.reply(again.id, 7, Message::Waited(WaitOutcome::Ready), &[]);
    stats_until(&client, |s| s.stale_replies == 1);
    assert_eq!(wait.try_take(), None);
    peer.reply(
        again.id,
        8,
        Message::Waited(WaitOutcome::Eio { cell: 0 }),
        &[],
    );
    assert_eq!(wait.wait(), Ok(WaitOutcome::Eio { cell: 0 }));
    assert_eq!(client.stats().epoch_changes, 1);
}

#[test]
fn failed_connects_back_off_doubling_to_the_cap_and_a_handshake_starts_over() {
    let script = Script::new();
    script.refuse.store(8, std::sync::atomic::Ordering::SeqCst);
    let timer = Arc::new(Recorder::default());
    let client = script.client(1, timer.clone());
    for _ in 0..8 {
        wake(&timer.clock);
    }
    let peer = script.accept();
    peer.handshake(1);
    let stats = stats_until(&client, |s| s.handshakes == 1);
    assert_eq!(stats.connect_failures, 8);
    assert_eq!(stats.connects, 1);

    // The server goes away: the steps start over.
    drop(peer);
    until(|| timer.clock.sleepers() == 1);
    let delays = timer.delays.lock().unwrap().clone();
    let steps = [50, 100, 200, 400, 800, 1600, 2000, 2000, 50];
    assert_eq!(delays.len(), steps.len());
    for (d, step) in delays.iter().zip(steps) {
        assert!(
            (step / 2..=step).contains(d),
            "{d} outside [{}, {step}]: {delays:?}",
            step / 2
        );
    }
    timer.clock.advance(2000);
    let peer = script.accept();
    peer.handshake(1);
    let stats = stats_until(&client, |s| s.handshakes == 2);
    assert_eq!(stats.disconnects, 1);
    assert_eq!(stats.epoch_changes, 0);
}
