//! The `ready` step of the NFS hand-over, over real sockets: the running process hands nothing
//! over until its successor says it serves, and a successor that dies at any step before that
//! leaves it serving, with its control socket and its attaches.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream as StdStream;

use super::Running;
use super::handoff_tests::{WAIT, getattr, handing, running, taking_over};
use crate::server::handoff::testing::{ask, connect, send, within};
use crate::server::handoff::{Handoff, Want, take_with_ready};
use crate::server::treefs::nfs::fixtures::attached;
use crate::server::treefs::nfs::wire_client::Client;

/// Lets every task that can run, run.
async fn settle() {
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
}

/// Asserts that `old` serves: a client it has answers, a new client mounts, the core takes an
/// attach again, and the process has not ended.
async fn serves(old: &Running, client: &mut Client, root: &[u8], digest: &str, name: &str) {
    assert_eq!(getattr(client, root).await, 0, "{name}: the client it has");
    let mut fresh = Client::connect(old.address).await;
    assert_eq!(fresh.mount("/").await, root, "{name}: a new client");
    assert!(!old._setup.core.is_held(), "{name}: the hold is let go");
    old._setup.core.attach(digest, name).expect(name);
    assert!(!old.server.is_finished(), "{name}: the process goes on");
}

/// Waits until a client can connect to the control socket at `path` and be answered: the control
/// socket of the process that serves is there.
async fn control_answers(path: &std::path::Path) -> serde_json::Value {
    within("the control socket answers", async {
        loop {
            if let Ok(stream) = StdStream::connect(path) {
                let mut read = BufReader::new(stream);
                read.get_mut().write_all(b"attachments\n").unwrap();
                let mut line = String::new();
                if read.read_line(&mut line).unwrap_or(0) > 0 {
                    return serde_json::from_str(&line).unwrap();
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

/// Asserts, for a successor that is killed at each step of the conversation (after the offer and
/// before its `ack`, after the `ack`, and after `done` and before `ready`), that the running
/// process goes on serving: the clients it has, new clients, and attaches, and that a successor
/// that then starts takes over and the running process ends.
#[tokio::test]
async fn a_successor_killed_at_each_step_leaves_the_old_instance_serving() {
    let (rig, digest) = attached("nfs-ready-steps");
    let (mut old, mut client, root, _) = running(&rig).await;
    let socket = rig.dir.path().join("handoff.sock");
    for (step, name) in ["before-ack", "after-ack", "after-done"]
        .into_iter()
        .enumerate()
    {
        let taker = connect(&socket).await;
        let offer = ask(&taker, &["take 1", "ready"]).await;
        assert_eq!(offer.lines[0], "offer", "{name}");
        if step >= 1 {
            send(&taker, &["ack"]).await;
        }
        if step >= 2 {
            let done = crate::server::handoff::wire::recv(&taker).await.unwrap();
            assert_eq!(done.lines, ["done"], "{name}");
            assert!(old._setup.core.is_held(), "{name}: held until the end");
        }
        drop(taker);
        within(name, async {
            while step >= 1 && !socket.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        settle().await;
        // The socket of the hand-off is the process's again, and answers.
        within(name, async {
            while StdStream::connect(&socket).is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        serves(&old, &mut client, &root, &digest, name).await;
    }
    let (new, _) = taking_over(&rig, &old).await;
    drop(client);
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that a successor whose start fails after it has taken the socket and the running
/// process has said `done` (its control socket cannot be bound) leaves the running process
/// serving: the clients, the hold let go, the attach through its control socket, and the
/// hand-over socket answering, so that a later successor takes over.
#[tokio::test]
async fn a_successor_whose_start_fails_after_done_leaves_the_old_instance_serving() {
    let (rig, digest) = attached("nfs-ready-failed-start");
    let (mut old, mut client, root, _) = running(&rig).await;
    let mut settings = handing(&rig, old.address.port());
    let blocked = rig.dir.path().join("a-directory");
    std::fs::create_dir(&blocked).unwrap();
    settings.control_socket = blocked;
    let why = Running::start(
        &settings,
        crate::server::treefs::nfs::run::tests::Recorder::mounted_at(&rig.mountpoint),
        crate::server::treefs::nfs::run::tests::euid(),
        Some(crate::server::treefs::nfs::fixtures::endless()),
        crate::server::treefs::nfs::fixtures::EPOCH,
    )
    .await
    .err()
    .unwrap();
    assert!(why.to_string().contains("fs.control_socket"), "{why}");
    let control = rig.settings().control_socket;
    assert_eq!(control_answers(&control).await["ok"], true);
    serves(&old, &mut client, &root, &digest, "later-run").await;
    let (new, _) = taking_over(&rig, &old).await;
    drop(client);
    old.handed_over().await;
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

/// Asserts that a successor that has replaced the running process's control socket (as its start
/// does) and is then gone before `ready` leaves the running process with a control socket that
/// answers, bound again at the same path, and an attach through it is done; and that a successor
/// that dies before it replaced the control socket leaves the file as it was.
#[tokio::test]
async fn the_control_socket_is_bound_again_when_a_successor_that_replaced_it_is_gone() {
    let (rig, digest) = attached("nfs-ready-control");
    let (old, client, _, _) = running(&rig).await;
    let control = rig.settings().control_socket;
    let handoff = Handoff {
        socket: rig.dir.path().join("handoff.sock"),
        wait: WAIT,
    };
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &[old.address],
    };

    // Gone before it replaced anything: the file is the one the process bound.
    let inode = std::fs::metadata(&control).unwrap();
    let taken = take_with_ready(&handoff, &want).await.unwrap().unwrap();
    assert!(old._setup.core.is_held());
    drop(taken);
    within("the hold is let go", async {
        while old._setup.core.is_held() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    settle().await;
    assert_eq!(std::fs::metadata(&control).unwrap().ino(), inode.ino());

    // Replaced by a successor that is then killed: a file no one listens on.
    let taken = take_with_ready(&handoff, &want).await.unwrap().unwrap();
    std::fs::remove_file(&control).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&control).unwrap());
    assert!(StdStream::connect(&control).is_err());
    drop(taken);
    let said = control_answers(&control).await;
    assert_eq!(said["ok"], true, "{said}");
    let reply = {
        let mut stream = BufReader::new(StdStream::connect(&control).unwrap());
        stream
            .get_mut()
            .write_all(format!("attach {digest} for after-control\n").as_bytes())
            .unwrap();
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    assert_eq!(reply["ok"], true, "{reply}");
    drop(client);
    drop(old);
}

/// Asserts that a successor that reports ready takes over, and not before: after its take has
/// returned (the running process has said `done`) the running process still answers its clients
/// and has not ended, and it holds its writes; once the successor says `ready` the running process
/// ends, leaves the mount, and the successor serves the clients that connect.
#[tokio::test]
async fn the_old_instance_ends_when_the_successor_says_ready_and_not_before() {
    let (rig, _) = attached("nfs-ready-says");
    let (mut old, mut client, root, runner) = running(&rig).await;
    let handoff = Handoff {
        socket: rig.dir.path().join("handoff.sock"),
        wait: WAIT,
    };
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &[old.address],
    };
    let mut taken = take_with_ready(&handoff, &want).await.unwrap().unwrap();
    settle().await;
    assert!(!old.server.is_finished(), "done is not the end");
    assert!(old._setup.core.is_held());
    assert_eq!(getattr(&mut client, &root).await, 0);
    let listener = taken.nfs.pop().unwrap();
    taken.ready.take().unwrap().send().await.unwrap();
    drop(client);
    old.handed_over().await;
    old.stop().await.unwrap();
    assert!(runner.calls().iter().all(|call| call[0] != "/sbin/umount"));
    drop(listener);
}

/// Asserts that a successor whose `ready` finds the running process gone from the connection (it
/// did not wait, and serves on) does not serve beside it: the start fails, and the error says it
/// is the `ready` that failed.
#[tokio::test]
async fn a_successor_whose_ready_finds_the_old_instance_gone_does_not_serve() {
    let (rig, _) = attached("nfs-ready-refused");
    let (listener, addr) = crate::server::handoff::testing::listening();
    let socket = rig.dir.path().join("handoff.sock");
    let fake = crate::server::handoff::testing::pretending(
        &socket,
        vec!["offer".to_owned(), format!("nfs {addr}")],
        vec![listener],
        Some(vec!["done".to_owned()]),
    );
    let why = Running::start(
        &handing(&rig, addr.port()),
        crate::server::treefs::nfs::run::tests::Recorder::mounted_at(&rig.mountpoint),
        crate::server::treefs::nfs::run::tests::euid(),
        Some(crate::server::treefs::nfs::fixtures::endless()),
        crate::server::treefs::nfs::fixtures::EPOCH,
    )
    .await
    .err()
    .unwrap();
    fake.await.unwrap();
    assert!(
        why.to_string()
            .contains("telling the running instance it can end"),
        "{why}"
    );
    assert_eq!(why.status(), 1);
}

/// Asserts that when the hold is let go and the control socket cannot be bound again where the
/// successor replaced it (the path is a directory), the process says so, in the words that tell
/// what it cannot do, and goes on serving; and that a hold let go with the control socket the one
/// this process bound binds nothing.
#[tokio::test]
async fn a_control_socket_that_cannot_be_bound_again_is_said_and_the_process_serves_on() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

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

    let (rig, _) = attached("nfs-ready-nocontrol");
    let (old, mut client, root, _) = running(&rig).await;
    let control = rig.settings().control_socket;
    let before = std::fs::metadata(&control).unwrap().ino();
    old._keeper.hold(true);
    old._keeper.hold(false);
    assert_eq!(std::fs::metadata(&control).unwrap().ino(), before);
    assert!(old._keeper.revived.lock().unwrap().is_none());

    std::fs::remove_file(&control).unwrap();
    std::fs::create_dir(&control).unwrap();
    let stderr = Stderr::default();
    let log =
        crate::server::build_info::stderr_log::LineLog::new(stderr.clone(), tracing::Level::WARN);
    tracing::subscriber::with_default(log, || old._keeper.hold(false));
    let said = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
    assert!(
        said.starts_with("tierstore fs: the control socket at "),
        "{said}"
    );
    assert!(
        said.contains("); this instance goes on serving and takes no attach until it restarts"),
        "{said}"
    );
    assert!(old._keeper.revived.lock().unwrap().is_none());
    assert_eq!(getattr(&mut client, &root).await, 0);
}
