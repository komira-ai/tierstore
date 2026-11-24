//! The one hand-over from the running instance's side: the lock and the mount across a taker that
//! does not complete, the lock across the end of either process, `written`, and a release before
//! this one on either side (B8: the previous release's frames are kept as fixtures, `fixtures/`).

use std::net::{SocketAddr, TcpListener};
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use tokio::net::UnixListener;

use crate::server::handoff::testing::{
    ask, connect, fake_mount, handoff, listening, scratch, send, settle, socket, texts, told,
    within,
};
use crate::server::handoff::wire::{self, Frame};
use crate::server::handoff::{Front, Hold, Offer, Socket, Want};
use crate::server::local_cache::testing::Rig;
use crate::server::local_cache::{LocalCache, Mode};
use crate::server::lock::try_take;

use super::super::take::{Ask, take_service};
use super::super::written::follow;
use super::NOT_UNDERSTOOD;

/// The wait of the `ready` step: only a hung test reaches it.
const WAIT: Duration = Duration::from_secs(3600);

/// The frames a taker of the previous release sends that asks for the `ready` step (the `nfs`
/// front's taker), in order, as on the wire: its request, `ack`, `ready`.
const PREVIOUS_TAKER: &str = include_str!("fixtures/previous_taker.frames");

/// The frames a cache node of the previous release sends, in order, as on the wire: its answer to
/// a request it does not understand (any but `take 1`: it takes no `ready` step), its offer of the
/// one S3 socket (`{s3}` stands for its address), and `done`.
const PREVIOUS_OFFER: &str = include_str!("fixtures/previous_offer.frames");

/// The frames of a fixture, each as its lines.
fn frames(text: &str) -> Vec<Vec<String>> {
    let frames = text.split_terminator("\n\n");
    frames
        .map(|frame| frame.split('\n').map(str::to_owned).collect())
        .collect()
}

/// Waits until `done` holds, looking again every few milliseconds: for what another task or the
/// old instance's look at its writes does.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    within(what, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

/// Whether a third process would take the lock at `path` now: it takes and lets it go at once.
fn free(path: &Path) -> bool {
    try_take(path).unwrap().is_some()
}

/// An old instance served on a thread and a runtime of its own, so that its end is the end of
/// everything it holds, as the end of its process is: every task, every descriptor, every cache.
struct Elsewhere {
    exit: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Elsewhere {
    /// Serves the offer `make` makes, on its own runtime, until [`Elsewhere::exit`].
    fn serve(make: impl FnOnce() -> Offer + Send + 'static) -> Self {
        let (exit, exited) = mpsc::channel::<()>();
        let (bound, is_bound) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                drop(tokio::spawn(make().serve()));
                bound.send(()).unwrap();
                let _ = tokio::task::spawn_blocking(move || exited.recv()).await;
            });
            // The end of the process: every task and what it holds is dropped.
            drop(runtime);
        });
        is_bound.recv().unwrap();
        Self { exit, thread }
    }

    /// Ends the old instance and everything it holds.
    fn exit(self) {
        self.exit.send(()).unwrap();
        self.thread.join().unwrap();
    }
}

/// The offer of the old instance at `path` with its S3 socket `listener` and the lock of `cache`.
fn cache_offer(path: &Path, listener: &TcpListener, cache: LocalCache) -> Offer {
    Offer::bind(path, vec![socket(Front::S3, listener)])
        .unwrap()
        .with_ready_wait(WAIT)
        .with_lock(cache)
}

/// A take of the sockets and the lock at `path` by a successor whose S3 address is `addr` and
/// whose lock file is `lock`: what it inherited.
async fn take_lock(path: &Path, addr: SocketAddr, lock: &Path) -> super::super::take::Inherited {
    let addrs = [addr];
    let want = Want {
        s3: &addrs,
        oci: &[],
        nfs: &[],
    };
    let ask = Ask {
        mount: None,
        lock: Some(lock),
    };
    let taken = take_service(&handoff(path.to_owned()), &want, ask).await;
    taken.unwrap().expect("an instance answers")
}

/// Asserts that a taker that is offered the lock and says `nak`, or goes away after `ack` and
/// before `ready`, leaves the lock where it was: a third process does not take it, and the old
/// cache goes on writing; the instance serves on. It would catch an old instance that lets the
/// lock go, or stops writing, for a successor that never came.
#[tokio::test]
async fn a_taker_that_naks_or_goes_away_leaves_the_lock_held() {
    let rig = Rig::new("handoff-lock-nak");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, _) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone()).serve());
    let taker = connect(&path).await;
    let offered = ask(&taker, &["take 1", "ready"]).await;
    assert_eq!(
        offered.lines.last().unwrap(),
        &format!("lock {}", lock.display())
    );
    send(&taker, &["nak"]).await;
    drop((taker, offered));
    assert!(!free(&lock));
    assert_eq!(old.mode(), Mode::ReadWrite);

    let taker = connect(&path).await;
    let offered = ask(&taker, &["take 1", "ready"]).await;
    let done = ask(&taker, &["ack"]).await;
    assert_eq!(texts(&done.lines), ["done"]);
    drop((taker, offered));
    // The control socket is made again once the instance has seen the taker go.
    until("the control socket again", || path.exists()).await;
    assert!(!free(&lock));
    assert_eq!(old.mode(), Mode::ReadWrite);
    assert!(!serving.is_finished());
    serving.abort();
}

/// Asserts what an instance that holds the mount does for a taker that goes away after `ack`: the
/// hold is told at `ack` and the mount quiesced after it, and when the taker is gone the mount is
/// resumed before the hold is let go; the mount is never given up. It would catch an old instance
/// that leaves its mount unread, or writes before it reads again.
#[tokio::test]
async fn a_taker_gone_after_ack_leaves_the_mount_resumed_and_the_hold_let_go() {
    let (dir, path) = scratch();
    let (mount, steps, _kernel) = fake_mount(dir.path());
    let (listener, _) = listening();
    let held = std::sync::Arc::clone(&steps);
    let hold: Hold = std::sync::Arc::new(move |on| {
        held.lock()
            .unwrap()
            .push(if on { "hold" } else { "let go" });
    });
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &listener)])
            .unwrap()
            .with_ready_wait(WAIT)
            .with_hold(hold)
            .with_mount(mount)
            .serve(),
    );
    let taker = connect(&path).await;
    let offered = ask(&taker, &["take 1", "ready", "mount"]).await;
    let done = ask(&taker, &["ack"]).await;
    assert_eq!(texts(&done.lines), ["done"]);
    assert_eq!(told(&steps), ["hold", "quiesce"]);
    drop((taker, offered));
    until("the resume", || told(&steps).len() == 4).await;
    assert_eq!(told(&steps), ["hold", "quiesce", "resume", "let go"]);
    assert!(!serving.is_finished());
    serving.abort();
}

/// Asserts that the lock outlives the end of the old instance after `ready`: with the old instance
/// and everything it held gone (its runtime, its tasks, its cache and its descriptor of the lock),
/// a third process does not take the lock, which the successor holds; the successor takes the
/// write role, and the lock is free once it is gone too. It would catch an old instance whose end
/// unlocks the lock under its successor.
#[tokio::test]
async fn the_lock_outlives_the_old_process_exit() {
    let rig = Rig::new("handoff-lock-exit");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let new = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let (at, copy) = (path.clone(), listener.try_clone().unwrap());
    let elsewhere = Elsewhere::serve(move || cache_offer(&at, &copy, old));
    let taken = take_lock(&path, addr, &lock).await;
    let predecessor = taken.ready.expect("the step").send_keeping().await.unwrap();
    elsewhere.exit();
    assert!(!free(&lock), "the successor holds it");
    let received = taken.lock.expect("the lock");
    within("the take", follow(predecessor, received, new.clone())).await;
    assert_eq!(new.mode(), Mode::ReadWrite);
    assert!(!free(&lock));
    drop(new);
    assert!(free(&lock));
}

/// Asserts that the old instance unlocks when its successor dies after `ready`: while the
/// successor lives, a third process does not take the lock; once its descriptor and its end of
/// the connection are gone, the lock is free for the successor started again. It would catch a
/// successor started again that waits for the old instance's whole drain.
#[tokio::test]
async fn the_old_unlocks_when_its_successor_dies_after_ready() {
    let rig = Rig::new("handoff-lock-dies");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone()).serve());
    let taken = take_lock(&path, addr, &lock).await;
    let predecessor = taken.ready.expect("the step").send_keeping().await.unwrap();
    within("the end of the offer", serving).await.unwrap();
    assert!(!free(&lock));
    assert_eq!(old.mode(), Mode::ReadOnly);
    // The successor dies.
    drop((predecessor, taken.lock));
    until("the unlock", || free(&lock)).await;
    assert_eq!(old.mode(), Mode::ReadOnly, "and it never writes again");
}

/// Asserts that the successor writes to the cache only after `written`, or after the end of the
/// connection: while the old instance has a write in flight it says nothing, and the successor is
/// read-only; when the write ends it says `written` and the successor is read-write; and with an
/// old instance whose writes never end, the end of the old instance makes the successor
/// read-write. It would catch two processes that write to one cache directory at once.
#[tokio::test]
async fn the_new_writes_the_cache_only_after_written_or_the_end_of_the_connection() {
    let rig = Rig::new("handoff-lock-written");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let new = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    old.set_evicting(true);
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone()).serve());
    let taken = take_lock(&path, addr, &lock).await;
    let predecessor = taken.ready.expect("the step").send_keeping().await.unwrap();
    within("the end of the offer", serving).await.unwrap();
    let following = tokio::spawn(follow(predecessor, taken.lock.unwrap(), new.clone()));
    settle().await;
    assert_eq!(
        new.mode(),
        Mode::ReadOnly,
        "a write of the old is in flight"
    );
    old.set_evicting(false);
    until("the write role", || new.mode() == Mode::ReadWrite).await;
    assert!(!following.is_finished(), "the old has not ended");
    drop(old);
    following.abort();

    // An old instance whose writes never end, which then ends.
    let rig = Rig::new("handoff-lock-ended");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let new = rig.locked();
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    old.set_evicting(true);
    let (at, copy) = (path.clone(), listener.try_clone().unwrap());
    let elsewhere = Elsewhere::serve(move || cache_offer(&at, &copy, old));
    let taken = take_lock(&path, addr, &lock).await;
    let predecessor = taken.ready.expect("the step").send_keeping().await.unwrap();
    let following = tokio::spawn(follow(predecessor, taken.lock.unwrap(), new.clone()));
    settle().await;
    assert_eq!(new.mode(), Mode::ReadOnly);
    elsewhere.exit();
    within("the take", following).await.unwrap();
    assert_eq!(new.mode(), Mode::ReadWrite);
}

/// Asserts that a `ready` is a hand-over for good even when `bye` cannot reach the taker (it went
/// away at once): the instance gives the mount up, stops writing, and ends its offer; it does not
/// resume. It would catch an old instance that reads the device again after the successor, which
/// holds it in its store, may have started reading it.
#[tokio::test]
async fn a_ready_is_a_hand_over_for_good_even_when_bye_cannot_be_sent() {
    let rig = Rig::new("handoff-ready-gone");
    let old = rig.locked();
    let (dir, path) = scratch();
    let (mount, steps, _kernel) = fake_mount(dir.path());
    let (listener, _) = listening();
    let serving = tokio::spawn(
        cache_offer(&path, &listener, old.clone())
            .with_mount(mount)
            .serve(),
    );
    let taker = connect(&path).await;
    let offered = ask(&taker, &["take 1", "ready", "mount"]).await;
    let done = ask(&taker, &["ack"]).await;
    assert_eq!(texts(&done.lines), ["done"]);
    send(&taker, &["ready"]).await;
    drop((taker, offered));
    within("the end of the offer", serving).await.unwrap();
    assert_eq!(told(&steps), ["quiesce", "give up"]);
    assert_eq!(old.mode(), Mode::ReadOnly);
}

/// Asserts that a taker that connected before the first one's `ack` and asks while the first is
/// being handed over (its `ready` read, the mount being given up) is refused: it is never offered
/// the mount given up, and the mount is quiesced once. It would catch an old instance that offers
/// a mount it is giving up to a second successor.
#[tokio::test]
async fn a_taker_that_asks_while_the_first_is_handed_over_is_refused() {
    let (dir, path) = scratch();
    let (mut mount, steps, _kernel) = fake_mount(dir.path());
    let (entered, in_give_up) = mpsc::channel::<()>();
    let (release, released) = mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let gave = std::sync::Arc::clone(&steps);
    mount.give_up = std::sync::Arc::new(move || {
        entered.send(()).unwrap();
        released.lock().unwrap().recv().unwrap();
        gave.lock().unwrap().push("give up");
    });
    let (listener, _) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &listener)])
            .unwrap()
            .with_ready_wait(WAIT)
            .with_mount(mount)
            .serve(),
    );
    let first = connect(&path).await;
    let second = connect(&path).await;
    let request = ["take 1", "ready", "mount"];
    let offered = ask(&first, &request).await;
    assert_eq!(texts(&ask(&first, &["ack"]).await.lines), ["done"]);
    send(&first, &["ready"]).await;
    within(
        "the give up",
        tokio::task::spawn_blocking(move || in_give_up.recv()),
    )
    .await
    .unwrap()
    .unwrap();
    let refused = ask(&second, &request).await;
    assert_eq!(
        texts(&refused.lines),
        ["refused", "another taker is taking the sockets"]
    );
    release.send(()).unwrap();
    assert_eq!(texts(&wire::recv(&first).await.unwrap().lines), ["bye"]);
    within("the end of the offer", serving).await.unwrap();
    assert_eq!(told(&steps), ["quiesce", "give up"]);
    drop(offered);
}

/// Asserts that an offer whose sockets and mount need more descriptors than one message carries
/// is refused by name, before anything is handed: the mount is not quiesced. It would catch an
/// offer whose last descriptors the kernel drops while the lines still name them.
#[tokio::test]
async fn an_offer_of_more_descriptors_than_a_message_carries_is_refused() {
    let (dir, path) = scratch();
    let (mount, steps, _kernel) = fake_mount(dir.path());
    let listeners: Vec<TcpListener> = (0..wire::MAX_FDS - 1).map(|_| listening().0).collect();
    let sockets: Vec<Socket> = listeners.iter().map(|l| socket(Front::S3, l)).collect();
    let serving = tokio::spawn(
        Offer::bind(&path, sockets)
            .unwrap()
            .with_ready_wait(WAIT)
            .with_mount(mount)
            .serve(),
    );
    let refused = ask(&connect(&path).await, &["take 1", "ready", "mount"]).await;
    assert_eq!(
        texts(&refused.lines),
        ["refused", "more descriptors than a message can carry"]
    );
    assert!(refused.fds.is_empty());
    assert!(told(&steps).is_empty());
    serving.abort();
}

/// How a taker of the previous release reads an offer (`server/handoff/take.rs` as released
/// before the one hand-over): a descriptor for each line; each line `<front> <address>`, split at
/// its first space; a line of a front it does not know (`s3`, `oci` and `nfs` are those it knows)
/// is skipped and its descriptor closed. What it takes: the front and the socket of each line it
/// knows.
fn previous_release_reads(frame: Frame) -> Vec<(String, TcpListener)> {
    let rest = &frame.lines[1..];
    assert_eq!(rest.len(), frame.fds.len(), "a descriptor for each line");
    let mut taken = Vec::new();
    for (line, fd) in rest.iter().zip(frame.fds) {
        let (name, addr) = line.split_once(' ').expect("<front> <address>");
        if !["s3", "oci", "nfs"].contains(&name) {
            drop(fd);
            continue;
        }
        let _: SocketAddr = addr.parse().expect("an address");
        taken.push((name.to_owned(), TcpListener::from(fd)));
    }
    taken
}

/// Asserts (B8) a taker of the previous release against this release's offer, replaying its
/// frames: it gets the S3 socket alone, skipping the `lock` line of an instance with a cache, and
/// the `fuse`, `control` and `lock` lines of one that holds the mount, each of whose descriptors
/// it closes; its hand-over completes (`done`, then `bye` to its `ready`), and the lock it did not
/// take is free once it has gone, for it to take at its tick as before. It would catch an offer
/// that a release already running cannot read.
#[tokio::test]
async fn a_previous_release_taker_skips_the_lock_fuse_and_control_lines_and_gets_the_sockets() {
    let rig = Rig::new("handoff-b8-taker");
    let lock = rig.config.path.join(".lock");
    let old = rig.locked();
    let (dir, path) = scratch();
    let (listener, addr) = listening();
    let serving = tokio::spawn(cache_offer(&path, &listener, old.clone()).serve());
    let replay = frames(PREVIOUS_TAKER);
    let taker = connect(&path).await;
    let mut answers = Vec::new();
    for frame in &replay {
        wire::send(&taker, frame, &[]).await.unwrap();
        answers.push(wire::recv(&taker).await.unwrap());
    }
    let mut answers = answers.into_iter();
    let offer = answers.next().unwrap();
    assert_eq!(
        offer.lines.last().unwrap(),
        &format!("lock {}", lock.display())
    );
    let taken = previous_release_reads(offer);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].0, "s3");
    assert_eq!(taken[0].1.local_addr().unwrap(), addr);
    assert_eq!(texts(&answers.next().unwrap().lines), ["done"]);
    assert_eq!(texts(&answers.next().unwrap().lines), ["bye"]);
    within("the end of the offer", serving).await.unwrap();
    drop(taker);
    until("the lock free for the taker's tick", || free(&lock)).await;

    // The offer of an instance that holds the mount, as a taker of this release asks for it.
    let (mount, _steps, _kernel) = fake_mount(dir.path());
    let other = dir.path().join("other.sock");
    let rig = Rig::new("handoff-b8-taker-mount");
    let serving = tokio::spawn(
        cache_offer(&other, &listener, rig.locked())
            .with_mount(mount)
            .serve(),
    );
    let offer = ask(&connect(&other).await, &["take 1", "ready", "mount"]).await;
    let names: Vec<&str> = offer.lines[1..]
        .iter()
        .map(|line| line.split_once(' ').unwrap().0)
        .collect();
    assert_eq!(names, ["s3", "fuse", "control", "lock"]);
    let taken = previous_release_reads(offer);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].1.local_addr().unwrap(), addr);
    serving.abort();
}

/// Asserts (B8) this release's taker against an instance of the previous release, replaying that
/// release's frames: it asks for the mount, is refused as not understood, asks again with the
/// `ready` step and is refused again, asks with `take 1` alone, and is handed the S3 socket with
/// no mount, no lock and no `ready` step. It would catch a start of this release that cannot take
/// over from the release before it.
#[tokio::test]
async fn a_taker_against_a_previous_release_offer_is_refused_the_mount_and_asks_again_without_it() {
    let (dir, path) = scratch();
    let (listener, addr) = listening();
    let replay = frames(PREVIOUS_OFFER);
    let (refusal, offer, done) = (replay[0].clone(), replay[1].clone(), replay[2].clone());
    assert_eq!(refusal, ["refused", NOT_UNDERSTOOD]);
    let offer: Vec<String> = offer
        .iter()
        .map(|line| line.replace("{s3}", &addr.to_string()))
        .collect();
    let bound = UnixListener::bind(&path).unwrap();
    let previous = async {
        let mut requests = Vec::new();
        loop {
            let (stream, _) = bound.accept().await.unwrap();
            let request = wire::recv(&stream).await.unwrap().lines;
            requests.push(request.clone());
            if request != ["take 1"] {
                wire::send(&stream, &refusal, &[]).await.unwrap();
                continue;
            }
            wire::send(&stream, &offer, &[listener.as_fd()])
                .await
                .unwrap();
            assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ack"]);
            wire::send(&stream, &done, &[]).await.unwrap();
            return requests;
        }
    };
    let (mountpoint, control) = (dir.path().join("mnt"), dir.path().join("fs-control.sock"));
    let lock: PathBuf = dir.path().join("cache.lock");
    let addrs = [addr];
    let want = Want {
        s3: &addrs,
        oci: &[],
        nfs: &[],
    };
    let ask = Ask {
        mount: Some(super::super::mount::MountWant {
            mountpoint: &mountpoint,
            control: &control,
            wait: WAIT,
            device: |_| Ok(()),
            prepare: Box::new(|_| panic!("no mount is offered")),
        }),
        lock: Some(&lock),
    };
    let handoff = handoff(path.clone());
    let (taken, requests) = tokio::join!(take_service(&handoff, &want, ask), previous);
    assert_eq!(
        requests,
        [
            vec!["take 1", "ready", "mount"],
            vec!["take 1", "ready"],
            vec!["take 1"]
        ]
    );
    let taken = taken.unwrap().unwrap();
    assert_eq!(taken.s3_addrs, [addr]);
    assert!(taken.mount.is_none());
    assert!(taken.lock.is_none());
    assert!(taken.ready.is_none());
}

/// Asserts that a request this release does not know, in any of its lines, is refused as not
/// understood, by an instance that holds the mount and takes the `ready` step: another version,
/// another second or third line, a line more. It would catch an instance that takes a request of
/// a later release for one of its own.
#[tokio::test]
async fn a_request_with_a_line_this_release_does_not_know_is_refused() {
    let (dir, path) = scratch();
    let (mount, steps, _kernel) = fake_mount(dir.path());
    let (listener, _) = listening();
    let serving = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::S3, &listener)])
            .unwrap()
            .with_ready_wait(WAIT)
            .with_mount(mount)
            .serve(),
    );
    for request in [
        &["take 2", "ready"][..],
        &["take 1", "later"],
        &["take 2", "ready", "mount"],
        &["take 1", "later", "mount"],
        &["take 1", "ready", "later"],
        &["take 1", "ready", "mount", "later"],
    ] {
        let refused = ask(&connect(&path).await, request).await;
        assert_eq!(
            texts(&refused.lines),
            ["refused", NOT_UNDERSTOOD],
            "{request:?}"
        );
    }
    assert!(told(&steps).is_empty());
    serving.abort();
}
