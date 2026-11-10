//! The hand-over of the NFS listening socket from a running process to the next, over real
//! sockets and the protocol, with a fake mount command: the next process takes the socket and
//! serves on it; the one it replaces stops accepting, answers what it owes, ends its connections
//! with a shutdown of its side, and ends without unmounting; a take that is refused leaves the
//! running process serving.

use std::sync::Arc;

use super::super::boot::BootSource;
use super::super::config::NfsSettings;
use super::super::fixtures::{EPOCH, attached, endless, loopback, settings};
use super::super::wire_client::{Client, Cred, Dec, Enc, GETATTR, LOOKUP, NFS, READ};
use super::Running;
use super::serve_with;
use super::tests::{Recorder, closed, euid};
use crate::server::handoff::Handoff;
use crate::server::handoff::testing::within;
use crate::server::treefs::core::AttachError;
use crate::server::treefs::testing::Rig;
use crate::server::treefs::testing::SIGNALS;
use crate::server::treefs::testing::until;

fn me() -> Cred {
    Cred::Sys(euid(), 0)
}

/// Settings whose control socket for hand-offs is in the rig's directory, serving on `port`.
fn handing(rig: &Rig, port: u16) -> crate::server::treefs::config::FsSettings {
    let nfs = NfsSettings {
        listen: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        handoff: Some(Handoff {
            socket: rig.dir.path().join("handoff.sock"),
            wait: std::time::Duration::from_secs(3600),
        }),
        ..loopback()
    };
    settings(rig, nfs)
}

/// The status of GETATTR of `handle` on `client`.
async fn getattr(client: &mut Client, handle: &[u8]) -> u32 {
    let reply = client
        .call(NFS, GETATTR, me(), Enc::default().opaque(handle))
        .await;
    Dec::new(&reply.body).u32()
}

/// The handle of `name` in `dir`.
async fn lookup(client: &mut Client, dir: &[u8], name: &str) -> Vec<u8> {
    let args = Enc::default().opaque(dir).opaque(name.as_bytes());
    let reply = client.call(NFS, LOOKUP, me(), args).await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "lookup of {name}");
    body.opaque()
}

/// A process that serves with a hand-off socket, a connection to it, and the handle of the root.
async fn running(rig: &Rig) -> (Running, Client, Vec<u8>, Arc<Recorder>) {
    let runner = Recorder::new(true);
    let old = Running::start(
        &handing(rig, 0),
        runner.clone(),
        euid(),
        Some(endless()),
        EPOCH,
    )
    .await
    .unwrap();
    let mut client = Client::connect(old.address).await;
    let root = client.mount("/").await;
    (old, client, root, runner)
}

/// Starts the next process over the socket the running one serves on; its mount command finds the
/// mount there.
async fn taking_over(rig: &Rig, old: &Running) -> (Running, Arc<Recorder>) {
    let runner = Recorder::mounted_at(&rig.mountpoint);
    let new = Running::start(
        &handing(rig, old.address.port()),
        runner.clone(),
        euid(),
        Some(endless()),
        EPOCH,
    )
    .await
    .unwrap();
    (new, runner)
}

/// Asserts that the next process takes the listening socket with the offer (it serves on the very
/// port, which it did not bind), makes no mount because the mount is there, serves the clients
/// that connect from then on with the handles of before, and that the old process, once its
/// clients have closed, ends without unmounting: the mount is the new one's.
#[tokio::test]
async fn the_listening_socket_passes_with_the_offer_and_the_new_instance_accepts_on_it() {
    let (rig, digest) = attached("nfs-handoff-passes");
    let (mut old, client, root, old_runner) = running(&rig).await;
    let (new, new_runner) = taking_over(&rig, &old).await;
    assert_eq!(new.address, old.address);
    let programs: Vec<String> = new_runner
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect();
    assert_eq!(programs, ["/sbin/mount"], "the new process mounts nothing");

    drop(client);
    old.handed_over().await;
    old.stop().await.unwrap();
    assert!(
        old_runner
            .calls()
            .iter()
            .all(|call| call[0] != "/sbin/umount"),
        "the old process leaves the mount to the new one"
    );
    let mut later = Client::connect(new.address).await;
    assert_eq!(later.mount("/").await, root);
    let trees = lookup(&mut later, &root, "trees").await;
    let tree = lookup(&mut later, &trees, &digest).await;
    assert_eq!(getattr(&mut later, &tree).await, 0);
    drop(later);
    new.stop().await.unwrap();
    assert_eq!(new_runner.calls().last().unwrap()[0], "/sbin/umount");
}

/// Asserts that the old process, once it has handed the socket over, takes no connection (every
/// client that connects afterwards is served by the new process, none is shut down for being
/// accepted by the old), and answers every request it had read: a READ that waits for a fill when
/// the hand-over happens is answered, with the bytes, after the fill, on the connection it came on.
#[tokio::test]
async fn the_old_instance_stops_accepting_at_ack_and_answers_every_owed_reply_before_it_exits() {
    let (rig, digest) = attached("nfs-handoff-owed");
    let (mut old, mut client, root, _) = running(&rig).await;
    let trees = lookup(&mut client, &root, "trees").await;
    let tree = lookup(&mut client, &trees, &digest).await;
    let bin = lookup(&mut client, &tree, "bin").await;
    let tool = lookup(&mut client, &bin, "tool").await;
    let before = rig.upstream.asked("fill").len();
    rig.upstream.hold_fills();
    client
        .send(
            NFS,
            READ,
            me(),
            Enc::default().opaque(&tool).u64(0).u32(100),
        )
        .await;
    // The request is read, and its fill is with the upstream, which holds it.
    while rig.upstream.asked("fill").len() == before {
        tokio::task::yield_now().await;
    }

    let (new, _) = taking_over(&rig, &old).await;
    // The old process stops accepting when it notices the ack, a moment after the new one has
    // read `done`; connections that come before that are shut down, and a client connects again.
    while !old.handed.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    for _ in 0..16 {
        let mut fresh = Client::connect(new.address).await;
        assert_eq!(fresh.mount("/").await, root);
        assert_eq!(getattr(&mut fresh, &tool).await, 0);
    }
    rig.upstream.release_fills();
    let reply = client.read().await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "the owed READ is answered");
    assert!(
        reply.body.windows(10).any(|window| window == b"tool bytes"),
        "with the file's bytes"
    );
    assert!(
        client.ended_cleanly().await,
        "and then the connection is shut down, not reset"
    );
    drop(client);
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that an idle connection of the old process ends with the shutdown of its write side
/// (the client reads the end of the stream, not a reset) when the socket is handed over.
#[tokio::test]
async fn the_write_side_of_each_connection_is_shut_down_before_exit() {
    let (rig, _) = attached("nfs-handoff-shutdown");
    let (old, mut idle, root, _) = running(&rig).await;
    let mut busy = Client::connect(old.address).await;
    assert_eq!(busy.mount("/").await, root);
    let (new, _) = taking_over(&rig, &old).await;
    assert!(idle.ended_cleanly().await);
    assert!(busy.ended_cleanly().await);
    drop((idle, busy));
    let mut old = old;
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that a taker whose settings name another address than the running process serves on is
/// refused (it answers `nak`): its start fails with both addresses in the message, and the running
/// process keeps its socket, its control socket and its clients, and takes the next taker that
/// names its address.
#[tokio::test]
async fn a_failed_ack_leaves_the_old_instance_serving() {
    let (rig, _) = attached("nfs-handoff-nak");
    let (old, mut client, root, _) = running(&rig).await;
    let other = if old.address.port() == 1 { 2 } else { 1 };
    let why = Running::start(
        &handing(&rig, other),
        Recorder::mounted_at(&rig.mountpoint),
        euid(),
        Some(endless()),
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(
        why.to_string().contains("the running instance listens on"),
        "{why}"
    );
    assert_eq!(why.status(), 1);
    assert!(!old._setup.core.is_held(), "a refused taker holds nothing");
    assert_eq!(getattr(&mut client, &root).await, 0);
    let mut fresh = Client::connect(old.address).await;
    assert_eq!(fresh.mount("/").await, root);
    let (new, _) = taking_over(&rig, &old).await;
    assert_eq!(new.address, old.address);
    drop((client, fresh));
    let mut old = old;
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that a process with a hand-off socket that no one else holds binds the address itself
/// and offers it, that the control socket of the hand-off is gone after a stop, and that a
/// control socket path in a directory that is not there refuses the start by name.
#[tokio::test]
async fn a_cold_start_binds_its_own_socket_and_a_bad_control_path_is_refused() {
    let (rig, _) = attached("nfs-handoff-cold");
    let socket = rig.dir.path().join("handoff.sock");
    let (old, client, _, _) = running(&rig).await;
    assert!(socket.exists());
    drop(client);
    let address = old.address;
    old.stop().await.unwrap();
    // The server task is gone, and its control socket with it, once nothing listens.
    closed(address).await;
    assert!(!socket.exists());

    let mut bad = handing(&rig, 0);
    bad.nfs.handoff = Some(Handoff {
        socket: rig.dir.path().join("none/handoff.sock"),
        wait: std::time::Duration::from_secs(1),
    });
    let why = Running::start(&bad, Recorder::new(true), euid(), Some(endless()), EPOCH)
        .await
        .err()
        .unwrap();
    assert!(
        why.to_string()
            .contains("the directory of the control socket"),
        "{why}"
    );
}

/// Asserts the process end to end: `serve_with` ends with success, with no signal, when the next
/// process takes its socket, and without an unmount; the next process holds the mount, and the
/// unmount at its stop is the only one.
#[test]
fn the_process_ends_by_itself_when_its_socket_is_taken_and_leaves_the_mount() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (rig, _) = attached("nfs-handoff-serve");
    let runner = Recorder::new(true);
    let (mine, theirs) = (handing(&rig, 0), runner.clone());
    let thread =
        std::thread::spawn(move || serve_with(&mine, theirs, euid(), &BootSource::fixed()));
    until(|| runner.mounted_port().is_some());
    let port = runner.mounted_port().unwrap();
    let next = Recorder::mounted_at(&rig.mountpoint);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let new = runtime
        .block_on(Running::start(
            &handing(&rig, port),
            next.clone(),
            euid(),
            Some(endless()),
            EPOCH,
        ))
        .unwrap();
    thread.join().unwrap().unwrap();
    assert!(runner.calls().iter().all(|call| call[0] != "/sbin/umount"));
    runtime.block_on(new.stop()).unwrap();
    assert_eq!(next.calls().last().unwrap()[0], "/sbin/umount");
}

/// Asserts that a process that took the socket and then cannot make its control socket (its
/// directory is gone) says so on stderr and serves without one, and does not fail the update.
#[tokio::test]
async fn a_process_that_cannot_make_its_control_socket_after_a_takeover_serves_without_one() {
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Stderr(Arc<Mutex<Vec<u8>>>);
    impl Write for Stderr {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (rig, _) = attached("nfs-handoff-nocontrol");
    let stderr = Stderr::default();
    let log =
        crate::server::build_info::stderr_log::LineLog::new(stderr.clone(), tracing::Level::WARN);
    let offer = tracing::subscriber::with_default(log, || {
        super::offer_after_takeover(
            &rig.dir.path().join("none/handoff.sock"),
            Vec::new(),
            std::sync::Arc::new(|_| {}),
        )
    });
    assert!(offer.is_none());
    let said = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
    assert!(
        said.starts_with("tierstore fs: hand-off: binding the control socket: "),
        "{said}"
    );
    assert!(
        said.contains(
            "serves without a control socket and cannot be handed over until it restarts"
        ),
        "{said}"
    );
    assert!(
        super::offer_after_takeover(
            &rig.dir.path().join("handoff.sock"),
            Vec::new(),
            std::sync::Arc::new(|_| {})
        )
        .is_some()
    );
}

/// Asserts that once the next process has taken the socket, the old one changes neither the table
/// of tree numbers nor the attachments file, though its clients are connected to it still: an
/// attach and a detach through its core are refused, the files are as they were when the next
/// process read them, and the next process's attach is done.
#[tokio::test]
async fn the_old_instance_writes_no_state_once_the_new_one_has_the_socket() {
    let (rig, digest) = attached("nfs-handoff-held");
    let (old, client, _, _) = running(&rig).await;
    let table = std::fs::read(rig.runtime.join("trees")).unwrap();
    let saved = std::fs::read(rig.runtime.join("attachments")).unwrap();
    let (new, _) = taking_over(&rig, &old).await;
    let core = &old._setup.core;
    assert!(core.is_held());
    let why = core.attach(&digest, "late-run").unwrap_err();
    assert!(matches!(why, AttachError::Replaced), "{why}");
    assert_eq!(core.detach("run-1"), None);
    assert_eq!(std::fs::read(rig.runtime.join("trees")).unwrap(), table);
    assert_eq!(
        std::fs::read(rig.runtime.join("attachments")).unwrap(),
        saved
    );
    let _ = new._setup.core.attach(&digest, "late-run").unwrap();
    drop(client);
    let mut old = old;
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that a start whose runtime directory cannot be used (the file of the table of tree
/// numbers is a directory) is refused before it asks for the socket: the running process still
/// has its socket, its hand-off socket and its clients, holds nothing, and the next start that can
/// be used takes the socket.
#[tokio::test]
async fn a_start_that_cannot_work_is_refused_before_the_running_instance_gives_up_its_socket() {
    let (rig, _) = attached("nfs-handoff-preflight");
    let (old, mut client, root, _) = running(&rig).await;
    let table = rig.runtime.join("trees");
    let kept = std::fs::read(&table).unwrap();
    std::fs::remove_file(&table).unwrap();
    std::fs::create_dir(&table).unwrap();
    let why = Running::start(
        &handing(&rig, old.address.port()),
        Recorder::mounted_at(&rig.mountpoint),
        euid(),
        Some(endless()),
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(why.to_string().contains("table of tree numbers"), "{why}");
    assert!(!old._setup.core.is_held());
    assert!(rig.dir.path().join("handoff.sock").exists());
    assert_eq!(getattr(&mut client, &root).await, 0);
    let mut fresh = Client::connect(old.address).await;
    assert_eq!(fresh.mount("/").await, root);
    std::fs::remove_dir(&table).unwrap();
    std::fs::write(&table, kept).unwrap();
    let (new, _) = taking_over(&rig, &old).await;
    assert_eq!(new.address, old.address);
    drop((client, fresh));
    let mut old = old;
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that a client that sees the end of the stream and never closes its side does not keep
/// the old process: once `fs.nfs.drain_wait` has passed after the hand-over it ends without the
/// client having closed (`handed_over` returns while the client is still held), without an
/// unmount, and the new process serves. The wait is set to a millisecond; the test asserts that
/// the process ends, not how long that took.
#[tokio::test]
async fn a_client_that_never_closes_is_let_go_when_the_drain_wait_has_passed() {
    let (rig, _) = attached("nfs-handoff-drain");
    let mut impatient = handing(&rig, 0);
    impatient.nfs.drain_wait = std::time::Duration::from_millis(1);
    let runner = Recorder::new(true);
    let mut old = Running::start(&impatient, runner.clone(), euid(), Some(endless()), EPOCH)
        .await
        .unwrap();
    let mut stubborn = Client::connect(old.address).await;
    let root = stubborn.mount("/").await;
    let (new, _) = taking_over(&rig, &old).await;
    assert!(stubborn.ended_cleanly().await);
    within(
        "the old process ends without the client closing",
        old.handed_over(),
    )
    .await;
    old.stop().await.unwrap();
    assert!(runner.calls().iter().all(|call| call[0] != "/sbin/umount"));
    let mut later = Client::connect(new.address).await;
    assert_eq!(later.mount("/").await, root);
    drop((stubborn, later));
    new.stop().await.unwrap();
}
