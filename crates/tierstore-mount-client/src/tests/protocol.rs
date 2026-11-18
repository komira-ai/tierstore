//! The protocol as the client speaks it (5.1, 5.4), against a server the test plays frame by
//! frame.
//!
//! Proves: the handshake carries the counter memfd, sealed against shrinking and not against
//! growing, and the lifeline, and every reconnect sends the same two; the lifeline stays open
//! while the client lives and hangs up when it is dropped; a refused handshake ends the client
//! (every outstanding and later request answers why, and it never connects again); each refusal of
//! `open-pack` reaches the reader, and a later open asks again with the same region; an
//! `open-pack` refused for the holder, and a `wait` refused for its handle, make the client
//! handshake again, re-open and re-issue; a `PackOpened` with another size, another region, an
//! unsealed state memfd or a header of another pack is refused; a refused re-open breaks the pack and ends its waits; a reply of another kind than its
//! request's, bytes that are not a frame, a frame with too few or too many descriptors, a stream
//! that ends inside a frame, and a write that fails each end the connection, and the client
//! connects again; a reply to no request is dropped; credentials the kernel adds to a read are
//! ignored; an `open-pack` too large for a frame (a tree reference of more than 65535 bytes) is
//! refused before it is sent; a `wait` asked while the connection is down is sent once the pack
//! is open again; an `eio` of a cell another joined read does not need re-queues that read on its
//! own cells; `ensure` is sent while the pack is open and dropped otherwise; `release` asked while
//! the connection is down is sent after the next handshake, a second one joins the first, a
//! refused one is reported, an accepted one ends the client, and one outstanding when a handshake
//! is refused answers that refusal.

use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use crate::{Attempt, BadPackFds, ClientError, OpenSpec, Pack, PackClient};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{FileType, SealFlags, fcntl_get_seals, fstat};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, Class, HolderId, JobId, Rank, TreeRef};
use tierstore_fetch_api::page::{
    CounterRegion, F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, MOUNT_SEALS,
};
use tierstore_fetch_api::wire::{Epoch, Frame, MAX_BODY, Message, PackHandle, RefusalCode};

use crate::tests::common::{
    MIB, PACKS, Peer, Script, first_region, pack_fds, pack_fds_with, pack_id, run, spec,
    stats_until, until, wake,
};

/// A client of holder 1 connected through a script, handshaken under epoch 1.
fn connected() -> (Arc<Script>, ManualClock, PackClient, Peer) {
    let script = Script::new();
    let clock = ManualClock::new();
    let client = script.client(1, Arc::new(clock.clone()));
    let peer = script.accept();
    peer.handshake(1);
    (script, clock, client, peer)
}

/// The next connection after the client backs off, handshaken under `epoch`.
fn reconnect(script: &Script, clock: &ManualClock, epoch: u64) -> Peer {
    wake(clock);
    let peer = script.accept();
    peer.handshake(epoch);
    peer
}

/// Pack `i` opened as `handle` under epoch 1.
fn opened(client: &PackClient, peer: &Peer, i: usize, handle: u64) -> Arc<Pack> {
    let ticket = client.open(spec(i));
    let _fds = peer.open(1, handle, PACKS[i].1);
    ticket.wait().unwrap()
}

fn bytes(offset: u64, len: u64) -> ByteRange {
    ByteRange { offset, len }
}

fn refused(code: RefusalCode) -> Message {
    Message::Refused { code }
}

fn inode(fd: &OwnedFd) -> (u64, u64) {
    let st = fstat(fd).unwrap();
    (st.st_dev, st.st_ino)
}

fn hung_up(fd: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::empty())];
    poll(
        &mut fds,
        Some(&Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    )
    .unwrap();
    fds[0].revents().contains(PollFlags::HUP)
}

#[test]
fn every_handshake_sends_the_same_sealed_counter_memfd_and_lifeline() {
    let script = Script::new();
    let clock = ManualClock::new();
    let client = script.client(4, Arc::new(clock.clone()));
    assert_eq!(client.holder(), HolderId(4));
    let peer = script.accept();
    let (first, fds) = peer.handshake(3);
    assert_eq!(
        first.message,
        Message::Handshake {
            holder: HolderId(4),
            version: 1
        }
    );
    assert_eq!(first.epoch, Epoch(0));
    let seals = fcntl_get_seals(&fds[0]).unwrap();
    assert!(seals.contains(SealFlags::from_bits_retain(F_SEAL_SHRINK)));
    assert!(!seals.contains(SealFlags::from_bits_retain(F_SEAL_GROW)));
    assert!(!seals.contains(SealFlags::from_bits_retain(F_SEAL_SEAL)));
    let st = fstat(&fds[1]).unwrap();
    assert_eq!(FileType::from_raw_mode(st.st_mode), FileType::Socket);

    drop(peer);
    wake(&clock);
    let peer = script.accept();
    let (again, fds2) = peer.handshake(3);
    assert_eq!(again.epoch, Epoch(3));
    assert_eq!(inode(&fds[0]), inode(&fds2[0]));
    assert_eq!(inode(&fds[1]), inode(&fds2[1]));
    let stats = stats_until(&client, |s| s.handshakes == 2);
    assert_eq!(stats.epoch_changes, 0);

    // The lifeline is open while the client lives, and hangs up when it is dropped.
    assert!(!hung_up(&fds[1]));
    drop(client);
    until(|| hung_up(&fds[1]));
    assert!(peer.closed());
}

#[test]
fn a_refused_handshake_ends_the_client() {
    let script = Script::new();
    let clock = ManualClock::new();
    let client = script.client(1, Arc::new(clock.clone()));
    let early = client.open(spec(1));
    let early_release = client.release();
    let peer = script.accept();
    let (hs, _) = peer.next();
    peer.reply(hs.id, 1, refused(RefusalCode::Version), &[]);
    let err = ClientError::HandshakeRefused(RefusalCode::Version);
    assert_eq!(early.wait().err(), Some(err.clone()));
    assert_eq!(early_release.wait(), Err(err.clone()));
    assert!(peer.closed());
    assert_eq!(client.open(spec(1)).wait().err(), Some(err.clone()));
    assert_eq!(client.open(spec(0)).wait().err(), Some(err.clone()));
    assert_eq!(client.release().wait(), Err(err));
    assert_eq!(clock.sleepers(), 0);
    assert_eq!(client.stats().handshakes, 0);
}

#[test]
fn a_refused_open_is_the_readers_miss_path_and_a_later_open_asks_again() {
    let (_script, _clock, client, peer) = connected();
    let first = client.open(spec(1));
    let (f, _) = peer.next();
    peer.reply(f.id, 1, refused(RefusalCode::ListVersionUnsupported), &[]);
    assert_eq!(
        first.wait().err(),
        Some(ClientError::OpenRefused {
            pack: pack_id(1),
            code: RefusalCode::ListVersionUnsupported
        })
    );
    let again = client.open(spec(1));
    let (f2, _fds) = peer.open(1, 3, PACKS[1].1);
    let region = |f: &Frame| match f.message {
        Message::OpenPack { region, .. } => region,
        _ => panic!("not an open-pack: {f:?}"),
    };
    assert_eq!(region(&f), region(&f2));
    assert_eq!(region(&f2), first_region(PACKS[1].1));
    assert!(again.wait().is_ok());
    let stats = stats_until(&client, |s| s.packs_opened == 1);
    assert_eq!(stats.opens_sent, 2);
}

#[test]
fn an_open_refused_for_the_holder_handshakes_again_and_opens_again() {
    let (script, clock, client, peer) = connected();
    let ticket = client.open(spec(1));
    let (f, _) = peer.next();
    peer.reply(f.id, 1, refused(RefusalCode::NotHandshaken), &[]);
    assert!(peer.closed());
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 2, PACKS[1].1);
    assert!(ticket.wait().is_ok());
    let stats = stats_until(&client, |s| s.packs_opened == 1);
    assert_eq!(stats.disconnects, 1);
    assert_eq!(stats.protocol_errors, 0);
}

#[test]
fn an_open_answered_with_another_size_region_or_state_page_is_refused() {
    let (_script, _clock, client, peer) = connected();
    let size = PACKS[1].1;
    let answer =
        |size_said: u64, region: Option<CounterRegion>, fds: crate::tests::common::PackFds| {
            let ticket = client.open(spec(1));
            let (f, _) = peer.next();
            let Message::OpenPack { region: asked, .. } = f.message else {
                panic!("not an open-pack: {f:?}");
            };
            peer.reply(
                f.id,
                1,
                Message::PackOpened {
                    pack: PackHandle(1),
                    pack_size: size_said,
                    region: region.unwrap_or(asked),
                },
                &[fds.data.as_fd(), fds.state.as_fd()],
            );
            ticket.wait().err()
        };
    assert_eq!(
        answer(size + 1, None, pack_fds(size + 1)),
        Some(ClientError::SizeMismatch {
            pack: pack_id(1),
            expected: size,
            got: size + 1
        })
    );
    let other = CounterRegion {
        offset: 8,
        groups: 1,
    };
    assert_eq!(
        answer(size, Some(other), pack_fds(size)),
        Some(ClientError::RegionMismatch {
            pack: pack_id(1),
            asked: first_region(size),
            got: other
        })
    );
    let unsealed = MOUNT_SEALS & !F_SEAL_SEAL;
    assert_eq!(
        answer(size, None, pack_fds_with(size, size, unsealed, 2)),
        Some(ClientError::BadFds {
            pack: pack_id(1),
            source: BadPackFds::Unsealed { seals: unsealed }
        })
    );
    assert_eq!(
        answer(size, None, pack_fds_with(size, size + 1, MOUNT_SEALS, 2)),
        Some(ClientError::BadFds {
            pack: pack_id(1),
            source: BadPackFds::BadHeader { pack_size: size }
        })
    );
    // Then a good answer opens it.
    let ticket = client.open(spec(1));
    let _fds = peer.open(1, 1, size);
    assert!(ticket.wait().is_ok());
}

#[test]
fn a_refused_reopen_breaks_the_pack_and_ends_its_waits() {
    let (script, clock, client, peer) = connected();
    let pack = opened(&client, &peer, 1, 1);
    let wait = pack.wait(bytes(0, 1), PACKS[1].1, JobId(1));
    let (w, _) = peer.next();
    assert!(matches!(w.message, Message::Wait { .. }));
    drop(peer);
    let peer = reconnect(&script, &clock, 2);
    let (f, _) = peer.next();
    peer.reply(f.id, 2, refused(RefusalCode::UnknownPack), &[]);
    let err = ClientError::OpenRefused {
        pack: pack_id(1),
        code: RefusalCode::UnknownPack,
    };
    assert_eq!(wait.wait(), Err(err.clone()));
    assert_eq!(pack.broken(), Some(&err));
    assert_eq!(
        pack.wait(bytes(0, 1), PACKS[1].1, JobId(1)).wait(),
        Err(err.clone())
    );
    assert!(matches!(
        pack.begin_read(bytes(0, 1)),
        Attempt::NotReady { bad: None }
    ));
    assert_eq!(client.open(spec(1)).wait().err(), Some(err));
    pack.ensure(&[run(0, 1)], Class::Demand, JobId(1), Rank(0));
    let stats = stats_until(&client, |s| s.ensures_dropped == 1);
    assert_eq!(stats.windows_outstanding, 0);
}

#[test]
fn a_refused_wait_handshakes_again_and_reissues_it() {
    let (script, clock, client, peer) = connected();
    let pack = opened(&client, &peer, 1, 1);
    let wait = pack.wait(bytes(0, 1), PACKS[1].1, JobId(1));
    let (w, _) = peer.next();
    peer.reply(w.id, 1, refused(RefusalCode::UnknownHandle), &[]);
    assert!(peer.closed());
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 4, PACKS[1].1);
    let (again, _) = peer.next();
    assert!(matches!(
        again.message,
        Message::Wait {
            pack: PackHandle(4),
            ..
        }
    ));
    peer.reply(again.id, 1, Message::Waited(WaitOutcome::Lost), &[]);
    assert_eq!(wait.wait(), Ok(WaitOutcome::Lost));
    assert_eq!(client.stats().waits_reissued, 1);
}

#[test]
fn a_reply_of_another_kind_ends_the_connection() {
    let (script, clock, client, peer) = connected();
    // `open-pack` answered with `ensured`.
    let ticket = client.open(spec(1));
    let (f, _) = peer.next();
    peer.reply(f.id, 1, Message::Ensured, &[]);
    assert!(peer.closed());
    // The handshake answered with `released`.
    wake(&clock);
    let peer = script.accept();
    let (hs, _) = peer.next();
    peer.reply(hs.id, 1, Message::Released, &[]);
    assert!(peer.closed());
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 1, PACKS[1].1);
    let pack = ticket.wait().unwrap();
    // A `wait` answered with `released`.
    let wait = pack.wait(bytes(0, 1), PACKS[1].1, JobId(1));
    let (w, _) = peer.next();
    peer.reply(w.id, 1, Message::Released, &[]);
    assert!(peer.closed());
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 1, PACKS[1].1);
    let (w, _) = peer.next();
    // An `ensure` answered with `waited`.
    pack.ensure(&[run(0, 1)], Class::Prepare, JobId(1), Rank(1));
    let (e, _) = peer.next();
    assert!(matches!(e.message, Message::Ensure { .. }));
    peer.reply(e.id, 1, Message::Waited(WaitOutcome::Ready), &[]);
    assert!(peer.closed());
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 1, PACKS[1].1);
    let (w2, _) = peer.next();
    assert_eq!(w.message, w2.message);
    // A `release` answered with `ensured`.
    let release = client.release();
    let (r, _) = peer.next();
    assert_eq!(
        r.message,
        Message::Release {
            holder: HolderId(1)
        }
    );
    peer.reply(r.id, 1, Message::Ensured, &[]);
    assert!(peer.closed());
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 1, PACKS[1].1);
    // The release is sent again after the handshake, then the re-issued wait.
    let (r, _) = peer.next();
    assert_eq!(
        r.message,
        Message::Release {
            holder: HolderId(1)
        }
    );
    peer.reply(r.id, 1, Message::Released, &[]);
    assert_eq!(release.wait(), Ok(()));
    assert_eq!(wait.wait(), Err(ClientError::Released));
    let stats = client.stats();
    assert_eq!(stats.protocol_errors, 5);
    assert_eq!(stats.disconnects, 5);
}

#[test]
fn a_reply_to_no_request_in_flight_is_dropped() {
    let (_script, _clock, client, peer) = connected();
    peer.reply(99, 1, Message::Waited(WaitOutcome::Ready), &[]);
    stats_until(&client, |s| s.unknown_replies == 1);
    // The connection serves on.
    let _pack = opened(&client, &peer, 2, 1);
    assert_eq!(client.stats().disconnects, 0);
}

#[test]
fn bytes_that_are_not_a_frame_end_the_connection() {
    let (script, clock, client, peer) = connected();
    let good = Frame {
        id: 1,
        epoch: Epoch(1),
        message: Message::Ensured,
    }
    .encode()
    .unwrap();
    // A bad magic.
    let mut bad = good.clone();
    bad[4] ^= 0xff;
    peer.send_raw(&bad, &[]);
    assert!(peer.closed());
    // A length shorter than the header.
    let peer = reconnect(&script, &clock, 1);
    peer.send_raw(&[4, 0, 0, 0], &[]);
    assert!(peer.closed());
    // A `PackOpened` without its two descriptors.
    let peer = reconnect(&script, &clock, 1);
    let opened = Frame {
        id: 1,
        epoch: Epoch(1),
        message: Message::PackOpened {
            pack: PackHandle(1),
            pack_size: 1,
            region: first_region(1),
        },
    }
    .encode()
    .unwrap();
    peer.send_raw(&opened, &[]);
    assert!(peer.closed());
    // More descriptors than a frame carries.
    let peer = reconnect(&script, &clock, 1);
    let many: Vec<_> = (0..8).map(|_| pack_fds(1)).collect();
    let borrowed: Vec<_> = many
        .iter()
        .flat_map(|f| [f.data.as_fd(), f.state.as_fd()])
        .collect();
    peer.send_raw(&good, &borrowed);
    assert!(peer.closed());
    // The stream ends inside the length, and inside the body.
    let peer = reconnect(&script, &clock, 1);
    peer.send_raw(&good[..2], &[]);
    drop(peer);
    let peer = reconnect(&script, &clock, 1);
    peer.send_raw(&good[..10], &[]);
    drop(peer);
    let stats = stats_until(&client, |s| s.disconnects == 6);
    assert_eq!(stats.protocol_errors, 6);
}

#[test]
fn a_failed_write_ends_the_connection() {
    let (script, clock, client, peer) = connected();
    peer.stream.shutdown(Shutdown::Read).unwrap();
    let ticket = client.open(spec(1));
    stats_until(&client, |s| s.disconnects == 1);
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 1, PACKS[1].1);
    assert!(ticket.wait().is_ok());
    assert_eq!(client.stats().protocol_errors, 0);
}

#[test]
fn an_eio_of_a_cell_a_joined_read_does_not_need_requeues_it_on_its_own_cells() {
    let (_script, _clock, client, peer) = connected();
    let pack = opened(&client, &peer, 0, 1);
    // A read of cell 1 in a file to the pack's end: its wait covers cells 1 to 7.
    let a = pack.wait(bytes(MIB, 1), PACKS[0].1, JobId(1));
    let (w, _) = peer.next();
    let Message::Wait { range, job, .. } = w.message else {
        panic!("not a wait: {w:?}");
    };
    assert_eq!(range, bytes(MIB, 7 * MIB));
    assert_eq!(job, JobId(1));
    let b = pack.wait(bytes(6 * MIB, 1), 7 * MIB, JobId(2));
    stats_until(&client, |s| s.wait_parts_joined == 1);
    peer.reply(w.id, 1, Message::Waited(WaitOutcome::Eio { cell: 6 }), &[]);
    assert_eq!(b.wait(), Ok(WaitOutcome::Eio { cell: 6 }));
    let (again, _) = peer.next();
    let Message::Wait { range, .. } = again.message else {
        panic!("not a wait: {again:?}");
    };
    assert_eq!(range, bytes(MIB, MIB));
    peer.reply(again.id, 1, Message::Waited(WaitOutcome::Ready), &[]);
    assert_eq!(a.wait(), Ok(WaitOutcome::Ready));
}

#[test]
fn ensure_is_sent_while_the_pack_is_open_and_dropped_otherwise() {
    let (_script, _clock, client, peer) = connected();
    let pack = opened(&client, &peer, 0, 6);
    let runs = [run(0, 2), run(9, 1)];
    pack.ensure(&runs, Class::Prepare, JobId(3), Rank(7));
    let (e, _) = peer.next();
    assert_eq!(
        e.message,
        Message::Ensure {
            pack: PackHandle(6),
            runs: runs.to_vec(),
            class: Class::Prepare,
            job: JobId(3),
            rank: Rank(7)
        }
    );
    peer.reply(e.id, 1, Message::Ensured, &[]);
    pack.ensure(&runs, Class::Background, JobId(3), Rank(8));
    let (e, _) = peer.next();
    // A refused `ensure` is a dropped hint; the connection serves on.
    peer.reply(e.id, 1, refused(RefusalCode::UnknownHandle), &[]);
    // Too many runs for one frame.
    let many = vec![run(0, 1); MAX_BODY / 8 + 1];
    pack.ensure(&many, Class::Background, JobId(3), Rank(9));
    let stats = stats_until(&client, |s| s.ensures_dropped == 1);
    assert_eq!(stats.ensures_sent, 2);
    // No connection.
    drop(peer);
    stats_until(&client, |s| s.disconnects == 1);
    pack.ensure(&runs, Class::Demand, JobId(3), Rank(0));
    let stats = stats_until(&client, |s| s.ensures_dropped == 2);
    assert_eq!(stats.ensures_sent, 2);
    assert_eq!(stats.protocol_errors, 0);
}

#[test]
fn release_asked_while_down_is_sent_after_the_handshake_and_ends_the_client() {
    let (script, clock, client, peer) = connected();
    let pack = opened(&client, &peer, 1, 1);
    let wait = pack.wait(bytes(0, 1), PACKS[1].1, JobId(1));
    let _ = peer.next();
    drop(peer);
    stats_until(&client, |s| s.disconnects == 1);
    let refused_release = client.release();
    let peer = reconnect(&script, &clock, 1);
    let (open, _) = peer.next();
    assert!(matches!(open.message, Message::OpenPack { .. }));
    let (r, _) = peer.next();
    assert_eq!(
        r.message,
        Message::Release {
            holder: HolderId(1)
        }
    );
    peer.reply(r.id, 1, refused(RefusalCode::NotHandshaken), &[]);
    assert_eq!(
        refused_release.wait(),
        Err(ClientError::ReleaseRefused(RefusalCode::NotHandshaken))
    );

    let release = client.release();
    let (r, _) = peer.next();
    // A second release while the first is in flight joins it.
    let second = client.release();
    stats_until(&client, |s| s.handshakes == 2);
    peer.reply(r.id, 1, Message::Released, &[]);
    assert_eq!(release.wait(), Ok(()));
    assert_eq!(second.wait(), Ok(()));
    assert_eq!(wait.wait(), Err(ClientError::Released));
    assert_eq!(pack.broken(), Some(&ClientError::Released));
    assert_eq!(
        client.open(spec(0)).wait().err(),
        Some(ClientError::Released)
    );
    assert_eq!(client.release().wait(), Ok(()));
    assert!(peer.closed());
    assert_eq!(clock.sleepers(), 0);
}

#[test]
fn an_open_asked_before_the_first_connection_waits_for_it() {
    let script = Script::new();
    script.refuse.store(1, std::sync::atomic::Ordering::SeqCst);
    let clock = ManualClock::new();
    let client = script.client(1, Arc::new(clock.clone()));
    let ticket = client.open(spec(2));
    stats_until(&client, |s| s.connect_failures == 1);
    let peer = reconnect(&script, &clock, 1);
    let _fds = peer.open(1, 1, PACKS[2].1);
    let pack = ticket.wait().unwrap();
    assert_eq!(pack.size(), PACKS[2].1);
}

#[test]
fn credentials_the_kernel_adds_to_a_read_are_ignored() {
    let script = Script::new();
    script
        .passcred
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = script.client(1, Arc::new(ManualClock::new()));
    let peer = script.accept();
    peer.handshake(1);
    let ticket = client.open(spec(1));
    let _fds = peer.open(1, 1, PACKS[1].1);
    assert!(ticket.wait().is_ok());
    assert_eq!(client.stats().protocol_errors, 0);
}

#[test]
fn an_open_too_large_for_a_frame_is_refused_before_it_is_sent() {
    let (_script, _clock, client, peer) = connected();
    let long = OpenSpec {
        tree: TreeRef("t".repeat(70_000)),
        ..spec(1)
    };
    let Some(ClientError::Encode { pack, reason }) = client.open(long).wait().err() else {
        panic!("an open-pack of a 70000-byte tree reference was not refused");
    };
    assert_eq!(pack, pack_id(1));
    assert!(reason.contains("65535"), "{reason}");
    // Nothing was sent: the next frame is the next open's.
    let ticket = client.open(spec(1));
    let (f, _fds) = peer.open(1, 1, PACKS[1].1);
    assert!(matches!(f.message, Message::OpenPack { ref tree, .. } if tree.0 == "tree-a"));
    assert!(ticket.wait().is_ok());
}

#[test]
fn a_wait_asked_while_the_connection_is_down_is_sent_once_the_pack_is_open_again() {
    let (script, clock, client, peer) = connected();
    let pack = opened(&client, &peer, 1, 1);
    drop(peer);
    stats_until(&client, |s| s.disconnects == 1);
    let wait = pack.wait(bytes(MIB, 1), PACKS[1].1, JobId(2));
    stats_until(&client, |s| s.windows_outstanding == 1);
    assert_eq!(client.stats().waits_sent, 0);
    let peer = reconnect(&script, &clock, 2);
    let _fds = peer.open(2, 9, PACKS[1].1);
    let (w, _) = peer.next();
    assert_eq!(
        w.message,
        Message::Wait {
            pack: PackHandle(9),
            range: bytes(MIB, 3 * MIB),
            job: JobId(2)
        }
    );
    peer.reply(w.id, 2, Message::Waited(WaitOutcome::Ready), &[]);
    assert_eq!(wait.wait(), Ok(WaitOutcome::Ready));
}
