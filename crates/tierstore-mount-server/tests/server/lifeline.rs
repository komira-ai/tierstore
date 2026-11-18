//! Holders and their lifelines (5.1): only the lifeline's hangup, or a `release`, ends a holder; a
//! connection that closes ends nothing but its own requests.
//!
//! Proves (section 14: "Mount killed (lifeline hangup): counters dropped, eviction proceeds; mount
//! whose protocol connection flaps keeps counters, regions and holds and is never zeroed while its
//! lifeline is open"; "A mount grows its counter memfd past the server's mapping: the server
//! remaps before acknowledging `open-pack`, and the evictor reads the new region's counters";
//! "More than 128 holders on one pack are all served"):
//! - a hangup drops the holder in the store once, so its counters no longer count, its holds are
//!   gone and the entry can be removed, and its handles and its connection are refused after it;
//! - a connection that closes and reconnects keeps the holder, its counters (with their values),
//!   its region, its hold and its handle, with no second registration or mapping;
//! - bytes written to the lifeline and a shut-down writing side are not a hangup;
//! - `release` ends the holder as a hangup does, only for the connection's own holder and epoch, and
//!   the holder can register again;
//! - 130 holders of one pack are all served and all their counters count;
//! - a region past the server's mapping is mapped before `PackOpened` is sent, and one past what can
//!   be mapped is refused with no hold.

use std::sync::atomic::Ordering;

use crate::common::{Fixture, MountProc, pack_id, region};
use rustix::io::write;
use rustix::net::{Shutdown, shutdown};
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::entry::{Entry, EntryStore, HolderCounters};
use tierstore_fetch_api::ids::{ByteRange, HolderId, JobId};
use tierstore_fetch_api::wire::{Epoch, Message, PackHandle, RefusalCode};

fn refused(code: RefusalCode) -> Message {
    Message::Refused { code }
}

fn wait(pack: PackHandle) -> Message {
    Message::Wait {
        pack,
        range: ByteRange { offset: 0, len: 1 },
        job: JobId(1),
    }
}

fn handle(message: &Message) -> PackHandle {
    match message {
        Message::PackOpened { pack, .. } => *pack,
        other => panic!("open-pack: {other:?}"),
    }
}

#[tokio::test]
async fn a_lifeline_hangup_drops_the_holder_and_lets_eviction_proceed() {
    let fx = Fixture::new();
    let mut mount = MountProc::new(9, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let h0 = handle(&conn.call(mount.open(0, 0), &[]).await.0.message);
    conn.call(mount.open(1, 8), &[]).await;
    // A reader of the mount is inside group 0 of pack 0.
    let counters = fx.mapper.counters(mount.holder);
    assert!(counters.set(0, 3));
    let entry = fx.store.entry(0);
    assert_eq!(entry.group_readers(0), 3);
    assert_eq!(fx.store.holders_of(0), 1);

    mount.exit();
    let stats = fx.server.wait_for(|s| s.holders_hung_up == 1).await;
    assert_eq!(stats.holders_live(), 0);
    assert_eq!(*fx.store.dropped.lock().unwrap(), vec![mount.holder]);
    // Its counters no longer count and its holds are gone: the evictor may proceed.
    assert_eq!(entry.group_readers(0), 0);
    assert_eq!((fx.store.holders_of(0), fx.store.holders_of(1)), (0, 0));
    assert!(fx.store.remove(&pack_id(0)).unwrap());
    // The connection is still open, and nothing of the holder is served on it.
    let (reply, _) = conn.call(wait(h0), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::UnknownHandle));
    let (reply, _) = conn.call(mount.open(2, 0), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    let (reply, _) = conn
        .call(
            Message::Release {
                holder: mount.holder,
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    assert_eq!(fx.store.dropped.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_connection_that_flaps_keeps_counters_regions_holds_and_handles() {
    let fx = Fixture::new();
    let mount = MountProc::new(9, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (first, _) = conn.call(mount.open(2, 16), &[]).await;
    let counters = fx.mapper.counters(mount.holder);
    assert!(counters.set(16 + 2, 5));
    let entry = fx.store.entry(2);
    assert_eq!(entry.group_readers(1), 5);

    for flap in 1..=3 {
        drop(conn);
        fx.server.wait_for(|s| s.connections_closed == flap).await;
        // Never zeroed while the lifeline is open.
        assert_eq!(entry.group_readers(1), 5);
        assert_eq!(fx.store.holders_of(2), 1);
        conn = fx.connect();
        assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
        let (again, _) = conn.call(mount.open(2, 16), &[]).await;
        assert_eq!(again.message, first.message);
        let (reply, _) = conn.call(wait(handle(&again.message)), &[]).await;
        assert_eq!(reply.message, Message::Waited(WaitOutcome::Ready));
    }
    let stats = fx.server.stats();
    assert_eq!((stats.holders_registered, stats.holders_live()), (1, 1));
    assert_eq!(fx.mapper.calls.load(Ordering::SeqCst), 1);
    assert!(fx.store.dropped.lock().unwrap().is_empty());
    assert_eq!(counters.remaps(), 0);
}

#[tokio::test]
async fn bytes_on_the_lifeline_and_a_shut_writing_side_are_not_a_hangup() {
    let fx = Fixture::new();
    let mut mount = MountProc::new(9, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let mine = mount.lifeline.as_ref().unwrap();
    assert_eq!(write(mine, b"still here").unwrap(), 10);
    shutdown(mine, Shutdown::Write).unwrap();
    // The holder is served after both.
    let (reply, fds) = conn.call(mount.open(0, 0), &[]).await;
    assert!(
        matches!(reply.message, Message::PackOpened { .. }),
        "{reply:?}"
    );
    assert_eq!(fds.len(), 2);
    assert_eq!(fx.server.stats().holders_hung_up, 0);
    assert!(fx.store.dropped.lock().unwrap().is_empty());
    mount.exit();
    fx.server.wait_for(|s| s.holders_hung_up == 1).await;
}

#[tokio::test]
async fn release_ends_the_connection_holder_only() {
    let fx = Fixture::new();
    let mut mount = MountProc::new(9, 4096);
    let mut conn = fx.connect();
    let mut other = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    assert_eq!(other.handshake(&mount).await, Message::HandshakeOk);
    let h = handle(&conn.call(mount.open(0, 0), &[]).await.0.message);

    // Another holder's id, and a stale epoch, are refused and end nothing.
    let (reply, _) = conn
        .call(
            Message::Release {
                holder: HolderId(8),
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    conn.epoch = Epoch(1);
    let (reply, _) = conn
        .call(
            Message::Release {
                holder: mount.holder,
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    conn.epoch = fx.server.epoch();
    assert_eq!(fx.server.stats().holders_live(), 1);

    let (reply, _) = conn
        .call(
            Message::Release {
                holder: mount.holder,
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, Message::Released);
    let stats = fx.server.stats();
    assert_eq!((stats.holders_released, stats.holders_live()), (1, 0));
    assert_eq!(*fx.store.dropped.lock().unwrap(), vec![mount.holder]);
    assert_eq!(fx.store.holders_of(0), 0);
    // The releasing connection is no longer bound; the other connection's holder has ended.
    let (reply, _) = conn.call(mount.open(0, 0), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    let (reply, _) = other.call(wait(h), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::UnknownHandle));
    let (reply, _) = other
        .call(
            Message::Release {
                holder: mount.holder,
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    // A handshake after the release registers the holder again, on the bound connection too.
    assert_eq!(other.handshake(&mount).await, Message::HandshakeOk);
    assert_eq!(fx.server.stats().holders_registered, 2);
    let (reply, _) = other.call(mount.open(0, 0), &[]).await;
    assert!(
        matches!(reply.message, Message::PackOpened { .. }),
        "{reply:?}"
    );
    // Its hangup ends the new registration.
    mount.exit();
    fx.server.wait_for(|s| s.holders_hung_up == 1).await;
    assert_eq!(fx.store.holders_of(0), 0);
}

#[tokio::test]
async fn more_than_128_holders_of_one_pack_are_all_served() {
    let fx = Fixture::new();
    let mut mounts = Vec::new();
    for holder in 0..130 {
        let mount = MountProc::new(1000 + holder, 4096);
        let mut conn = fx.connect();
        assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
        let (reply, fds) = conn.call(mount.open(0, 0), &[]).await;
        assert!(
            matches!(reply.message, Message::PackOpened { .. }),
            "{reply:?}"
        );
        assert_eq!(fds.len(), 2);
        assert!(fx.mapper.counters(mount.holder).set(0, 1));
        mounts.push((mount, conn));
    }
    assert_eq!(fx.store.holders_of(0), 130);
    assert_eq!(fx.store.entry(0).group_readers(0), 130);
}

#[tokio::test]
async fn a_region_past_the_mapping_is_mapped_before_the_reply() {
    let fx = Fixture::new();
    // The memfd is 8 bytes when the mount handshakes; it grows before the open.
    let mount = MountProc::new(9, 8);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let counters = fx.mapper.counters(mount.holder);
    assert_eq!(counters.len(), 8);
    rustix::fs::ftruncate(&mount.counters, 8192).unwrap();
    let (reply, _) = conn.call(mount.open(2, 4096), &[]).await;
    assert_eq!(
        reply.message,
        Message::PackOpened {
            pack: PackHandle(1),
            pack_size: crate::common::PACKS[2].1,
            region: region(2, 4096),
        }
    );
    // The mapping covered the region when the reply was sent.
    assert_eq!(counters.remaps(), 1);
    assert!(counters.len() >= region(2, 4096).end().unwrap());
    // The evictor reads the new region's counters.
    assert!(counters.set(4096 + 2 * 2, 4));
    assert_eq!(fx.store.entry(2).group_readers(2), 4);
}

#[tokio::test]
async fn a_region_past_what_can_be_mapped_is_refused_with_no_hold() {
    let fx = Fixture::new();
    *fx.mapper.limit.lock().unwrap() = Some(4096);
    let mount = MountProc::new(9, 8);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (reply, fds) = conn.call(mount.open(0, 8192), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::BadRegion));
    assert!(fds.is_empty());
    assert_eq!(fx.store.holders_of(0), 0);
}

#[tokio::test]
async fn a_holder_that_ends_while_its_pack_is_attached_gets_no_handle_and_keeps_no_hold() {
    let fx = Fixture::new();
    let mut mount = MountProc::new(9, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (attached_tx, attached_rx) = std::sync::mpsc::channel();
    let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
    *fx.store.attach_pause.lock().unwrap() = Some((attached_tx, proceed_rx));
    let id = conn.send(mount.open(0, 0), &[]).await;
    // The hold is taken; before the server records the handle, the mount exits.
    tokio::task::spawn_blocking(move || attached_rx.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(fx.store.holders_of(0), 1);
    mount.exit();
    fx.server.wait_for(|s| s.holders_hung_up == 1).await;
    proceed_tx.send(()).unwrap();
    let (reply, fds) = conn.recv().await;
    assert_eq!(reply.id, id);
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    assert!(fds.is_empty());
    assert_eq!(fx.store.holders_of(0), 0);
}

#[tokio::test]
async fn a_hangup_after_a_release_ends_nothing_more() {
    let fx = Fixture::new();
    let mut mount = MountProc::new(9, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    conn.call(mount.open(0, 0), &[]).await;
    let (reply, _) = conn
        .call(
            Message::Release {
                holder: mount.holder,
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, Message::Released);
    mount.exit();
    let stats = fx.server.wait_for(|s| s.lifeline_hangups == 1).await;
    assert_eq!((stats.holders_released, stats.holders_hung_up), (1, 0));
    assert_eq!(*fx.store.dropped.lock().unwrap(), vec![mount.holder]);
}
