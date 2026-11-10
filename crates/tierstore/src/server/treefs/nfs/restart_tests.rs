//! A restart of the process under a mount that goes on: what a client holds (handles) is answered
//! by the next start, from the table of tree numbers the first left; what a client resends is
//! answered as it was; a mount that is there is not made again. All of it over real loopback
//! sockets and the protocol, with a fake mount command.

use std::net::SocketAddr;

use super::super::fixtures::{EPOCH, attached, loopback, settings};
use super::super::wire_client::{Client, Cred, Dec, Enc, GETATTR, LOOKUP, NFS, READ};
use super::Running;
use super::tests::{Recorder, closed, euid};
use crate::server::treefs::testing::Rig;

fn me() -> Cred {
    Cred::Sys(euid(), 0)
}

/// The status and the rest of the reply of a call as the user of the process.
async fn call(client: &mut Client, procedure: u32, args: Enc) -> (u32, Vec<u8>) {
    let reply = client.call(NFS, procedure, me(), args).await;
    assert_eq!(reply.accepted, 0);
    (Dec::new(&reply.body).u32(), reply.body[4..].to_vec())
}

/// The handle `name` has in `dir`.
async fn lookup(client: &mut Client, dir: &[u8], name: &str) -> Vec<u8> {
    let args = Enc::default().opaque(dir).opaque(name.as_bytes());
    let (code, body) = call(client, LOOKUP, args).await;
    assert_eq!(code, 0, "lookup of {name}");
    Dec::new(&body).opaque()
}

/// Starts over a fake mount command that has not mounted anything, in the boot `epoch`: the
/// running process, the address it serves on, and a client that has the handle of the root.
async fn started(
    rig: &Rig,
    epoch: u32,
) -> (
    Running,
    SocketAddr,
    Client,
    Vec<u8>,
    std::sync::Arc<Recorder>,
) {
    let runner = Recorder::new(true);
    let running = Running::start(
        &settings(rig, loopback()),
        runner.clone(),
        euid(),
        None,
        epoch,
    )
    .await
    .unwrap();
    let address = running.address;
    let mut client = Client::connect(address).await;
    let root = client.mount("/").await;
    (running, address, client, root, runner)
}

/// The handle of `bin/tool` of the tree `digest`, from the root handle.
async fn tool_of(client: &mut Client, root: &[u8], digest: &str) -> Vec<u8> {
    let trees = lookup(client, root, "trees").await;
    let tree = lookup(client, &trees, digest).await;
    let bin = lookup(client, &tree, "bin").await;
    lookup(client, &bin, "tool").await
}

/// The attributes and the first bytes of a file, as replies carry them.
async fn attrs_and_bytes(client: &mut Client, handle: &[u8]) -> ((u32, Vec<u8>), (u32, Vec<u8>)) {
    let attrs = call(client, GETATTR, Enc::default().opaque(handle)).await;
    let read = Enc::default().opaque(handle).u64(0).u32(100);
    (attrs, call(client, READ, read).await)
}

/// Asserts that handles a client got before a restart are answered after it with the same entries:
/// the mount is made again by nothing (the clients' hard mount waits), the table of tree numbers is
/// the one the first start left, so the handle of `bin/tool` names `bin/tool`, and reads give its
/// bytes.
#[tokio::test]
async fn handles_issued_before_a_restart_are_answered_after_it_with_the_same_entries() {
    let (rig, digest) = attached("nfs-restart-handles");
    let (first, address, mut client, root, _) = started(&rig, EPOCH).await;
    let tool = tool_of(&mut client, &root, &digest).await;
    let before = attrs_and_bytes(&mut client, &tool).await;
    assert_eq!(before.0.0, 0);
    drop(client);
    first.stop().await.unwrap();
    closed(address).await;

    let (second, _, mut again, new_root, _) = started(&rig, EPOCH).await;
    assert_eq!(new_root, root, "the root's handle is the same");
    assert_eq!(attrs_and_bytes(&mut again, &tool).await, before);
    assert_eq!(tool_of(&mut again, &root, &digest).await, tool);
    drop(again);
    second.stop().await.unwrap();
}

/// Asserts that a handle of a mount of another epoch (a boot that is gone) is `NFS3ERR_STALE`
/// after a start in the new boot, and that the new boot's own handles are served.
#[tokio::test]
async fn a_handle_of_another_mount_id_is_stale() {
    let (rig, digest) = attached("nfs-restart-stale");
    let (first, address, mut client, root, _) = started(&rig, EPOCH).await;
    let tool = tool_of(&mut client, &root, &digest).await;
    drop(client);
    first.stop().await.unwrap();
    closed(address).await;

    let (second, _, mut again, new_root, _) = started(&rig, EPOCH + 1).await;
    assert_ne!(new_root, root);
    let (code, _) = call(&mut again, GETATTR, Enc::default().opaque(&tool)).await;
    assert_eq!(code, 70);
    let (code, _) = call(&mut again, GETATTR, Enc::default().opaque(&root)).await;
    assert_eq!(code, 70);
    let fresh = tool_of(&mut again, &new_root, &digest).await;
    assert_eq!(
        call(&mut again, GETATTR, Enc::default().opaque(&fresh))
            .await
            .0,
        0
    );
    drop(again);
    second.stop().await.unwrap();
}

/// Asserts that the table of tree numbers is reopened and not made again when the epoch matches,
/// and not before the first start has none: with the attachments file gone, so that nothing
/// attaches the tree again, the handle of `bin/tool` is still answered, by a tree that is read
/// from the table's digest the first time something names it. A start of another epoch has no such
/// table.
#[tokio::test]
async fn the_table_of_tree_numbers_is_reopened_not_recreated_when_it_matches() {
    let (rig, digest) = attached("nfs-restart-table");
    let (first, address, mut client, root, _) = started(&rig, EPOCH).await;
    let tool = tool_of(&mut client, &root, &digest).await;
    drop(client);
    first.stop().await.unwrap();
    closed(address).await;
    std::fs::remove_file(rig.runtime.join("attachments")).unwrap();

    let (second, address, mut again, _, _) = started(&rig, EPOCH).await;
    assert!(second._setup.reopened);
    assert!(second._setup.core.attachments().is_empty());
    assert_eq!(attrs_and_bytes(&mut again, &tool).await.1.0, 0);
    drop(again);
    second.stop().await.unwrap();
    closed(address).await;

    let (third, _, _, _, _) = started(&rig, EPOCH + 1).await;
    assert!(!third._setup.reopened);
    third.stop().await.unwrap();
}

/// Asserts that a request the client sends again after a restart (its reply was lost with the old
/// process) is answered with the same bytes as the first answer: a LOOKUP, a GETATTR and a READ
/// change nothing, so no record of requests is kept.
#[tokio::test]
async fn a_resent_lookup_or_read_after_restart_is_answered_identically() {
    let (rig, digest) = attached("nfs-restart-resent");
    let (first, address, mut client, root, _) = started(&rig, EPOCH).await;
    let trees = lookup(&mut client, &root, "trees").await;
    let tree = lookup(&mut client, &trees, &digest).await;
    let bin = lookup(&mut client, &tree, "bin").await;
    let lookup_args = || Enc::default().opaque(&bin).opaque(b"tool");
    let first_lookup = call(&mut client, LOOKUP, lookup_args()).await;
    let tool = Dec::new(&first_lookup.1).opaque();
    let first_read = call(
        &mut client,
        READ,
        Enc::default().opaque(&tool).u64(2).u32(5),
    )
    .await;
    assert_eq!(first_read.0, 0);
    drop(client);
    first.stop().await.unwrap();
    closed(address).await;

    let (second, _, mut again, _, _) = started(&rig, EPOCH).await;
    assert_eq!(call(&mut again, LOOKUP, lookup_args()).await, first_lookup);
    let resent = call(&mut again, READ, Enc::default().opaque(&tool).u64(2).u32(5)).await;
    assert_eq!(resent, first_read);
    drop(again);
    second.stop().await.unwrap();
}

/// Asserts that the attachments are restored by a restart from the file, the pins are asserted
/// again to the cache under the mount's id (the same for both starts of one boot, another for the
/// next boot), and the control socket takes the attachments of the next start.
#[tokio::test]
async fn attachments_and_pins_are_restored_on_restart() {
    let (rig, digest) = attached("nfs-restart-pins");
    let (first, address, client, _, _) = started(&rig, EPOCH).await;
    drop(client);
    first.stop().await.unwrap();
    closed(address).await;
    let asked = rig.upstream.asked("pins-live").len();

    let (second, _, client, _, _) = started(&rig, EPOCH).await;
    drop(client);
    let attached: Vec<String> = second
        ._setup
        .core
        .attachments()
        .into_iter()
        .map(|(tree, run)| format!("{tree} {run}"))
        .collect();
    assert_eq!(attached, [format!("{digest} run-1")]);
    let pins = rig.upstream.asked("pins-live");
    assert_eq!(pins.len(), asked + 1);
    let live = pins.last().unwrap();
    assert!(live.contains(&format!("nfs-{EPOCH:08x}")), "{live}");
    assert!(live.ends_with(&digest), "{live}");
    second.stop().await.unwrap();
}

/// Asserts that a start finds the mount a restart's predecessor left and makes none, and that a
/// start with nothing mounted at the mount point makes it.
#[tokio::test]
async fn a_mount_that_is_there_is_not_made_again() {
    let (rig, _) = attached("nfs-restart-mounted");
    let runner = Recorder::mounted_at(&rig.mountpoint);
    let running = Running::start(
        &settings(&rig, loopback()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let programs: Vec<String> = runner
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect();
    assert_eq!(programs, ["/sbin/mount"]);
    running.stop().await.unwrap();
    let last = runner.calls().pop().unwrap();
    assert_eq!(last[0], "/sbin/umount");

    let runner = Recorder::new(true);
    let running = Running::start(
        &settings(&rig, loopback()),
        runner.clone(),
        euid(),
        None,
        EPOCH,
    )
    .await
    .unwrap();
    let programs: Vec<String> = runner
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect();
    assert_eq!(programs, ["/sbin/mount", "/sbin/mount_nfs"]);
    running.stop().await.unwrap();
}
