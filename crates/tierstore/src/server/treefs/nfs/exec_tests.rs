//! The NFS front's part of the hand-over to the next image of the process
//! (`docs/design/one-service.md` section 3.4), over real sockets and the protocol, with a fake
//! mount command: the old image answers what it owes before it gives its sockets to the offer; the
//! next image, started from the offer, serves on the same socket and control socket and finds the
//! mount; an offer it refuses leaves a cold start that binds the same address and finds the mount.
//! The exec itself is not made here (a test cannot replace its own image): the offer is written
//! into the pair as the exec would leave it, and read from the other end.

use std::os::fd::AsFd as _;
use std::path::PathBuf;

use super::super::fixtures::{EPOCH, attached, endless};
use super::super::wire_client::{Client, Dec, Enc, NFS, READ};
use super::handoff_tests::{getattr, handing, lookup, me, running};
use super::tests::{Recorder, ask, closed, euid};
use super::{Leaving, Running};
use crate::server::handoff::testing::{identity, settle, within};
use crate::server::handoff::{Front, Handing, HandoffError, Image, Own, Want, read_handed};
use crate::server::treefs::config::FsSettings;

/// The offer of `handing` as the next image finds it: written into the pair, and the end it reads.
async fn handed_end(handing: Handing) -> std::os::fd::OwnedFd {
    let args = ["tierstore".into(), "fs".into()];
    let image = Image::prepare(&handing, PathBuf::from("tierstore"), args)
        .await
        .unwrap();
    drop(handing);
    image.into_handed()
}

/// What the next image of `settings` asks of an offer.
fn want(settings: &FsSettings) -> [std::net::SocketAddr; 1] {
    [settings.nfs.listen]
}

/// The programs `runner` ran.
fn programs(runner: &Recorder) -> Vec<String> {
    runner
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect()
}

/// Asserts that the old image answers every request it has read before it gives its sockets to the
/// offer (and so before the exec, which comes after): a READ that waits for a fill when the
/// hand-over is asked for is answered, with the file's bytes, on its connection, which is then shut
/// down from the server's side, not reset; until then no offer is made. The offer holds the
/// listening socket the clients used, still open, so a client that connects meanwhile is queued, not
/// refused; and the mount is not touched.
#[tokio::test]
async fn every_owed_nfs_reply_is_sent_before_the_exec() {
    let (rig, digest) = attached("nfs-exec-owed");
    let (old, mut client, root, old_runner) = running(&rig).await;
    let trees = lookup(&mut client, &root, "trees").await;
    let tree = lookup(&mut client, &trees, &digest).await;
    let bin = lookup(&mut client, &tree, "bin").await;
    let tool = lookup(&mut client, &bin, "tool").await;
    let before = rig.upstream.asked("fill").len();
    rig.upstream.hold_fills();
    let read = Enc::default().opaque(&tool).u64(0).u32(100);
    client.send(NFS, READ, me(), read).await;
    // The request is read, and its fill is with the upstream, which holds it.
    while rig.upstream.asked("fill").len() == before {
        tokio::task::yield_now().await;
    }
    let address = old.address;
    let handing = tokio::spawn(old.hand_to_image());
    settle().await;
    assert!(!handing.is_finished(), "no offer while a READ is owed");

    rig.upstream.release_fills();
    let reply = client.read().await;
    assert_eq!(Dec::new(&reply.body).u32(), 0, "the owed READ is answered");
    assert!(
        reply.body.windows(10).any(|window| window == b"tool bytes"),
        "with the file's bytes"
    );
    assert!(
        client.ended_cleanly().await,
        "and the connection is shut down, not reset"
    );
    drop(client);
    let handing = within("the offer", handing).await.unwrap().unwrap();
    assert_eq!(handing.sockets.len(), 1);
    assert_eq!(handing.sockets[0].front(), Front::Nfs);
    assert_eq!(handing.sockets[0].local_addr().unwrap(), address);
    let queued = tokio::net::TcpStream::connect(address).await;
    assert!(queued.is_ok(), "the socket stays open: {queued:?}");
    assert!(handing.lock.is_none());
    assert!(
        programs(&old_runner)
            .iter()
            .all(|program| program != "/sbin/umount"),
        "the mount is left as it is"
    );
}

/// Asserts that the next image, started from the offer, serves on the very listening socket the
/// old image served (the clients' mount reaches it, with the handles of before) and on the very
/// control socket (the file's inode is the one the old image made, and it answers), runs no mount
/// command but the look for a mount (the mount is there), and that the old image left the mount and
/// the control socket's file in place.
#[tokio::test]
async fn the_next_image_serves_the_handed_sockets_and_finds_its_mount() {
    let (rig, digest) = attached("nfs-exec-next");
    let (old, client, root, old_runner) = running(&rig).await;
    let address = old.address;
    let settings = handing(&rig, address.port());
    let control_file = std::fs::metadata(&settings.control_socket).unwrap();
    drop(client);
    let handing = old.hand_to_image().await.unwrap();
    let control = handing
        .control
        .as_ref()
        .map(|(path, fd)| (path.clone(), identity(fd.as_fd())));
    assert_eq!(
        control.as_ref().map(|(path, _)| path),
        Some(&settings.control_socket)
    );
    let end = handed_end(handing).await;

    let own = Own {
        control: Some(&settings.control_socket),
        lock: None,
    };
    let nfs = want(&settings);
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &nfs,
    };
    let handed = read_handed(end, &want, &own).await.unwrap();
    let listener = handed.control.as_ref().unwrap();
    assert_eq!(Some(identity(listener.as_fd())), control.map(|(_, id)| id));
    let runner = Recorder::mounted_at(&rig.mountpoint);
    let new = Running::start_handed(
        &settings,
        runner.clone(),
        euid(),
        Some(endless()),
        EPOCH,
        handed,
    )
    .await
    .unwrap();
    assert_eq!(new.address, address);
    assert_eq!(
        programs(&runner),
        ["/sbin/mount"],
        "the mount is found, not made"
    );
    assert!(
        programs(&old_runner)
            .iter()
            .all(|program| program != "/sbin/umount")
    );
    use std::os::unix::fs::MetadataExt as _;
    let now = std::fs::metadata(&settings.control_socket).unwrap();
    assert_eq!(
        (now.dev(), now.ino()),
        (control_file.dev(), control_file.ino())
    );
    let listed = tokio::task::spawn_blocking(move || ask(&settings.control_socket, "attachments"));
    let listed = listed.await.unwrap();
    assert_eq!(listed["ok"], true, "{listed}");

    let mut later = Client::connect(address).await;
    assert_eq!(later.mount("/").await, root);
    let trees = lookup(&mut later, &root, "trees").await;
    let tree = lookup(&mut later, &trees, &digest).await;
    assert_eq!(getattr(&mut later, &tree).await, 0);
    drop(later);
    new.leave(Leaving::Signal).await.unwrap();
}

/// Asserts that an offer the next image refuses (its file names another control socket) fails its
/// start with the reason and closes what it was handed, and that the start launchd makes next,
/// without the offer, binds the same address itself (the refused socket is closed) and finds the
/// mount the old image left: it runs no mount command but the look, and the clients' mount reaches
/// it with the handles of before.
#[tokio::test]
async fn an_offer_the_new_image_refuses_starts_it_cold_on_the_same_addresses_and_mount() {
    let (rig, digest) = attached("nfs-exec-refused");
    let (old, client, root, _) = running(&rig).await;
    let address = old.address;
    drop(client);
    let end = handed_end(old.hand_to_image().await.unwrap()).await;

    let mut moved = handing(&rig, address.port());
    moved.control_socket = rig.dir.path().join("moved-control.sock");
    let own = Own {
        control: Some(&moved.control_socket),
        lock: None,
    };
    let nfs = want(&moved);
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &nfs,
    };
    let refused = read_handed(end, &want, &own).await.err().unwrap();
    assert!(
        matches!(&refused, HandoffError::Mismatch { offered, .. }
            if offered[0].starts_with("control ")),
        "{refused}"
    );

    // The refused socket is closed (a process another test starts holds it for the moment
    // between its fork and its exec at most).
    within("the refused socket closes", closed(address)).await;
    let runner = Recorder::mounted_at(&rig.mountpoint);
    let cold = Running::start(&moved, runner.clone(), euid(), Some(endless()), EPOCH)
        .await
        .unwrap();
    assert_eq!(cold.address, address);
    assert_eq!(
        programs(&runner),
        ["/sbin/mount"],
        "the mount is found, not made"
    );
    let mut later = Client::connect(address).await;
    assert_eq!(later.mount("/").await, root);
    let trees = lookup(&mut later, &root, "trees").await;
    let tree = lookup(&mut later, &trees, &digest).await;
    assert_eq!(getattr(&mut later, &tree).await, 0);
    drop(later);
    cold.leave(Leaving::Signal).await.unwrap();
}
