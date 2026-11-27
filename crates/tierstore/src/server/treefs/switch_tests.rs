//! The start of a node whose running instance holds no mount (`docs/design/one-service.md` section
//! 3.1): the S3 process of the file the node had, or an instance that gave its mount up. The start
//! asks that instance for its sockets and lock alone, adopts or makes the mount, and accepts the
//! sockets once it has the mount. Over the rig of `interface_tests.rs`.

use std::os::fd::{AsFd as _, AsRawFd as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UnixListener};

use super::{
    FsNode, OnSystem, assert_answers, assert_unanswered, left_state, memory, platform, reply_to,
    start, start_on, stop, told,
};
use crate::server::cache_cli_tests::{QUICK, probe};
use crate::server::config::settings_from_text;
use crate::server::handoff::testing::within;
use crate::server::handoff::wire;
use crate::server::handoff::{Ask, Handoff, MountWant, Offer, Want, offered, take_service};
use crate::server::treefs::fakedev;
use crate::server::treefs::handover::offered as mount_offered;
use crate::server::treefs::interface::Taking;
use crate::server::treefs::launch::Started;
use crate::server::treefs::run::{Prepared, Running};
use crate::server::treefs::sd::Manager;
use crate::server::treefs::start::StartError;
use crate::server::treefs::testing::{FakeManager, Rig, sample};

/// The `[server]` lines of both files: the hand-off on `control`, with a wait the tests never reach.
fn server_lines(control: &std::path::Path) -> String {
    format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"",
        control.display().to_string()
    )
}

/// Asserts that the old `tierstore` at `control` still takes part in a hand-off: a taker asking it
/// now is offered its sockets (it was told `nak`, or saw the connection end, and holds no other
/// taker), and the offer is refused, so that it serves on.
async fn still_offers(control: &std::path::Path, s3: std::net::SocketAddr) {
    let handoff = Handoff {
        socket: control.to_owned(),
        wait: std::time::Duration::from_secs(30),
    };
    let want = Want {
        s3: &[s3],
        oci: &[],
        nfs: &[],
    };
    let asked = within("the old tierstore offers again", offered(&handoff, &want))
        .await
        .unwrap()
        .expect("the old tierstore answers");
    asked.refuse().await;
}

/// Asserts that a start whose file names other addresses than the running instance listens on is
/// refused before it gets anything: the start fails by naming them, it adopted and mounted nothing
/// and told the service manager nothing, and the old `tierstore` was told `nak` (not left waiting
/// for an answer: it offers its sockets to the next taker) and serves on. Catches a start that
/// makes a mount and only then finds that it cannot take the sockets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_file_names_other_addresses_fails_before_it_gets_a_mount() {
    let node = FsNode::new("tsfs-switch-order");
    let control = node.dir.path().join("control.sock");
    let server = server_lines(&control);
    let old_settings = settings_from_text(&node.node.text(&server, ""), QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let mut old_s3 = start(old_settings, memory(), OnSystem::default());
    let old_bound = within("the old S3 process serves", &mut old_s3.bound)
        .await
        .unwrap();
    // The new file names another address than the old process serves.
    let new_settings = node.settings(&server, "", "127.0.0.2:0");
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let new = start_on(new_settings, memory(), on_platform);
    let error = within("the start fails", new.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the running instance listens on"),
        "{error}"
    );
    assert!(manager.texts().is_empty(), "{:?}", manager.texts());
    assert!(!node.aborted());
    // The old S3 process serves on.
    assert!(!old_s3.ended.is_finished());
    let (status, _) = probe(old_bound.s3, "/healthz").await;
    assert_eq!(status, 200);
    still_offers(&control, old_bound.s3).await;
    old_s3.stop.send(()).unwrap();
    within("the old S3 process ends", old_s3.ended)
        .await
        .unwrap()
        .unwrap();
}

/// An old `tierstore` that offers one S3 socket at `control` and goes away at the `ack`, as one that
/// dies there would: the new process's `done` never comes. It is asked for the mount first, which
/// it refuses as an instance that holds none does, and then for the `ready` step, which the start
/// of a node with an `fs` interface asks for with the cache lock. Returns the address it offers.
async fn dies_at_ack(control: PathBuf) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let socket = UnixListener::bind(&control).unwrap();
    drop(tokio::spawn(async move {
        let (stream, _) = socket.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap();
        assert_eq!(request.lines, ["take 1", "ready", "mount"]);
        let refused = [
            "refused".to_owned(),
            "this instance holds no mount".to_owned(),
        ];
        wire::send(&stream, &refused, &[]).await.unwrap();
        drop(stream);
        let (stream, _) = socket.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap();
        assert_eq!(request.lines, ["take 1", "ready"]);
        let lines = ["offer".to_owned(), format!("s3 {addr}")];
        wire::send(&stream, &lines, &[listener.as_fd()])
            .await
            .unwrap();
        let answer = wire::recv(&stream).await.unwrap();
        assert_eq!(answer.lines, ["ack"]);
        // Gone: the socket file stays, and no one listens on it any more.
        drop((stream, socket, listener));
    }));
    addr
}

/// Asserts what follows a start that loses the old `tierstore` after its `ack` (here the old
/// `tierstore` goes away at its own `ack`): the mount, adopted from the descriptor the service
/// manager kept, is left to the store, which keeps it (nothing reads the connection, and the manager
/// is told nothing); the start fails. Started again (systemd's restart, with that store), the new
/// process adopts the kept descriptor, answers the request that waited, and binds the S3 address
/// itself, as no instance answers there any more. Catches a start that, once past the `ack`, goes
/// on without the sockets, or leaves a mount that no process serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_loses_the_sockets_after_ack_leaves_the_mount_to_the_store() {
    let node = FsNode::new("tsfs-switch-dies");
    let control = node.dir.path().join("control.sock");
    let server = server_lines(&control);
    let addr = dies_at_ack(control.clone()).await;
    let settings = || node.settings(&server, "", &addr.to_string());
    let fs = settings().fs.unwrap();
    left_state(&fs);
    let (stored, kernel) = fakedev::pair();
    let first = FakeManager::start(&node.dir.path().join("first.sock"));
    let on_platform = platform(&first, Some(stored.as_raw_fd()), node.kernel(false));
    let new = start_on(settings(), memory(), on_platform);
    let error = within("the first start fails", new.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().starts_with("hand-off: "), "{error}");
    // The mount was adopted and then left: the kernel was asked to send its requests again, and
    // nothing answers any more.
    assert_eq!(kernel.reply().unique, 0);
    assert_unanswered(&kernel, 1);
    assert!(first.texts().is_empty(), "{:?}", first.texts());

    // Started again with the store: it adopts the descriptor and answers the request that waited.
    let second = FakeManager::start(&node.dir.path().join("second.sock"));
    let on_platform = platform(&second, Some(stored.as_raw_fd()), node.kernel(false));
    let mut again = start_on(settings(), memory(), on_platform);
    let bound = within("the new process serves", &mut again.bound)
        .await
        .unwrap();
    assert_eq!(bound.s3, addr);
    assert_eq!(reply_to(&kernel), (1, 0));
    assert_answers(&kernel, 2);
    told(&second, &["READY=1"]);
    assert!(!node.aborted());
    stop(again).await.unwrap();
}

/// The waits of the tests: only a hung test reaches them.
const WAIT: Duration = Duration::from_secs(3600);

/// Asserts that a start whose mount's instance gives up first (it goes away before it answers
/// `bye` to `ready`, and so serves on) fails by saying so, and leaves that instance's control
/// socket file where it is: the file is the instance's, which goes on accepting clients on it.
/// Catches a start that removes the file as it lets the mount go, so that no client can attach a
/// tree on the node until the running instance is restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_mount_instance_gives_up_first_leaves_its_control_socket() {
    let node = FsNode::new("tsfs-one-gives-up");
    let fs = node.settings("", "", "127.0.0.1:0").fs.unwrap();
    left_state(&fs);
    let rig = Rig::new("tsfs-one-gives-up-cache", sample());
    // The running instance of the one process: its mount offered in the one hand-over.
    let (device, _kernel) = fakedev::pair();
    let prepared = Prepared::load(&fs).unwrap();
    let running =
        Arc::new(Running::adopt((&fs, &rig.calls()), prepared, device, (None, false)).unwrap());
    let mount = mount_offered(
        &running,
        &fs,
        Manager::new(None),
        (Arc::new(|_| {}), Arc::default()),
    );
    let socket = node.dir.path().join("old-handoff.sock");
    let offer = Offer::bind(&socket, Vec::new())
        .unwrap()
        .with_ready_wait(WAIT)
        .with_mount(mount);
    let serving = tokio::spawn(offer.serve());
    let handoff = Handoff { socket, wait: WAIT };
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &[],
    };
    let ask = Ask {
        mount: Some(MountWant {
            mountpoint: &fs.mountpoint,
            control: &fs.control_socket,
            wait: WAIT,
            // The stand-in for the device is a socket.
            device: |_| Ok(()),
            prepare: Box::new(|_| Ok(())),
        }),
        lock: None,
        leaves_another_lock: false,
    };
    let mut taken = within("the take", take_service(&handoff, &want, ask))
        .await
        .unwrap()
        .expect("an instance answers");
    let (mount, ready) = (taken.mount.take().expect("the mount"), taken.ready.take());
    // The instance goes away before its `bye`.
    serving.abort();
    let prepared = Prepared::load(&fs).unwrap();
    let successor =
        Running::take_over((&fs, &rig.calls()), prepared, mount.device, mount.control).unwrap();
    let taking = Taking {
        manager: Manager::new(None),
        mount: Some((
            fs.clone(),
            Started {
                running: Arc::new(successor),
                hung: None,
            },
        )),
        ready,
    };
    let error = within("the start fails", taking.serve())
        .await
        .err()
        .expect("the start fails");
    assert!(matches!(error, StartError::Handoff(_)), "{error}");
    assert!(fs.control_socket.exists());
    drop(running);
}

/// Asserts that leaving a start that got no mount (its file has no `fs` interface) does nothing
/// and returns: there is no instance to give anything back to. Catches a start with no mount that
/// waits on, or touches, a mount it never had.
#[tokio::test]
async fn abandoning_a_start_with_no_mount_does_nothing() {
    let taking = Taking {
        manager: Manager::new(None),
        mount: None,
        ready: None,
    };
    within("leaving no mount ends", taking.abandon()).await;
}
