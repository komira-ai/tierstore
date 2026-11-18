//! The epoch (5.4): every reply carries the server's epoch, and a request that names what a
//! previous server handed out is refused, so a restarted server never answers for an old one.
//!
//! Proves: replies, refusals included, are stamped with the server's epoch and the request's id;
//! a handshake is accepted under epoch 0 and under a previous server's epoch; after the handshake,
//! a `wait` or `ensure` under another epoch is refused `UnknownHandle` even when the new server
//! has handed out the same handle number, and an `open-pack` or `release` under another epoch is
//! refused `NotHandshaken`; a mount that re-handshakes with the new server, re-opens and re-issues
//! its `wait` is answered under the new epoch. Catches handle numbers of a previous server
//! aliasing a new server's packs.

use crate::common::{Conn, Fixture, MountProc, pack_id};
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, Rank};
use tierstore_fetch_api::wire::{Epoch, Message, PackHandle, RefusalCode};

fn refused(code: RefusalCode) -> Message {
    Message::Refused { code }
}

fn wait(pack: PackHandle) -> Message {
    Message::Wait {
        pack,
        range: ByteRange {
            offset: 0,
            len: 4096,
        },
        job: JobId(1),
    }
}

fn ensure(pack: PackHandle) -> Message {
    Message::Ensure {
        pack,
        runs: vec![CellRun { first: 0, count: 1 }],
        class: Class::Prepare,
        job: JobId(1),
        rank: Rank(0),
    }
}

async fn open(conn: &mut Conn, mount: &MountProc, i: usize) -> PackHandle {
    match conn.call(mount.open(i, 0), &[]).await.0.message {
        Message::PackOpened { pack, .. } => pack,
        other => panic!("open-pack: {other:?}"),
    }
}

#[tokio::test]
async fn every_reply_carries_the_server_epoch_and_the_request_id() {
    let fx = Fixture::new();
    let mount = MountProc::new(3, 4096);
    let mut conn = fx.connect();
    // Before the handshake the mount stamps 0; the refusal is still the server's.
    let (reply, _) = conn.call(wait(PackHandle(9)), &[]).await;
    assert_eq!(reply.epoch, crate::common::EPOCH);
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    assert_eq!(conn.epoch, crate::common::EPOCH);
    let handle = open(&mut conn, &mount, 0).await;
    for message in [wait(handle), ensure(handle)] {
        let id = conn.send(message, &[]).await;
        let (reply, _) = conn.recv().await;
        assert_eq!((reply.id, reply.epoch), (id, crate::common::EPOCH));
    }
}

#[tokio::test]
async fn a_restarted_server_refuses_what_the_previous_one_handed_out() {
    let fx = Fixture::new();
    let mount = MountProc::new(3, 4096);
    let mut old = fx.connect();
    assert_eq!(old.handshake(&mount).await, Message::HandshakeOk);
    let old_handle = open(&mut old, &mount, 0).await;

    // A new server (a restart) of the same store under a new epoch. The mount reconnects; its
    // first frame still carries the old epoch.
    let new_epoch = Epoch(0x0bad_cafe);
    let restarted = fx.restart(new_epoch);
    let mut conn = Conn::to(&restarted);
    conn.epoch = crate::common::EPOCH;
    let (reply, _) = conn.call(mount.handshake(), &mount.fds()).await;
    assert_eq!(
        (reply.message, reply.epoch),
        (Message::HandshakeOk, new_epoch)
    );

    // The new server opens pack 1 for the mount and hands out the same number the old one gave
    // pack 0.
    let new_handle = open(&mut conn, &mount, 1).await;
    assert_eq!(new_handle, old_handle);

    // A `wait` or `ensure` still stamped with the old epoch names the old server's handle: refused,
    // never answered for pack 1.
    conn.epoch = crate::common::EPOCH;
    for message in [wait(old_handle), ensure(old_handle)] {
        let (reply, _) = conn.call(message, &[]).await;
        assert_eq!(reply.message, refused(RefusalCode::UnknownHandle));
        assert_eq!(reply.epoch, new_epoch);
    }
    for message in [
        mount.open(0, 0),
        Message::Release {
            holder: mount.holder,
        },
    ] {
        let (reply, _) = conn.call(message, &[]).await;
        assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    }
    assert!(fx.engine.waits().is_empty());
    assert!(fx.engine.ensures().is_empty());

    // Under the new epoch the mount re-opens pack 0 and re-issues its wait.
    conn.epoch = new_epoch;
    let reopened = open(&mut conn, &mount, 0).await;
    assert_ne!(reopened, new_handle);
    let (reply, _) = conn.call(wait(reopened), &[]).await;
    assert_eq!(reply.message, Message::Waited(WaitOutcome::Ready));
    assert_eq!(reply.epoch, new_epoch);
    assert_eq!(fx.engine.waits().len(), 1);
    assert_eq!(fx.engine.waits()[0].0, pack_id(0));
}

#[tokio::test]
async fn a_handshake_is_accepted_under_any_epoch() {
    let fx = Fixture::new();
    for (holder, stamped) in [(1, Epoch(0)), (2, Epoch(77)), (3, crate::common::EPOCH)] {
        let mount = MountProc::new(holder, 4096);
        let mut conn = fx.connect();
        conn.epoch = stamped;
        let (reply, _) = conn.call(mount.handshake(), &mount.fds()).await;
        assert_eq!(reply.message, Message::HandshakeOk, "stamped {stamped:?}");
    }
    assert_eq!(fx.server.stats().holders_registered, 3);
}

#[tokio::test]
async fn the_random_epoch_is_never_zero() {
    let epoch = tierstore_mount_server::random_epoch().unwrap();
    assert_ne!(epoch, Epoch(0));
}
