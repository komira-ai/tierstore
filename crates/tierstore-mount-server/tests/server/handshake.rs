//! The handshake (5.1, 5.4): what the server checks of a mount's version and of the two
//! descriptors it sends, and how a reconnect of a live holder is told from another process that
//! names the same holder id.
//!
//! Proves: a handshake with the counter memfd and the lifeline registers the holder once and
//! binds the connection; a reconnect with the same two descriptors is accepted without a second
//! registration or mapping; a wrong version, a counter descriptor that is not a memfd or is not
//! sealed against shrinking, a lifeline that is not a socket, a mapping or registration that
//! fails, other descriptors for a live holder, and another holder on a bound connection are each
//! refused with the exact code and register nothing; requests before a handshake are refused.

use std::os::fd::AsFd;
use std::sync::atomic::Ordering;

use crate::common::{Fixture, MountProc, sealed_memfd};
use tierstore_fetch_api::ids::{ByteRange, Class, HolderId, JobId, Rank};
use tierstore_fetch_api::wire::{Message, PackHandle, RefusalCode};

fn refused(code: RefusalCode) -> Message {
    Message::Refused { code }
}

#[tokio::test]
async fn a_handshake_registers_the_holder_once_and_a_reconnect_is_the_same_holder() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut first = fx.connect();
    assert_eq!(first.handshake(&mount).await, Message::HandshakeOk);
    assert_eq!(first.epoch, fx.server.epoch());

    // A second connection of the same process (a reconnect) with the same descriptors.
    let mut second = fx.connect();
    assert_eq!(second.handshake(&mount).await, Message::HandshakeOk);
    // And the same connection again.
    assert_eq!(first.handshake(&mount).await, Message::HandshakeOk);

    let stats = fx.server.stats();
    assert_eq!(stats.handshakes, 3);
    assert_eq!(stats.holders_registered, 1);
    assert_eq!(stats.holders_live(), 1);
    assert_eq!(fx.mapper.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_version_the_server_does_not_speak_is_refused() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut conn = fx.connect();
    let (reply, fds) = conn
        .call(
            Message::Handshake {
                holder: mount.holder,
                version: 2,
            },
            &mount.fds(),
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::Version));
    assert!(fds.is_empty());
    assert_eq!(fx.server.stats().holders_registered, 0);
    assert_eq!(fx.mapper.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_counter_descriptor_that_is_not_a_sealed_memfd_is_refused() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut conn = fx.connect();
    // Not a memfd: the lifeline's own socket in the counters' place.
    let (reply, _) = conn
        .call(
            mount.handshake(),
            &[mount.lifeline_sent.as_fd(), mount.lifeline_sent.as_fd()],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    // A memfd that may still shrink.
    let unsealed = sealed_memfd(4096, 0);
    let (reply, _) = conn
        .call(
            mount.handshake(),
            &[unsealed.as_fd(), mount.lifeline_sent.as_fd()],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    assert_eq!(fx.mapper.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fx.server.stats().holders_registered, 0);
    // The same connection then handshakes properly.
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
}

#[tokio::test]
async fn a_lifeline_that_is_not_a_socket_is_refused() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let other = sealed_memfd(4096, tierstore_mount_server::COUNTER_MEMFD_SEALS);
    let mut conn = fx.connect();
    let (reply, _) = conn
        .call(mount.handshake(), &[mount.counters.as_fd(), other.as_fd()])
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    assert_eq!(fx.server.stats().holders_registered, 0);
}

#[tokio::test]
async fn a_mapping_or_a_registration_that_fails_refuses_the_handshake() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut conn = fx.connect();
    fx.mapper.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        conn.handshake(&mount).await,
        refused(RefusalCode::NotHandshaken)
    );
    fx.mapper.fail.store(false, Ordering::SeqCst);
    fx.store.fail_register.store(true, Ordering::SeqCst);
    assert_eq!(
        conn.handshake(&mount).await,
        refused(RefusalCode::NotHandshaken)
    );
    assert_eq!(fx.server.stats().holders_registered, 0);
    fx.store.fail_register.store(false, Ordering::SeqCst);
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    assert_eq!(fx.server.stats().holders_registered, 1);
}

#[tokio::test]
async fn another_process_naming_a_live_holder_is_refused() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    // Same id, a new memfd and a new lifeline: not the process that registered it.
    let impostor = MountProc::new(7, 4096);
    let mut other = fx.connect();
    assert_eq!(
        other.handshake(&impostor).await,
        refused(RefusalCode::NotHandshaken)
    );
    // Same memfd, another lifeline.
    let half = MountProc::new(7, 4096);
    let (reply, _) = other
        .call(
            mount.handshake(),
            &[mount.counters.as_fd(), half.lifeline_sent.as_fd()],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::NotHandshaken));
    let stats = fx.server.stats();
    assert_eq!((stats.handshakes, stats.holders_registered), (1, 1));
}

#[tokio::test]
async fn a_bound_connection_does_not_take_another_live_holder() {
    let fx = Fixture::new();
    let a = MountProc::new(1, 4096);
    let b = MountProc::new(2, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&a).await, Message::HandshakeOk);
    assert_eq!(
        conn.handshake(&b).await,
        refused(RefusalCode::NotHandshaken)
    );
    assert_eq!(fx.server.stats().holders_registered, 1);
    // The connection still serves `a`.
    let (reply, fds) = conn.call(a.open(0, 0), &[]).await;
    assert!(
        matches!(reply.message, Message::PackOpened { .. }),
        "{reply:?}"
    );
    assert_eq!(fds.len(), 2);
}

#[tokio::test]
async fn requests_before_a_handshake_are_refused() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut conn = fx.connect();
    let cases = [
        (mount.open(0, 0), RefusalCode::NotHandshaken),
        (
            Message::Release {
                holder: HolderId(7),
            },
            RefusalCode::NotHandshaken,
        ),
        (
            Message::Wait {
                pack: PackHandle(1),
                range: ByteRange { offset: 0, len: 1 },
                job: JobId(1),
            },
            RefusalCode::UnknownHandle,
        ),
        (
            Message::Ensure {
                pack: PackHandle(1),
                runs: vec![],
                class: Class::Demand,
                job: JobId(1),
                rank: Rank(0),
            },
            RefusalCode::UnknownHandle,
        ),
    ];
    for (message, code) in cases {
        let (reply, fds) = conn.call(message.clone(), &[]).await;
        assert_eq!(reply.message, refused(code), "{message:?}");
        assert_eq!(reply.epoch, fx.server.epoch());
        assert!(fds.is_empty());
    }
    // The counts of a reply move after it is written: wait for them.
    let stats = fx.server.wait_for(|s| s.replies == 4).await;
    assert_eq!(stats.refusals, 4);
    assert!(fx.engine.waits().is_empty());
    assert!(fx.engine.ensures().is_empty());
}

#[tokio::test]
async fn a_store_that_panics_ends_the_connection_with_its_panic() {
    let fx = Fixture::new();
    let mount = MountProc::new(7, 4096);
    let mut conn = fx.connect();
    fx.store.panic_register.store(true, Ordering::SeqCst);
    conn.send(mount.handshake(), &mount.fds()).await;
    let served = conn.served.await;
    assert!(served.unwrap_err().is_panic());
}
