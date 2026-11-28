//! The NFS front as the old image of the hand-over to the next image of the process, over real
//! sockets and the protocol, with a fake mount command: the old image ends its connections with a
//! shutdown of its side, writes no state once it has handed over, and ends without the clients
//! having closed once `fs.nfs.drain_wait` has passed; a start that cannot work is refused before it
//! binds. The next image's start from the offer is in `exec_tests.rs`.

use std::sync::Arc;

use super::super::config::NfsSettings;
use super::super::fixtures::{EPOCH, attached, endless, loopback, settings};
use super::super::wire_client::{Client, Cred, Dec, Enc, GETATTR, LOOKUP, NFS};
use super::tests::{Recorder, euid};
use super::{Leaving, Running};
use crate::server::handoff::testing::within;
use crate::server::treefs::core::AttachError;
use crate::server::treefs::testing::Rig;

pub(super) fn me() -> Cred {
    Cred::Sys(euid(), 0)
}

/// Settings of a process serving on `port`, in the rig's directory.
pub(super) fn handing(rig: &Rig, port: u16) -> crate::server::treefs::config::FsSettings {
    let nfs = NfsSettings {
        listen: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        ..loopback()
    };
    settings(rig, nfs)
}

/// The status of GETATTR of `handle` on `client`.
pub(super) async fn getattr(client: &mut Client, handle: &[u8]) -> u32 {
    let reply = client
        .call(NFS, GETATTR, me(), Enc::default().opaque(handle))
        .await;
    Dec::new(&reply.body).u32()
}

/// The handle of `name` in `dir`.
pub(super) async fn lookup(client: &mut Client, dir: &[u8], name: &str) -> Vec<u8> {
    let args = Enc::default().opaque(dir).opaque(name.as_bytes());
    let reply = client.call(NFS, LOOKUP, me(), args).await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "lookup of {name}");
    body.opaque()
}

/// A process that serves, a connection to it, and the handle of the root.
pub(super) async fn running(rig: &Rig) -> (Running, Client, Vec<u8>, Arc<Recorder>) {
    let runner = Recorder::new(true);
    let old = Running::start(
        (&handing(rig, 0), &rig.calls()),
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

/// Asserts that every connection of the old image, an idle one and one that has mounted, ends with
/// the shutdown of its write side (the client reads the end of the stream, not a reset) when it
/// hands the sockets over, and that the offer then holds the listening socket, still open.
#[tokio::test]
async fn the_write_side_of_each_connection_is_shut_down_before_the_offer() {
    let (rig, _) = attached("nfs-handoff-shutdown");
    let (old, mut idle, root, _) = running(&rig).await;
    let mut busy = Client::connect(old.address).await;
    assert_eq!(busy.mount("/").await, root);
    let address = old.address;
    let handing = tokio::spawn(old.hand_to_image());
    assert!(idle.ended_cleanly().await);
    assert!(busy.ended_cleanly().await);
    drop((idle, busy));
    let handing = within("the offer", handing).await.unwrap().unwrap();
    assert_eq!(handing.sockets.len(), 1);
    let queued = tokio::net::TcpStream::connect(address).await;
    assert!(queued.is_ok(), "the socket stays open: {queued:?}");
}

/// Asserts that once the old image has handed over, it changes neither the table of tree numbers
/// nor the attachments file, though its clients were connected to it: its core is held, an attach
/// and a detach through it are refused, and the files are as they were when it handed over.
#[tokio::test]
async fn the_old_image_writes_no_state_once_it_has_handed_over() {
    let (rig, digest) = attached("nfs-handoff-held");
    let (old, client, _, _) = running(&rig).await;
    let table = std::fs::read(rig.runtime.join("trees")).unwrap();
    let saved = std::fs::read(rig.runtime.join("attachments")).unwrap();
    let core = Arc::clone(&old._setup.core);
    assert!(!core.is_held());
    drop(client);
    let _offer = old.hand_to_image().await.unwrap();
    assert!(core.is_held());
    let why = core.attach(&digest, "late-run").unwrap_err();
    assert!(matches!(why, AttachError::Replaced), "{why}");
    assert_eq!(core.detach("run-1"), None);
    assert_eq!(std::fs::read(rig.runtime.join("trees")).unwrap(), table);
    assert_eq!(
        std::fs::read(rig.runtime.join("attachments")).unwrap(),
        saved
    );
}

/// Asserts that a start whose runtime directory cannot be used (the file of the table of tree
/// numbers is a directory) is refused with the reason, before it binds an address or mounts.
#[tokio::test]
async fn a_start_that_cannot_work_is_refused_before_it_binds_or_mounts() {
    let (rig, _) = attached("nfs-handoff-preflight");
    let (old, client, _, _) = running(&rig).await;
    drop(client);
    old.leave(Leaving::Signal).await.unwrap();
    let table = rig.runtime.join("trees");
    let _ = std::fs::remove_file(&table);
    std::fs::create_dir(&table).unwrap();
    let runner = Recorder::new(true);
    let why = Running::start(
        (&handing(&rig, 0), &rig.calls()),
        runner.clone(),
        euid(),
        Some(endless()),
        EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(why.to_string().contains("table of tree numbers"), "{why}");
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| call[0] != "/sbin/mount" && call[0] != "/sbin/mount_nfs"),
        "nothing is looked for or mounted"
    );
}

/// Asserts that a client that sees the end of the stream and never closes its side does not keep
/// the old image: once `fs.nfs.drain_wait` has passed after it handed over, its offer is made
/// while the client is still held, without an unmount. The wait is set to a millisecond; the test
/// asserts that the offer is made, not how long that took.
#[tokio::test]
async fn a_client_that_never_closes_is_let_go_when_the_drain_wait_has_passed() {
    let (rig, _) = attached("nfs-handoff-drain");
    let mut impatient = handing(&rig, 0);
    impatient.nfs.drain_wait = std::time::Duration::from_millis(1);
    let runner = Recorder::new(true);
    let old = Running::start(
        (&impatient, &rig.calls()),
        runner.clone(),
        euid(),
        Some(endless()),
        EPOCH,
    )
    .await
    .unwrap();
    let mut stubborn = Client::connect(old.address).await;
    let _root = stubborn.mount("/").await;
    let address = old.address;
    let handing = tokio::spawn(old.hand_to_image());
    assert!(stubborn.ended_cleanly().await);
    let handing = within("the offer without the client closing", handing)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handing.sockets.len(), 1);
    assert!(runner.calls().iter().all(|call| call[0] != "/sbin/umount"));
    let queued = tokio::net::TcpStream::connect(address).await;
    assert!(queued.is_ok(), "the socket stays open: {queued:?}");
    drop(stubborn);
}
