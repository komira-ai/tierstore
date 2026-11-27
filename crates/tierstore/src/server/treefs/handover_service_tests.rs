//! The mount in the one hand-over (`server/handoff/`: `take 1`, `ready`, `mount`), over a process
//! with a stand-in for the kernel's end of `/dev/fuse`: who reads the device at each step, what a
//! taker that goes away at each step leaves, and the store entries.

use std::collections::BTreeSet;
use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::config::FsSettings;
use super::fakedev::{FakeDevice, outcome, pair};
use super::handover::{self, check_device, offered};
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
    /// The socket the instance offers on.
    socket: PathBuf,
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
    let socket = rig.dir.path().join("handoff.sock");
    let offer = Offer::bind(&socket, Vec::new())
        .unwrap()
        .with_ready_wait(WAIT)
        .with_mount(mount);
    let serving = tokio::spawn(offer.serve());
    One {
        running,
        kernel,
        manager,
        serving,
        socket,
        settings,
        rig,
    }
}

/// Takes the mount of `old` with the one hand-over; `prepare` runs with the device before `ack`.
async fn take_one(old: &One, prepare: Prepare<'_>) -> crate::server::handoff::Inherited {
    let handoff = Handoff {
        socket: old.socket.clone(),
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
        let socket = &self.socket;
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
    let socket = old.socket.clone();
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
