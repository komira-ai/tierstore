//! The start that switches a node from two processes to one (`docs/design/one-service.md` section
//! 4.2): it asks the old `tierstore` for its sockets and the old `tierstore fs` for the mount,
//! checks both offers before it accepts either, accepts the mount's first, and tells either
//! `ready` only once both have said `done`. Over the rig of `interface_tests.rs`.

use std::os::fd::{AsFd as _, AsRawFd as _};
use std::path::PathBuf;

use tokio::net::{TcpListener, UnixListener};

use super::{
    FsNode, OnSystem, assert_answers, memory, old_fs, platform, start, start_on, stop, told,
};
use crate::server::cache_cli_tests::{QUICK, probe};
use crate::server::config::settings_from_text;
use crate::server::handoff::testing::within;
use crate::server::handoff::wire;
use crate::server::handoff::{Handoff, Want, offered};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::testing::FakeManager;

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
    asked.refuse().await.unwrap();
}

/// Asserts that the switch checks the old `tierstore`'s offer of its sockets before it accepts the
/// old `tierstore fs`'s offer of the mount: with an old `tierstore` that listens on other addresses
/// than the new file names, the start fails by naming them, and the old `tierstore fs` was never
/// told `ack` (it serves the mount on, answering the kernel, and still offers it on its hand-over
/// socket; the new process stored no descriptor and told the manager nothing), and the old
/// `tierstore` serves on, offering its sockets to the next taker. Catches a start that takes the
/// mount and only then finds that it cannot take the sockets, which moves the mount and leaves the
/// node half switched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_switch_checks_both_offers_before_either_ack() {
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
    let fs = new_settings.fs.clone().unwrap();
    let old = old_fs(&fs);
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
    // The old fs process holds the mount and was asked for nothing.
    assert!(!old.serving.is_finished());
    assert_answers(&old.kernel, 1);
    assert!(fs.handoff_socket.exists());
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
    old.serving.abort();
    drop(old.running);
}

/// Asserts that a switch whose offer of the mount does not match the new file (the old `tierstore
/// fs` serves another control socket) is refused to both old processes, and changes nothing: the
/// start fails naming the mismatch, the old `tierstore fs` serves the mount on and still offers it,
/// the new process stored no descriptor, and the old `tierstore` was told `nak` (not left waiting
/// for an answer: it offers its sockets to the next taker) and serves on. Catches a start that
/// leaves the old `tierstore` holding a taker that never answers, which refuses every later one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_whose_offers_do_not_both_match_naks_both_and_changes_nothing() {
    let node = FsNode::new("tsfs-switch-nak");
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
    let new_settings = node.settings(&server, "", &old_bound.s3.to_string());
    let fs = new_settings.fs.clone().unwrap();
    // The old fs process serves its control socket at another path than the new file names.
    let old = old_fs(&FsSettings {
        control_socket: node.dir.path().join("other-control.sock"),
        ..fs.clone()
    });
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let new = start_on(new_settings, memory(), on_platform);
    let error = within("the start fails", new.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("other-control.sock"), "{error}");
    assert!(!old.serving.is_finished());
    assert_answers(&old.kernel, 1);
    assert!(fs.handoff_socket.exists());
    assert!(manager.texts().is_empty(), "{:?}", manager.texts());
    assert!(!old_s3.ended.is_finished());
    let (status, _) = probe(old_bound.s3, "/healthz").await;
    assert_eq!(status, 200);
    still_offers(&control, old_bound.s3).await;
    old_s3.stop.send(()).unwrap();
    within("the old S3 process ends", old_s3.ended)
        .await
        .unwrap()
        .unwrap();
    old.serving.abort();
    drop(old.running);
}

/// An old `tierstore` that offers one S3 socket at `control` and goes away at the `ack`, as one that
/// dies there would: the new process's `done` never comes. Returns the address it offers.
async fn dies_at_ack(control: PathBuf) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let socket = UnixListener::bind(&control).unwrap();
    drop(tokio::spawn(async move {
        let (stream, _) = socket.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap();
        assert_eq!(request.lines, ["take 1"]);
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

/// Asserts what follows a switch whose new process loses the old `tierstore` after its `ack` to the
/// old `tierstore fs` (here the old `tierstore` goes away at its own `ack`): the start fails, and
/// the old `tierstore fs`, which was told `ack` but never `ready`, serves the mount again; the
/// device the new process put in its store is there. Started again (systemd's restart, with that
/// store), the new process takes the mount from the old `tierstore fs`, which ends, and binds the
/// S3 address itself, as no instance answers there any more. Stopped, it leaves the mount to the
/// store, and a start after that adopts the kept descriptor. Catches a start that, once past the
/// mount's `ack`, tells it `ready` without the sockets, or leaves a mount that no process serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_whose_new_process_dies_after_ack_is_started_again_and_takes_or_adopts() {
    let node = FsNode::new("tsfs-switch-dies");
    let control = node.dir.path().join("control.sock");
    let server = server_lines(&control);
    let addr = dies_at_ack(control.clone()).await;
    let settings = || node.settings(&server, "", &addr.to_string());
    let fs = settings().fs.unwrap();
    let old = old_fs(&fs);
    let first = FakeManager::start(&node.dir.path().join("first.sock"));
    let on_platform = platform(&first, None, node.kernel(true));
    let new = start_on(settings(), memory(), on_platform);
    let error = within("the first start fails", new.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().starts_with("hand-off: "), "{error}");
    // The old fs process was told `ack` and never `ready`: it serves the mount again.
    told(&first, &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1"]);
    assert!(!old.serving.is_finished());
    assert_answers(&old.kernel, 1);
    let kept = first.take_fds("FDSTORE=1\nFDNAME=fuse");
    assert_eq!(kept.len(), 1);
    // It makes its hand-over socket again once it reads again (systemd's restart comes 2 s later).
    crate::server::local_cache::testing::until("the old fs process offers again", || {
        fs.handoff_socket.exists()
    })
    .await;

    // Started again with the store: it takes the mount from the old fs process.
    let second = FakeManager::start(&node.dir.path().join("second.sock"));
    let on_platform = platform(&second, Some(kept[0].as_raw_fd()), node.kernel(true));
    let mut again = start_on(settings(), memory(), on_platform);
    within("the old fs process gives the mount", old.serving)
        .await
        .unwrap()
        .unwrap();
    let bound = within("the new process serves", &mut again.bound)
        .await
        .unwrap();
    assert_eq!(bound.s3, addr);
    assert_answers(&old.kernel, 2);
    told(&second, &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"]);
    stop(again).await.unwrap();
    drop(old.running);

    // Stopped, it left the mount to the store; the next start adopts it.
    let stored = second.take_fds("FDSTORE=1\nFDNAME=fuse");
    let third = FakeManager::start(&node.dir.path().join("third.sock"));
    let on_platform = platform(&third, Some(stored[0].as_raw_fd()), node.kernel(true));
    let mut adopter = start_on(settings(), memory(), on_platform);
    within("the adopter serves", &mut adopter.bound)
        .await
        .unwrap();
    assert_answers(&old.kernel, 3);
    told(&third, &["READY=1"]);
    assert!(!node.aborted());
    stop(adopter).await.unwrap();
}

/// Asks the fill socket at `path` `line` on a connection of its own, and gives the answer's line.
async fn ask_fill(path: &std::path::Path, line: &str) -> String {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let mut answer = String::new();
    BufReader::new(read).read_line(&mut answer).await.unwrap();
    answer
}

/// Asserts that a process of the one-process layout whose file names `local_cache.fill_socket`
/// binds that socket while its cache is the writer, and answers on it as a node of the old layout
/// does (a request of the pins is answered `ok`, which only the writer does); and that when an
/// old-layout `tierstore fs` takes the mount from it with `take-fs 1` (the rollback of the switch:
/// that process fills through the socket its `fs.toml` names, which is this one), it goes on
/// serving the socket, and S3, until it stops, which removes the socket. Catches a release M whose
/// one process makes no fill socket, so that the rollback's old `tierstore fs` cannot fill a file
/// until the old `tierstore` has the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_one_process_instance_binds_the_fill_socket_while_it_is_the_writer_in_m() {
    use crate::server::treefs::fills::Reach;
    use crate::server::treefs::handover::{self, Paths};
    use crate::server::treefs::run::{Prepared, Running};

    let node = FsNode::new("tsfs-one-fill");
    let fill = node.dir.path().join("fill.sock");
    let local = format!("fill_socket = {:?}", fill.display().to_string());
    let settings = node.settings_with("", "", "127.0.0.1:0", &local);
    let fs = settings.fs.clone().unwrap();
    let old = old_fs(&fs);
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let mut one = start_on(settings, memory(), on_platform);
    within("the old fs process gives the mount", old.serving)
        .await
        .unwrap()
        .unwrap();
    let bound = within("the one process serves", &mut one.bound)
        .await
        .unwrap();
    crate::server::local_cache::testing::until("the fill socket is bound", || fill.exists()).await;
    assert_eq!(ask_fill(&fill, "pins-live boot-1").await, "ok 0\n");

    // An old-layout `tierstore fs` on the old `fs.toml`, which names this fill socket, takes the
    // mount back.
    let back = FsSettings {
        fill_socket: Some(fill.clone()),
        ..fs.clone()
    };
    let paths = Paths {
        socket: &back.handoff_socket,
        mountpoint: &back.mountpoint,
        control: &back.control_socket,
        wait: back.handoff_wait,
        device: |_| Ok(()),
    };
    let taken = within(
        "the old-layout fs process takes the mount",
        handover::take(&paths, Box::new(|_| Ok(()))),
    )
    .await
    .unwrap()
    .expect("the one process answers take-fs 1");
    taken.notice.ready().await.unwrap();
    let prepared = Prepared::load(&back).unwrap();
    let running = Running::take_over(
        (&back, &Reach::Socket),
        prepared,
        taken.device,
        taken.control,
    )
    .unwrap();
    running.start_reading().unwrap();
    assert_answers(&old.kernel, 1);
    // The one process is the writer still: it serves the fill socket, and S3.
    assert_eq!(ask_fill(&fill, "pins-live boot-1").await, "ok 0\n");
    let (status, _) = probe(bound.s3, "/healthz").await;
    assert_eq!(status, 200);
    stop(one).await.unwrap();
    assert!(!fill.exists());
    running.leave();
    drop(old.running);
}

/// Asserts that a start whose mount's instance gives up first (it answers `ready` with something
/// other than `bye`, and so serves on) fails by saying so, and leaves that instance's control
/// socket file where it is: the file is the instance's, which goes on accepting clients on it.
/// Catches a start that removes the file as it lets the mount go, so that no client can attach a
/// tree on the node until the running instance is restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_mount_instance_gives_up_first_leaves_its_control_socket() {
    use crate::server::treefs::fakedev;
    use crate::server::treefs::handover::offer_lines;

    let node = FsNode::new("tsfs-one-gives-up");
    let settings = node.settings("", "", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    super::left_state(&fs);
    let listener = UnixListener::bind(&fs.handoff_socket).unwrap();
    let control = std::os::unix::net::UnixListener::bind(&fs.control_socket).unwrap();
    let (device, _kernel) = fakedev::pair();
    let lines = offer_lines(&fs.mountpoint, &fs.control_socket);
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &lines, &[device.as_fd(), control.as_fd()])
            .await
            .unwrap();
        assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ack"]);
        wire::send(&stream, &["done".to_owned()], &[])
            .await
            .unwrap();
        assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ready"]);
        wire::send(&stream, &["so long".to_owned()], &[])
            .await
            .unwrap();
        control
    });
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let started = start_on(settings, memory(), on_platform);
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "hand-over: the running instance sent something other than bye after ready"
    );
    let _control = peer.await.unwrap();
    assert!(fs.control_socket.exists());
}
