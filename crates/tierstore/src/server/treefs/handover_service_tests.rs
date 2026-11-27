//! The mount in the one hand-over (`server/handoff/`: `take 1`, `ready`, `mount`) and the `bye` of
//! `take-fs 1`, over a process with a stand-in for the kernel's end of `/dev/fuse`: who reads the
//! device at each step, what a taker that goes away at each step leaves, and the store entries.

use std::collections::BTreeSet;
use std::os::fd::AsFd as _;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;

use super::config::FsSettings;
use super::fakedev::{FakeDevice, instance, outcome, pair};
use super::handover::{self, HandoverError, check_device, offered, take};
use super::init_record::{self, Record};
use super::ino;
use super::run::{Prepared, Running};
use super::sd::{FUSE_NAME, Manager};
use super::testing::{FakeManager, Rig, sample};
use crate::server::handoff::testing::{ask, connect, send, settle, within};
use crate::server::handoff::{Ask, Handoff, MountWant, Offer, Prepare, Want, take_service, wire};

/// The waits of the tests: only a hung test reaches them.
const WAIT: Duration = Duration::from_secs(3600);

/// The text of a removal of the descriptor's store entry.
const UNSTORE: &str = "FDSTOREREMOVE=1\nFDNAME=fuse";

/// A running instance of the one process over a stand-in for the kernel, that offers its mount
/// on its hand-over socket, and removes its store entry from `manager` at `ready`. The scratch
/// directory is the last to go: the manager is told to end at its socket there.
struct One {
    /// The instance's own state, which a test asks whether it reads the device.
    running: Arc<Running>,
    kernel: FakeDevice,
    manager: FakeManager,
    serving: tokio::task::JoinHandle<()>,
    settings: FsSettings,
    rig: Rig,
}

/// [`One`], in a scratch directory called `name`.
fn one(name: &str) -> One {
    let rig = Rig::new(name, sample());
    let settings = rig.settings();
    init_record::write(
        &rig.runtime,
        &Record::first(outcome(false), "mount-1".to_owned()),
    )
    .unwrap();
    let (device, kernel) = pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running = Arc::new(
        Running::adopt((&settings, &rig.calls()), prepared, device, (None, false)).unwrap(),
    );
    let manager = FakeManager::start(&rig.dir.path().join("old-manager.sock"));
    let store = Manager::new(Some(manager.socket().as_os_str()));
    let mount = offered(
        &running,
        &settings,
        store,
        (Arc::new(|_| {}), Arc::default()),
    );
    let offer = Offer::bind(&settings.handoff_socket, Vec::new())
        .unwrap()
        .with_ready_wait(WAIT)
        .with_mount(mount);
    let serving = tokio::spawn(offer.serve());
    One {
        running,
        kernel,
        manager,
        serving,
        settings,
        rig,
    }
}

/// Takes the mount of `old` with the one hand-over; `prepare` runs with the device before `ack`.
async fn take_one(old: &One, prepare: Prepare<'_>) -> crate::server::handoff::Inherited {
    let handoff = Handoff {
        socket: old.settings.handoff_socket.clone(),
        wait: WAIT,
    };
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &[],
    };
    let ask = Ask {
        mount: Some(MountWant {
            mountpoint: &old.settings.mountpoint,
            control: &old.settings.control_socket,
            wait: WAIT,
            // The stand-in is a socket, not the FUSE device.
            device: |_| Ok(()),
            prepare,
        }),
        lock: None,
        leaves_another_lock: false,
    };
    let taken = within("the take", take_service(&handoff, &want, ask)).await;
    taken.unwrap().expect("an instance answers")
}

/// The successor of a take: the state read again and the connection taken over, read only from
/// [`Running::start_reading`].
fn successor(old: &One, taken: &mut crate::server::handoff::Inherited) -> Running {
    let mount = taken.mount.take().expect("the mount");
    let prepared = Prepared::load(&old.settings).unwrap();
    Running::take_over(
        (&old.settings, &old.rig.calls()),
        prepared,
        mount.device,
        mount.control,
    )
    .unwrap()
}

/// Asserts the reading of the device across the one hand-over: of twenty requests queued before
/// the take, the old instance answers those it read, and from `ack` on reads no other (a request
/// that comes then is not answered, nor are those it had not read); the successor reads only from
/// its `start_reading` after `bye`, and answers the rest. Each request is answered once. It would
/// catch a request read by both processes, or by neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_old_reads_no_request_after_ack_and_answers_what_it_read() {
    let old = one("tfs-svc-reads");
    for unique in 1..=20 {
        old.kernel.getattr(unique, ino::ROOT);
    }
    let mut taken = take_one(&old, Box::new(|_| Ok(()))).await;
    // The old instance's readers stopped before `done`.
    assert!(!old.running.reading());
    let by_old = old.kernel.waiting();
    old.kernel.getattr(21, ino::ROOT);
    settle().await;
    old.kernel.assert_silent();
    let next = successor(&old, &mut taken);
    assert!(!next.reading());
    old.kernel.assert_silent();
    let ready = taken.ready.take().expect("the step");
    let _predecessor = within("bye", ready.send_keeping()).await.unwrap();
    assert!(!old.running.reading(), "never again after ready");
    next.start_reading().unwrap();
    assert!(next.reading());
    let mut by_new = Vec::new();
    while by_old.len() + by_new.len() < 21 {
        by_new.push(old.kernel.reply().unique);
    }
    let all: BTreeSet<u64> = by_old.iter().chain(&by_new).copied().collect();
    assert_eq!(
        all,
        (1..=21).collect::<BTreeSet<u64>>(),
        "each answered once"
    );
    assert!(by_new.contains(&21));
    within("the end of the offer", old.serving).await.unwrap();
}

/// Asserts that the successor reads the device only after the old instance's `bye`: between the
/// take and `bye` a request is answered by nobody, and is answered by the successor once it
/// reads. It would catch a successor that reads while the old instance may still resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_new_reads_the_device_only_after_bye() {
    let old = one("tfs-svc-bye");
    old.assert_answers(1);
    let mut taken = take_one(&old, Box::new(|_| Ok(()))).await;
    let next = successor(&old, &mut taken);
    // Neither process has a reader between the take and `bye`.
    assert!(!next.reading());
    assert!(!old.running.reading());
    old.kernel.getattr(2, ino::ROOT);
    settle().await;
    old.kernel.assert_silent();
    let _predecessor = within("bye", taken.ready.take().unwrap().send_keeping())
        .await
        .unwrap();
    assert!(!next.reading(), "the successor starts its readers itself");
    old.kernel.assert_silent();
    next.start_reading().unwrap();
    assert_eq!(old.kernel.reply().unique, 2);
}

impl One {
    /// Asserts that a request `unique` is answered.
    fn assert_answers(&self, unique: u64) {
        self.kernel.getattr(unique, ino::ROOT);
        let reply = self.kernel.reply();
        assert_eq!((reply.unique, reply.error), (unique, 0));
    }

    /// Waits for the hand-over socket, which the instance makes again after a taker that went away
    /// after `ack` (it removed the file then).
    async fn socket_again(&self) {
        let socket = &self.settings.handoff_socket;
        within("the hand-over socket again", async {
            while !socket.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
}

/// Asserts that a taker gone after `ack` and before `ready` leaves the old instance reading the
/// device again, and a taker gone after `ready` does not: the old instance has handed over for
/// good, and a request then waits for whoever holds the device in its store. It would catch an old
/// instance that never resumes, or one that resumes beside a successor that may read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taker_gone_before_ready_leaves_the_old_resuming_and_one_gone_after_ready_does_not() {
    let old = one("tfs-svc-gone");
    let taken = take_one(&old, Box::new(|_| Ok(()))).await;
    assert!(!old.running.reading());
    drop(taken);
    old.assert_answers(1);
    assert!(old.running.reading(), "resumed");
    old.socket_again().await;
    let taken = take_one(&old, Box::new(|_| Ok(()))).await;
    let predecessor = within("bye", taken.ready.unwrap().send_keeping())
        .await
        .unwrap();
    drop(predecessor);
    // The offer has ended: nothing of the instance can resume the mount any more.
    within("the end of the offer", old.serving).await.unwrap();
    assert!(!old.running.reading());
    old.kernel.getattr(2, ino::ROOT);
    settle().await;
    old.kernel.assert_silent();
}

/// Asserts that a successor killed before `ready`, at each frame of the conversation (after it
/// connected, after its request, after the offer, after `ack`, after `done`), leaves the old
/// instance serving the device and its hand-over socket for the next taker. It would catch an old
/// instance left quiesced, or without a hand-over socket, by a taker that died at one point.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_killed_before_ready_leaves_the_old_serving() {
    let old = one("tfs-svc-killed");
    let socket = old.settings.handoff_socket.clone();
    let request = ["take 1", "ready", "mount"];
    for cut in 0..5 {
        let mut taker = connect(&socket).await;
        if cut >= 1 {
            send(&taker, &request).await;
        }
        if cut >= 2 {
            // The conversation of the taker killed before may not have ended yet.
            let mut offer = wire::recv(&taker).await.unwrap();
            while offer.lines == ["refused", "another taker is taking the sockets"] {
                tokio::task::yield_now().await;
                taker = connect(&socket).await;
                offer = ask(&taker, &request).await;
            }
            assert_eq!(offer.fds.len(), 2, "{:?}", offer.lines);
        }
        if cut >= 3 {
            send(&taker, &["ack"]).await;
        }
        if cut >= 4 {
            assert_eq!(wire::recv(&taker).await.unwrap().lines, ["done"]);
        }
        let was = inode(&socket);
        drop(taker);
        old.assert_answers(100 + cut);
        if cut >= 3 {
            // The socket's file was removed at `ack`, and is made again once the taker is gone.
            within("the hand-over socket again", async {
                while inode(&socket) == was || inode(&socket).is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        }
    }
    assert!(!old.serving.is_finished());
    assert!(!old.manager.texts().iter().any(|text| text == UNSTORE));
    old.serving.abort();
}

/// The inode of the file at `path`, if there is one.
fn inode(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).ok().map(|meta| meta.ino())
}

/// Asserts that a successor killed after `ready` has the device in its own store: the old
/// instance does not resume, and a start of the successor's unit again adopts the stored
/// descriptor and serves the requests that waited. It would catch a mount that nobody can serve
/// after a successor's crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_killed_after_ready_is_started_again_and_adopts_from_its_store() {
    let old = one("tfs-svc-store");
    let store = FakeManager::start(&old.rig.dir.path().join("new-manager.sock"));
    let manager = Manager::new(Some(store.socket().as_os_str()));
    let taken = take_one(
        &old,
        Box::new(|device| {
            manager
                .store(FUSE_NAME, device.as_fd())
                .and_then(|()| manager.barrier())
                .map_err(|why| why.to_string())
        }),
    )
    .await;
    let predecessor = within("bye", taken.ready.unwrap().send_keeping())
        .await
        .unwrap();
    // The successor dies before it read anything.
    drop((predecessor, taken.mount));
    within("the end of the offer", old.serving).await.unwrap();
    assert!(!old.running.reading());
    old.kernel.getattr(1, ino::ROOT);
    settle().await;
    old.kernel.assert_silent();
    let stored = store.take_fds("FDSTORE=1\nFDNAME=fuse");
    let device = stored.into_iter().next().expect("the stored descriptor");
    let prepared = Prepared::load(&old.settings).unwrap();
    let again = Running::adopt(
        (&old.settings, &old.rig.calls()),
        prepared,
        device,
        (None, false),
    )
    .unwrap();
    assert_eq!(old.kernel.reply().unique, 1);
    drop(again);
}

/// Asserts that the old instance removes its own descriptor store entry at `ready` and not
/// before: nothing is removed by a taker that goes before `ready`, and the removal is sent by the
/// time the successor has its `bye`. It would catch an old instance whose unit, started again after
/// a crash in its drain, adopts the device the successor reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_old_removes_its_store_entry_at_ready() {
    let old = one("tfs-svc-unstore");
    drop(take_one(&old, Box::new(|_| Ok(()))).await);
    old.assert_answers(1);
    old.socket_again().await;
    assert!(!old.manager.texts().iter().any(|text| text == UNSTORE));
    let taken = take_one(&old, Box::new(|_| Ok(()))).await;
    assert!(!old.manager.texts().iter().any(|text| text == UNSTORE));
    let _predecessor = within("bye", taken.ready.unwrap().send_keeping())
        .await
        .unwrap();
    within("the removal", async {
        while !old.manager.texts().iter().any(|text| text == UNSTORE) {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Asserts the `bye` of `take-fs 1`: a taker that asks for it with `ready` is answered `bye` to its
/// `ready`, the running instance removes its store entry and ends, and once the taker is gone it
/// does not read again; a taker of a release before it (`take-fs 1` alone) is answered nothing to
/// its `ready`. It would catch an old `tierstore fs` that resumes after its successor read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_fs_says_bye_and_the_old_fs_process_never_resumes_after_it() {
    let old = instance("tfs-svc-takefs", WAIT, None);
    let taken = within("the take", take(&old.paths(), Box::new(|_| Ok(()))))
        .await
        .unwrap()
        .expect("answered");
    within("bye", taken.notice.ready()).await.unwrap();
    within("the end", old.serving).await.unwrap().unwrap();
    assert!(!old.running.reading());
    drop(taken);
    old.kernel.getattr(1, ino::ROOT);
    settle().await;
    old.kernel.assert_silent();

    for (request, answer) in [
        (&["take-fs 1", "ready"][..], Some(vec!["bye".to_owned()])),
        (&["take-fs 1"][..], None),
    ] {
        let old = instance("tfs-svc-takefs-raw", WAIT, None);
        let taker = connect(&old.settings.handoff_socket).await;
        let offer = ask(&taker, request).await;
        assert_eq!(offer.fds.len(), 2);
        assert_eq!(ask(&taker, &["ack"]).await.lines, ["done"]);
        send(&taker, &["ready"]).await;
        assert_eq!(
            wire::recv(&taker).await.ok().map(|frame| frame.lines),
            answer
        );
        within("the end", old.serving).await.unwrap().unwrap();
    }
}

/// Asserts the taker of `take-fs 1` against a running instance of a release before the `bye`: it
/// asks with `ready`, is refused as not understood, asks with `take-fs 1` alone, takes the mount,
/// and its `ready` waits for no answer. It would catch a start of this release that cannot take
/// the mount from the release before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_fs_taker_asks_a_previous_release_without_ready() {
    let rig = Rig::new("tfs-svc-previous", sample());
    let settings = rig.settings();
    let listener = UnixListener::bind(&settings.handoff_socket).unwrap();
    let (device, _kernel) = pair();
    let control =
        std::os::unix::net::UnixListener::bind(rig.dir.path().join("offered.sock")).unwrap();
    let lines = handover::offer_lines(&settings.mountpoint, &settings.control_socket);
    let previous = async {
        let mut requests = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let request = wire::recv(&stream).await.unwrap().lines;
            requests.push(request.clone());
            if request != ["take-fs 1"] {
                let refused = [
                    "refused".to_owned(),
                    "the request is not understood".to_owned(),
                ];
                wire::send(&stream, &refused, &[]).await.unwrap();
                continue;
            }
            wire::send(&stream, &lines, &[device.as_fd(), control.as_fd()])
                .await
                .unwrap();
            assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ack"]);
            wire::send(&stream, &["done".to_owned()], &[])
                .await
                .unwrap();
            assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ready"]);
            return requests;
        }
    };
    let paths = handover::Paths {
        socket: &settings.handoff_socket,
        mountpoint: &settings.mountpoint,
        control: &settings.control_socket,
        wait: WAIT,
        device: |_| Ok(()),
    };
    let taking = async {
        let taken = take(&paths, Box::new(|_| Ok(()))).await.unwrap().unwrap();
        taken.notice.ready().await.unwrap();
    };
    let ((), requests) = tokio::join!(taking, previous);
    assert_eq!(requests, [vec!["take-fs 1", "ready"], vec!["take-fs 1"]]);
}

/// Asserts the checks of a taker of `take-fs 1` before `ack`: a running instance of another user
/// is refused before anything is asked, by name; a device the check refuses and a control socket
/// that does not listen are each refused with a `nak`; and a frame other than `bye` after `ready`
/// is an error. It would catch a successor that takes a mount from a process that is not the
/// service's, or of the wrong kind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_fs_taker_checks_its_peer_and_the_kinds_before_ack() {
    let old = instance("tfs-svc-peer", WAIT, None);
    let me = rustix::process::geteuid().as_raw();
    let refused = handover::take_as(&old.paths(), Box::new(|_| Ok(())), me.wrapping_add(1))
        .await
        .err()
        .unwrap();
    assert_eq!(
        refused.to_string(),
        format!(
            "hand-over: checking the running instance's user: the running instance is the user {me}"
        )
    );
    old.assert_serving(1);
    let refuse: fn(std::os::fd::BorrowedFd<'_>) -> std::io::Result<()> =
        |_| Err(std::io::Error::other("not the FUSE device"));
    let paths = handover::Paths {
        device: refuse,
        ..old.paths()
    };
    let refused = take(&paths, Box::new(|_| Ok(()))).await;
    assert!(
        matches!(refused, Err(HandoverError::Unprepared(ref why)) if why == "not the FUSE device")
    );
    old.assert_serving(2);
    old.serving.abort();

    // A control socket that does not listen, and an answer to `ready` that is not `bye`.
    let rig = Rig::new("tfs-svc-kinds", sample());
    let settings = rig.settings();
    let listener = UnixListener::bind(&settings.handoff_socket).unwrap();
    let (device, _kernel) = pair();
    let control =
        std::os::unix::net::UnixListener::bind(rig.dir.path().join("offered.sock")).unwrap();
    let not_listening = std::fs::File::open(rig.dir.path()).unwrap();
    let lines = handover::offer_lines(&settings.mountpoint, &settings.control_socket);
    let peer = async {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &lines, &[device.as_fd(), not_listening.as_fd()])
            .await
            .unwrap();
        let first = wire::recv(&stream).await.unwrap().lines;
        let (stream, _) = listener.accept().await.unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &lines, &[device.as_fd(), control.as_fd()])
            .await
            .unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &["done".to_owned()], &[])
            .await
            .unwrap();
        let _ = wire::recv(&stream).await.unwrap();
        wire::send(&stream, &["so long".to_owned()], &[])
            .await
            .unwrap();
        first
    };
    let paths = handover::Paths {
        socket: &settings.handoff_socket,
        mountpoint: &settings.mountpoint,
        control: &settings.control_socket,
        wait: WAIT,
        device: |_| Ok(()),
    };
    let taking = async {
        let refused = take(&paths, Box::new(|_| Ok(()))).await.err().unwrap();
        let taken = take(&paths, Box::new(|_| Ok(()))).await.unwrap().unwrap();
        (refused, taken.notice.ready().await.err().unwrap())
    };
    let ((refused, not_bye), first) = tokio::join!(taking, peer);
    assert_eq!(first, ["nak"]);
    assert_eq!(
        refused.to_string(),
        "hand-over: not taken: the descriptor of the control socket is not a listening Unix socket"
    );
    assert_eq!(
        not_bye.to_string(),
        "hand-over: the running instance sent something other than bye after ready"
    );
}

/// Asserts the check of the FUSE device: the character device of the FUSE numbers is it; another
/// character device, and a regular file, are not, and a descriptor of a file is refused with words
/// that say so. It would catch a successor that stores something other than the connection as its
/// mount.
#[test]
fn only_the_fuse_device_is_the_fuse_device() {
    let character = 0o020_644;
    let device = rustix::fs::makedev(10, 229);
    assert!(handover::is_fuse(character, device));
    assert!(!handover::is_fuse(character, rustix::fs::makedev(1, 3)));
    assert!(!handover::is_fuse(0o100_644, device));
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-svc-device");
    let path = dir.path().join("file");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let refused = check_device(file.as_fd()).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the offered device is not the FUSE device"
    );
}

/// Asserts the adapter's handling of what cannot be done: a resume that fails is told to the
/// caller's `stuck` (the start ends the process there), one that succeeds is not, and a give-up
/// whose store cannot be reached still gives the mount up. It would catch a mount left unserved
/// with nobody told, or a give-up that stops halfway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_resume_is_told_and_an_unreachable_store_does_not_stop_the_give_up() {
    static STUCK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let note: fn(&std::io::Error) = |_| {
        let _ = STUCK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    };
    handover::stuck_on(Ok(()), &note);
    assert_eq!(STUCK.load(std::sync::atomic::Ordering::SeqCst), 0);
    handover::stuck_on(Err(std::io::Error::other("no readers")), &note);
    assert_eq!(STUCK.load(std::sync::atomic::Ordering::SeqCst), 1);

    let rig = Rig::new("tfs-svc-unreachable", sample());
    let settings = rig.settings();
    init_record::write(
        &rig.runtime,
        &Record::first(outcome(false), "mount-1".to_owned()),
    )
    .unwrap();
    let (device, kernel) = pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running = Arc::new(
        Running::adopt((&settings, &rig.calls()), prepared, device, (None, false)).unwrap(),
    );
    let nobody = Manager::new(Some(rig.dir.path().join("nobody-home").as_os_str()));
    let mount = offered(
        &running,
        &settings,
        nobody,
        (Arc::new(note), Arc::default()),
    );
    (mount.quiesce)();
    (mount.give_up)();
    assert!(!running.reading());
    kernel.getattr(1, ino::ROOT);
    kernel.assert_silent();
}

/// Asserts that a request of `take-fs` with a line this release does not know is refused as not
/// understood: another version with the `ready` line, and another second line. It would catch a
/// running instance that takes a later release's request for one of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_fs_request_with_a_line_this_release_does_not_know_is_refused() {
    let old = instance("tfs-svc-unknown", WAIT, None);
    for request in [&["take-fs 2", "ready"][..], &["take-fs 1", "later"]] {
        let taker = connect(&old.settings.handoff_socket).await;
        let refused = ask(&taker, request).await;
        assert_eq!(
            refused.lines,
            ["refused", "the request is not understood"],
            "{request:?}"
        );
    }
    old.assert_serving(1);
    old.serving.abort();
}

/// Asserts that the `take-fs 1` offer of a process of the one-process layout shares its claim of
/// the mount with the one hand-over: while a taker of the one hand-over is at the mount, a request
/// is refused as busy; once that taker is gone, a request is offered the mount, and a taker that
/// goes away before `ack` leaves the claim free again; once the mount was given up on the one
/// hand-over, a request is refused as by an instance that holds none, and the socket's file, which
/// the successor binds after its `bye`, is left in place when the offer goes. It would catch a
/// mount offered on both conversations at once, or after it was given up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_fs_shares_the_claim_of_the_mount_with_the_one_hand_over() {
    use crate::server::handoff::{Claim, Holding};
    let rig = Rig::new("tfs-svc-claim", sample());
    let settings = rig.settings();
    let record = Record::first(outcome(false), "mount-1".to_owned());
    init_record::write(&rig.runtime, &record).unwrap();
    let (device, _kernel) = pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running = Arc::new(
        Running::adopt((&settings, &rig.calls()), prepared, device, (None, false)).unwrap(),
    );
    let claim = Arc::new(Claim::default());
    let offer = handover::Offer::bind(
        &settings.handoff_socket,
        &settings.mountpoint,
        &settings.control_socket,
        WAIT,
    )
    .unwrap()
    .with_claim(Arc::clone(&claim));
    let serving = tokio::spawn(offer.serve(Arc::clone(&running)));
    let request = ["take-fs 1", "ready"];

    assert_eq!(claim.claim(), Holding::Here);
    let busy = ask(&connect(&settings.handoff_socket).await, &request).await;
    assert_eq!(busy.lines, ["refused", "another taker is taking the mount"]);
    claim.release();
    let taker = connect(&settings.handoff_socket).await;
    let offered = ask(&taker, &request).await;
    assert_eq!(offered.lines[0], "offer-fs");
    assert_eq!(claim.holding(), Holding::Taking);
    drop((taker, offered));
    crate::server::local_cache::testing::until("the claim is free again", || {
        claim.holding() == Holding::Here
    })
    .await;

    assert_eq!(claim.claim(), Holding::Here);
    claim.give_up();
    let gone = ask(&connect(&settings.handoff_socket).await, &request).await;
    assert_eq!(gone.lines, ["refused", "this instance holds no mount"]);
    serving.abort();
    assert!(serving.await.is_err(), "the offer was ended");
    assert!(settings.handoff_socket.exists());
}

/// Asserts that the mount the one hand-over offers keeps no part of the process's state of it
/// alive: once the process has ended its service of the mount (a hang or a stop takes that state
/// whole), the offer shares nothing and its other steps do nothing, and a resume is not told as
/// stuck. It would catch an offer that keeps the state alive, which makes the end of a hung mount
/// fail to take it out of the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_offered_after_its_service_ended_shares_nothing() {
    static STUCK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let rig = Rig::new("tfs-svc-ended", sample());
    let settings = rig.settings();
    let record = Record::first(outcome(false), "mount-1".to_owned());
    init_record::write(&rig.runtime, &record).unwrap();
    let (device, _kernel) = pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running = Arc::new(
        Running::adopt((&settings, &rig.calls()), prepared, device, (None, false)).unwrap(),
    );
    let nobody = Manager::new(Some(rig.dir.path().join("nobody-home").as_os_str()));
    let note: fn(&std::io::Error) = |_| {
        let _ = STUCK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    };
    let mount = offered(
        &running,
        &settings,
        nobody,
        (Arc::new(note), Arc::default()),
    );
    let alone = Arc::try_unwrap(running)
        .ok()
        .expect("the offer holds no part of it");
    alone.leave();
    let shared = (mount.share)().expect_err("nothing is shared");
    assert_eq!(shared.kind(), std::io::ErrorKind::NotFound);
    (mount.quiesce)();
    (mount.resume)();
    (mount.give_up)();
    assert_eq!(STUCK.load(std::sync::atomic::Ordering::SeqCst), 0);
}
