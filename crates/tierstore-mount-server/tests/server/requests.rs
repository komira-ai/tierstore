//! `open-pack`, `wait` and `ensure` (5.4), each in a task of its own.
//!
//! Proves: the outcome of a `wait` is the engine's (`ready`, `eio(cell)`, `lost`); waits parked on
//! a fill do not hold up other requests, and replies leave in the order they complete; at the
//! in-flight bound the server reads no further request until one is answered; a `wait` is clamped
//! to the pack and one wholly past its end is ready without the engine; an `ensure` reaches the
//! engine with its class, job and rank and its runs clipped to the pack, and not at all when none
//! is inside; every refusal of `open-pack` has its exact code (unknown tree, a list that is not L2,
//! no such pack, another cell size, a bad or changed region, an entry that cannot be opened or has
//! no descriptors, another holder) and takes no hold; another holder's handle is refused; two
//! concurrent `open-pack`s of one pack get one handle and one hold; a listening socket serves
//! connections.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::common::{Conn, FakeList, Fixture, MountProc, PACKS, pack_id, region, tree, until};
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, Rank, TreeRef};
use tierstore_fetch_api::list::{ListVersionUnsupported, Refusal};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_fetch_api::wire::{Message, PackHandle, RefusalCode};
use tierstore_mount_server::{Config, TreeError};
use tokio::net::{UnixListener, UnixStream};

fn refused(code: RefusalCode) -> Message {
    Message::Refused { code }
}

fn wait(pack: PackHandle, offset: u64, len: u64) -> Message {
    Message::Wait {
        pack,
        range: ByteRange { offset, len },
        job: JobId(4),
    }
}

/// A connection of a handshaken mount of `holder`.
async fn mounted(fx: &Fixture, holder: u64) -> (MountProc, Conn) {
    let mount = MountProc::new(holder, 4096);
    let mut conn = fx.connect();
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    (mount, conn)
}

async fn open(conn: &mut Conn, mount: &MountProc, i: usize) -> PackHandle {
    match conn.call(mount.open(i, 0), &[]).await.0.message {
        Message::PackOpened { pack, .. } => pack,
        other => panic!("open-pack: {other:?}"),
    }
}

#[tokio::test]
async fn a_wait_ends_with_the_engine_outcome() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    let h = open(&mut conn, &mount, 1).await;
    for outcome in [
        WaitOutcome::Ready,
        WaitOutcome::Eio { cell: 3 },
        WaitOutcome::Lost,
    ] {
        fx.engine.set_outcome(pack_id(1), outcome);
        let (reply, _) = conn.call(wait(h, 0, 10), &[]).await;
        assert_eq!(reply.message, Message::Waited(outcome));
    }
    let waits = fx.engine.waits();
    assert_eq!(waits.len(), 3);
    assert!(
        waits
            .iter()
            .all(|w| *w == (pack_id(1), ByteRange { offset: 0, len: 10 }, JobId(4)))
    );
}

#[tokio::test]
async fn parked_waits_do_not_hold_up_other_requests() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    let parked = open(&mut conn, &mount, 0).await;
    let other = open(&mut conn, &mount, 1).await;
    fx.engine.park(pack_id(0));
    let first = conn.send(wait(parked, 0, 1), &[]).await;
    let second = conn.send(wait(parked, 100, 1), &[]).await;
    let ensure = conn
        .send(
            Message::Ensure {
                pack: other,
                runs: vec![CellRun { first: 0, count: 2 }],
                class: Class::Prepare,
                job: JobId(4),
                rank: Rank(1),
            },
            &[],
        )
        .await;
    let ready = conn.send(wait(other, 0, 1), &[]).await;
    // The ensure and the wait of pack 1 are answered while both waits of pack 0 are parked.
    let mut ids = vec![conn.recv().await.0.id, conn.recv().await.0.id];
    ids.sort_unstable();
    assert_eq!(ids, vec![ensure, ready]);
    until(|| fx.engine.parked(&pack_id(0)) == 2).await;
    fx.engine.complete(&pack_id(0), WaitOutcome::Ready);
    let mut ids = vec![conn.recv().await.0.id, conn.recv().await.0.id];
    ids.sort_unstable();
    assert_eq!(ids, vec![first, second]);
}

#[tokio::test]
async fn at_the_in_flight_bound_no_further_request_is_read() {
    let one = Config {
        max_in_flight: NonZeroUsize::MIN,
    };
    for (config, ensured_first) in [(Config::default(), true), (one, false)] {
        let fx = Fixture::with(crate::common::EPOCH, config);
        let (mount, mut conn) = mounted(&fx, 1).await;
        let parked = open(&mut conn, &mount, 0).await;
        let other = open(&mut conn, &mount, 1).await;
        fx.engine.park(pack_id(0));
        let waited = conn.send(wait(parked, 0, 1), &[]).await;
        until(|| fx.engine.parked(&pack_id(0)) == 1).await;
        let ensured = conn
            .send(
                Message::Ensure {
                    pack: other,
                    runs: vec![CellRun { first: 0, count: 1 }],
                    class: Class::Demand,
                    job: JobId(4),
                    rank: Rank(0),
                },
                &[],
            )
            .await;
        if ensured_first {
            // Under the default bound the ensure is read and answered while the wait is parked.
            assert_eq!(conn.recv().await.0.id, ensured);
        } else {
            // A full round trip on another connection runs the server's tasks; the ensure is
            // still unread.
            let (second, mut other) = mounted(&fx, 2).await;
            open(&mut other, &second, 2).await;
            assert!(fx.engine.ensures().is_empty());
        }
        fx.engine.complete(&pack_id(0), WaitOutcome::Ready);
        let mut order = Vec::new();
        while order.len() < 2 - usize::from(ensured_first) {
            order.push(conn.recv().await.0.id);
        }
        if !ensured_first {
            // At a bound of one the ensure is read only once the wait is answered.
            assert_eq!(order, vec![waited, ensured]);
        }
        assert_eq!(fx.engine.ensures().len(), 1);
    }
}

#[tokio::test]
async fn a_wait_is_clamped_to_the_pack() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    let h = open(&mut conn, &mount, 1).await;
    let size = PACKS[1].1;
    let (reply, _) = conn.call(wait(h, size - 10, 100), &[]).await;
    assert_eq!(reply.message, Message::Waited(WaitOutcome::Ready));
    assert_eq!(
        fx.engine.waits(),
        vec![(
            pack_id(1),
            ByteRange {
                offset: size - 10,
                len: 10
            },
            JobId(4)
        )]
    );
    // At or past the end, and an empty range: ready, the engine not asked.
    for (offset, len) in [(size, 10), (size + 5, u64::MAX), (0, 0)] {
        let (reply, _) = conn.call(wait(h, offset, len), &[]).await;
        assert_eq!(reply.message, Message::Waited(WaitOutcome::Ready));
    }
    assert_eq!(fx.engine.waits().len(), 1);
}

#[tokio::test]
async fn an_ensure_reaches_the_engine_with_its_runs_clipped_to_the_pack() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    let h = open(&mut conn, &mount, 1).await;
    let ensure = |runs| Message::Ensure {
        pack: h,
        runs,
        class: Class::Background,
        job: JobId(8),
        rank: Rank(5),
    };
    let r = |first, count| CellRun { first, count };
    // Pack 1 has four cells.
    let (reply, _) = conn
        .call(ensure(vec![r(0, 1), r(2, 9), r(4, 1), r(1, 0)]), &[])
        .await;
    assert_eq!(reply.message, Message::Ensured);
    let ensures = fx.engine.ensures();
    assert_eq!(ensures.len(), 1);
    let e = &ensures[0];
    assert_eq!(
        (&e.pack, &e.runs, e.class, e.job, e.rank),
        (
            &pack_id(1),
            &vec![r(0, 1), r(2, 2)],
            Class::Background,
            JobId(8),
            Rank(5)
        )
    );
    // Nothing inside the pack: answered, the engine not asked.
    let (reply, _) = conn.call(ensure(vec![r(4, 3)]), &[]).await;
    assert_eq!(reply.message, Message::Ensured);
    let (reply, _) = conn.call(ensure(vec![]), &[]).await;
    assert_eq!(reply.message, Message::Ensured);
    assert_eq!(fx.engine.ensures().len(), 1);
}

#[tokio::test]
async fn each_open_pack_refusal_has_its_code_and_takes_no_hold() {
    let fx = Fixture::new();
    fx.trees.set(
        TreeRef("v1".into()),
        Err(TreeError::Unsupported(ListVersionUnsupported {
            reason: Refusal::V1List,
        })),
    );
    let mut shifted = FakeList::packs();
    shifted.records[0].0.cell_shift = 21;
    fx.trees
        .set(TreeRef("shifted".into()), Ok(Arc::new(shifted)));
    let (mount, mut conn) = mounted(&fx, 1).await;
    let open_of = |tree: &str, pack: u32, region: CounterRegion| Message::OpenPack {
        tree: TreeRef(tree.into()),
        pack,
        holder: mount.holder,
        region,
    };
    let cases = [
        (open_of("nope", 0, region(0, 0)), RefusalCode::UnknownPack),
        (
            open_of("v1", 0, region(0, 0)),
            RefusalCode::ListVersionUnsupported,
        ),
        (open_of("tree-a", 3, region(0, 0)), RefusalCode::UnknownPack),
        (
            open_of("shifted", 0, region(0, 0)),
            RefusalCode::UnknownPack,
        ),
        // Pack 2 has three groups, not one.
        (open_of("tree-a", 2, region(0, 0)), RefusalCode::BadRegion),
        (open_of("tree-a", 0, region(0, 3)), RefusalCode::BadRegion),
        (
            Message::OpenPack {
                tree: tree(),
                pack: 0,
                holder: tierstore_fetch_api::ids::HolderId(2),
                region: region(0, 0),
            },
            RefusalCode::NotHandshaken,
        ),
    ];
    for (message, code) in cases {
        let (reply, fds) = conn.call(message.clone(), &[]).await;
        assert_eq!(reply.message, refused(code), "{message:?}");
        assert!(fds.is_empty());
    }
    for i in 0..3 {
        assert_eq!(fx.store.holders_of(i), 0, "pack {i}");
    }
    // A pack already open is not opened again with another region.
    open(&mut conn, &mount, 0).await;
    let (reply, _) = conn.call(mount.open(0, 64), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::BadRegion));
    assert_eq!(fx.store.holders_of(0), 1);
}

#[tokio::test]
async fn an_entry_that_cannot_be_opened_or_has_no_descriptors_is_refused() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    fx.store.fail_open.store(true, Ordering::SeqCst);
    let (reply, _) = conn.call(mount.open(0, 0), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::UnknownPack));
    fx.store.fail_open.store(false, Ordering::SeqCst);
    fx.store.no_fds.store(true, Ordering::SeqCst);
    let (reply, fds) = conn.call(mount.open(1, 0), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::UnknownPack));
    assert!(fds.is_empty());
    assert_eq!(fx.store.holders_of(1), 0);
}

#[tokio::test]
async fn another_holder_handle_is_refused() {
    let fx = Fixture::new();
    let (a, mut conn_a) = mounted(&fx, 1).await;
    let (_b, mut conn_b) = mounted(&fx, 2).await;
    let h = open(&mut conn_a, &a, 0).await;
    let (reply, _) = conn_b.call(wait(h, 0, 1), &[]).await;
    assert_eq!(reply.message, refused(RefusalCode::UnknownHandle));
    let (reply, _) = conn_b
        .call(
            Message::Ensure {
                pack: h,
                runs: vec![CellRun { first: 0, count: 1 }],
                class: Class::Demand,
                job: JobId(1),
                rank: Rank(0),
            },
            &[],
        )
        .await;
    assert_eq!(reply.message, refused(RefusalCode::UnknownHandle));
    assert!(fx.engine.waits().is_empty() && fx.engine.ensures().is_empty());
}

#[tokio::test]
async fn concurrent_opens_of_one_pack_get_one_handle_and_one_hold() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    fx.trees.gate(2);
    let first = conn.send(mount.open(0, 0), &[]).await;
    let second = conn.send(mount.open(0, 0), &[]).await;
    let (a, a_fds) = conn.recv().await;
    let (b, b_fds) = conn.recv().await;
    let mut ids = vec![a.id, b.id];
    ids.sort_unstable();
    assert_eq!(ids, vec![first, second]);
    assert_eq!(a.message, b.message);
    assert!(matches!(a.message, Message::PackOpened { .. }), "{a:?}");
    assert_eq!((a_fds.len(), b_fds.len()), (2, 2));
    assert_eq!(fx.trees.calls(), 2);
    assert_eq!(fx.store.holders_of(0), 1);
}

#[tokio::test]
async fn a_listening_socket_serves_each_connection() {
    let (dir, path) = tierstore_testkit::dir::TestDir::for_socket(
        &[
            &std::env::temp_dir(),
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        ],
        "mount-server",
        "s",
    );
    let fx = Fixture::new();
    let listener = UnixListener::bind(&path).unwrap();
    let server = fx.server.clone();
    let serving = tokio::spawn(async move { server.serve(listener).await });
    for holder in [1, 2] {
        let mount = MountProc::new(holder, 4096);
        let mut conn = Conn::client(UnixStream::connect(&path).await.unwrap());
        assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
        assert!(matches!(
            conn.call(mount.open(0, 0), &[]).await.0.message,
            Message::PackOpened { .. }
        ));
    }
    assert_eq!(fx.server.stats().connections, 2);
    serving.abort();
    drop(dir);
}

#[tokio::test]
async fn a_connection_that_closes_with_a_wait_parked_cancels_the_wait_and_keeps_the_holder() {
    let fx = Fixture::new();
    let (mount, mut conn) = mounted(&fx, 1).await;
    let h = open(&mut conn, &mount, 0).await;
    fx.engine.park(pack_id(0));
    conn.send(wait(h, 0, 1), &[]).await;
    until(|| fx.engine.parked(&pack_id(0)) == 1).await;
    drop(conn);
    let stats = fx.server.wait_for(|s| s.connections_closed == 1).await;
    // The parked wait was dropped with its connection: completing the fill reaches no one.
    assert_eq!(fx.engine.complete(&pack_id(0), WaitOutcome::Ready), 0);
    assert_eq!(stats.holders_live(), 1);
    assert_eq!(fx.store.holders_of(0), 1);
    drop(mount);
}
