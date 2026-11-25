//! The check that the mount of the `fs` interface reaches the host (`host_view.rs`), through the
//! start of a build node: a mount in a mount namespace of the process's own refuses the start
//! before anything is bound or the service manager is told it is ready, whichever way the mount
//! was got. Over the rig of `interface_tests.rs`, with files in a scratch directory for the
//! namespaces and the tables of mounts of this process and of process 1.

use std::os::fd::AsRawFd as _;
use std::sync::Arc;

use super::Service;
use super::{
    CONNECTION, FsNode, Started, assert_answers, assert_unanswered, left_state, memory, old_fs,
    platform, stop, told,
};
use crate::Backend;
use crate::server::cache_cli::{Imaging, OnSystem, serve_cache_on};
use crate::server::cache_cli_tests::probe;
use crate::server::config_cache::CacheSettings;
use crate::server::handoff::testing::within;
use crate::server::local_cache::{Disk, SystemDisk};
use crate::server::treefs::fakedev;
use crate::server::treefs::host_view::HostView;
use crate::server::treefs::launch::Platform;
use crate::server::treefs::sd::Manager;
use crate::server::treefs::testing::FakeManager;

/// The namespaces of the process that serves, and of process 1 when the host's differs.
const OWN: &str = "mnt:[4026532500]";
const HOST: &str = "mnt:[4026531841]";

/// The files of the host's side in `node`'s scratch directory: this process is in `own`, process
/// 1 is in `host` (none: its link cannot be read), and the table of process 1 shows the node's
/// mount (`shows`) or the root alone.
fn host_view(node: &FsNode, host: Option<&str>, shows: bool) -> HostView {
    let dir = node.dir.path().join("host");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(OWN, dir.join("own-ns")).unwrap();
    if let Some(host) = host {
        std::os::unix::fs::symlink(host, dir.join("host-ns")).unwrap();
    }
    let table = if shows {
        format!(
            "36 24 0:{CONNECTION} / {} rw - fuse.tierstore-fs x rw\n",
            node.mountpoint().display()
        )
    } else {
        "24 1 8:2 / / rw - ext4 /dev/sda2 rw\n".to_owned()
    };
    std::fs::write(dir.join("host-mountinfo"), table).unwrap();
    HostView {
        own_ns: dir.join("own-ns"),
        host_ns: dir.join("host-ns"),
        host_mountinfo: dir.join("host-mountinfo"),
    }
}

/// Starts a build node with `settings` on `platform`, seeing the host as `host` does.
fn start_seeing(settings: CacheSettings, platform: Platform, host: HostView) -> Started {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (told, bound) = tokio::sync::oneshot::channel();
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let upstream: Arc<dyn Backend> = memory();
    let system = OnSystem {
        service: Some(Service::on(settings.fs.clone(), platform).seeing(host)),
        nfs: None,
    };
    let ended = tokio::spawn(async move {
        let stopped = async move {
            let _ = stopped.await;
        };
        let told = move |at| drop(told.send(at));
        serve_cache_on(
            &settings,
            (upstream, disk),
            system,
            stopped,
            told,
            Imaging::default(),
        )
        .await
    });
    Started { bound, stop, ended }
}

/// The text of the refusal for the node's mount point and these namespaces.
fn refusal(node: &FsNode, host: &str) -> String {
    format!(
        "fs.mountpoint {}: the mount is not in the host's mount namespace: this process is in \
         {OWN}, process 1 is in {host}, and the table of mounts of process 1 does not show the mount; \
         a unit option such as ReadWritePaths=, ProtectSystem= or PrivateTmp=, for example in a drop-in \
         of the host's, gives the service a mount namespace of its own, from which a mount does not \
         reach the host or the job runtime; remove the option and restart the unit \
         (docs/design/one-service.md section 2.2, docs/install.md)",
        node.mountpoint().display()
    )
}

/// Asserts that a start that adopts the connection the manager kept, in the namespace of process 1,
/// serves, tells the manager `READY=1` and answers `/healthz`: the check does not hold back a node
/// whose mount is the host's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_in_the_namespace_of_process_1_is_served_and_ready() {
    let node = FsNode::new("tsfs-host-ready");
    let settings = node.settings("", "", "127.0.0.1:0");
    left_state(settings.fs.as_ref().unwrap());
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
    let host = host_view(&node, Some(OWN), false);
    let mut started = start_seeing(settings, on_platform, host);
    let addresses = within("the node serves", &mut started.bound).await.unwrap();
    assert_eq!(kept.reply().unique, 0);
    told(&manager, &["READY=1"]);
    assert_eq!(probe(addresses.s3, "/healthz").await.0, 200);
    stop(started).await.unwrap();
}

/// Asserts the incident: a start that adopts the kept connection in a mount namespace of its own
/// fails with the full text, names both namespaces and the cause, binds no address, never tells the
/// manager `READY=1`, stops reading the connection, and takes the descriptor out of the manager's
/// store so that the next start makes the mount afresh. Catches a node that serves a mount no job
/// can use and says it is ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_in_a_namespace_of_its_own_fails_the_start_and_is_never_ready() {
    let node = FsNode::new("tsfs-host-own");
    let settings = node.settings("", "", "127.0.0.1:0");
    left_state(settings.fs.as_ref().unwrap());
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
    let host = host_view(&node, Some(HOST), false);
    let started = start_seeing(settings, on_platform, host);
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), refusal(&node, HOST));
    assert!(started.bound.await.is_err(), "no address was said");
    // The connection was served (the kernel was asked to send its requests again) and then left.
    assert_eq!(kept.reply().unique, 0);
    assert_unanswered(&kept, 1);
    told(&manager, &["FDSTOREREMOVE=1\nFDNAME=fuse"]);
}

/// Asserts the case of the packaged unit, whose account cannot read the namespace of process 1: the
/// table of process 1 decides, a mount it shows is ready, and one it does not show is refused with
/// the text saying that the namespace of process 1 could not be read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_host_namespace_is_judged_by_the_table_of_process_1() {
    let node = FsNode::new("tsfs-host-table");
    let settings = node.settings("", "", "127.0.0.1:0");
    left_state(settings.fs.as_ref().unwrap());
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    // The mount in the table of this process, and in that of process 1.
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(true));
    let mut started = start_seeing(settings, on_platform, host_view(&node, None, true));
    let _addresses = within("the node serves", &mut started.bound).await.unwrap();
    assert_eq!(kept.reply().unique, 0);
    told(&manager, &["READY=1"]);
    stop(started).await.unwrap();

    let refused = FsNode::new("tsfs-host-hidden");
    let settings = refused.settings("", "", "127.0.0.1:0");
    left_state(settings.fs.as_ref().unwrap());
    let (stored, _kept) = fakedev::pair();
    let manager = FakeManager::start(&refused.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), refused.kernel(true));
    let started = start_seeing(settings, on_platform, host_view(&refused, None, false));
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        refusal(&refused, "unreadable (entity not found)")
    );
}

/// Asserts that a refusal is not held back by a manager that cannot be told to drop the
/// descriptor: the start fails with the same text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_stands_when_the_manager_cannot_drop_the_descriptor() {
    let node = FsNode::new("tsfs-host-nomanager");
    let settings = node.settings("", "", "127.0.0.1:0");
    left_state(settings.fs.as_ref().unwrap());
    let (stored, _kept) = fakedev::pair();
    let nobody = node.dir.path().join("nobody-home");
    let on_platform = Platform {
        manager: Manager::new(Some(nobody.as_os_str())),
        ..platform(
            &FakeManager::start(&node.dir.path().join("notify.sock")),
            Some(stored.as_raw_fd()),
            node.kernel(false),
        )
    };
    let started = start_seeing(settings, on_platform, host_view(&node, Some(HOST), false));
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), refusal(&node, HOST));
}

/// Asserts the hand-over: a start that takes the mount from a running instance, where the table of
/// process 1 shows it, is ready as before; where it does not, the start fails with the text, tells
/// no `READY=1`, takes nothing out of the manager's store (the descriptor is the instance's) and
/// gives the mount back, so that the instance it was taken from answers the kernel again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_taken_over_is_ready_where_the_host_sees_it_and_given_back_where_not() {
    let node = FsNode::new("tsfs-host-taken");
    let settings = node.settings("", "", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    let old = old_fs(&fs);
    let manager = FakeManager::start(&node.dir.path().join("taker.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let mut taker = start_seeing(settings, on_platform, host_view(&node, Some(HOST), true));
    let _addresses = within("the taker serves", &mut taker.bound).await.unwrap();
    within("the old instance gives the mount", old.serving)
        .await
        .unwrap()
        .unwrap();
    told(
        &manager,
        &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
    );
    stop(taker).await.unwrap();
    drop(old.running);

    let other = FsNode::new("tsfs-host-given-back");
    let settings = other.settings("", "", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    let old = old_fs(&fs);
    let manager = FakeManager::start(&other.dir.path().join("taker.sock"));
    let on_platform = platform(&manager, None, other.kernel(true));
    let started = start_seeing(settings, on_platform, host_view(&other, Some(HOST), false));
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), refusal(&other, HOST));
    assert!(started.bound.await.is_err(), "no address was said");
    assert_answers(&old.kernel, 1);
    told(&manager, &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1"]);
    drop(old.running);
}
