//! A build node's hand-over to its next image: the cache's writes in flight end before the offer
//! is made, the next image takes the write role with the handed lock before it writes, and a
//! whole hand-over of a node in this process, from the drain of the old image to the next image
//! serving on its sockets (the exec between them is the one step not made: a test cannot replace
//! its own image).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;

use super::{NextImage, exec_next, stop_or_next_on, take_write_role};
use crate::server::cache_cli::{Imaging, OnSystem, serve_cache_on};
use objstore_s3::{ByteRange, ObjectKey, ObjectStore as _};

use crate::server::cache_cli_tests::{Node, client, probe};
use crate::server::handoff::testing::{listening, settle, socket, within};
use crate::server::handoff::{Front, Image, Own, Want, read_handed};
use crate::server::local_cache::testing::{Rig, wait_until};
use crate::server::local_cache::{Disk, Mode, SystemDisk};
use crate::server::lock::{Received, try_take};
use crate::{Backend, MemoryBackend};

/// The image a hand-over gives, as `next` makes it with a program of no consequence.
async fn image_of(next: &NextImage) -> Option<Image> {
    let program = || Ok(PathBuf::from("tierstore"));
    next.image(program, ["tierstore".into()]).await.unwrap()
}

/// Asserts that the hand-over's step of the cache waits for a fill in flight to end before it
/// returns, after which the offer is made and the exec follows: until the fill has ended there is
/// no offer, and once it has, its request is answered and nothing of it is left in the stage; that
/// the step starts no write from the moment it begins (a miss read meanwhile is served and not
/// written); and that the offer then holds the listening socket and the cache's lock, which stays
/// locked once the cache is gone.
#[tokio::test]
async fn the_cache_writes_in_flight_end_before_the_exec() {
    let rig = Rig::new("next-image-writes");
    let cache = rig.locked();
    assert_eq!(cache.mode(), Mode::ReadWrite);
    rig.put("cas/held", b"held bytes").await;
    rig.put("cas/later", b"later bytes").await;
    let gate = rig.tamper.hold_key_after_read("cas/held");
    let reading = cache.clone();
    let fill = tokio::spawn(async move { reading.get("cas/held", None).await });
    wait_until("the fill is in flight", || rig.tamper.parked() == 1).await;

    let next = Arc::new(NextImage::default());
    next.ask();
    let (listener, addr) = listening();
    let (handing, on) = (Arc::clone(&next), cache.clone());
    let sockets = vec![socket(Front::S3, &listener)];
    let hand = tokio::spawn(async move { handing.hand(sockets, &on).await });
    settle().await;
    assert!(!hand.is_finished(), "the hand-over waits for the fill");
    assert!(
        image_of(&next).await.is_none(),
        "no offer before the writes have ended"
    );
    assert_eq!(cache.mode(), Mode::ReadOnly, "and no write starts");
    let later = cache.get("cas/later", None).await.unwrap();
    assert_eq!(later.as_ref(), b"later bytes");
    assert!(!rig.has_file("cas/later"), "a miss is served, not written");

    gate.add_permits(16);
    let held = within("the fill", fill).await.unwrap().unwrap();
    assert_eq!(
        held.as_ref(),
        b"held bytes",
        "the request of the fill is answered"
    );
    within("the hand-over", hand).await.unwrap().unwrap();
    // It stopped at its next look, as on a lost lock: nothing of it is left half written.
    assert_eq!(rig.staged_files(), 0, "the fill in flight has ended");
    let image = image_of(&next).await.unwrap();
    drop(cache);
    let lock = rig.config.path.join(".lock");
    assert!(
        try_take(&lock).unwrap().is_none(),
        "the offer holds the lock"
    );
    let want = Want {
        s3: &[addr],
        oci: &[],
        nfs: &[],
    };
    let own = Own {
        control: None,
        lock: Some(&lock),
    };
    let handed = read_handed(image.into_handed(), &want, &own).await.unwrap();
    assert_eq!(handed.inherited.s3_addrs, [addr]);
    assert!(handed.inherited.lock.is_some());
}

/// Asserts that a stop that was not asked to hand over gives no image, and that no image is no
/// exec.
#[tokio::test]
async fn a_stop_that_is_not_a_hand_over_gives_no_image() {
    let rig = Rig::new("next-image-stop");
    let cache = rig.locked();
    let next = NextImage::default();
    assert!(!next.asked());
    assert!(image_of(&next).await.is_none());
    assert!(exec_next(None).is_ok());
    assert_eq!(cache.mode(), Mode::ReadWrite);
}

/// Asserts that the next image's cache, opened while the offer holds the lock, is read-only and
/// writes nothing (a miss is served and not written, the stage is left alone), and that with the
/// handed lock it takes the write role at once, without waiting for a tick: the stage is emptied
/// and the next miss is written; and that it may offer the lock in its turn, as no process is left
/// to hand it back.
#[tokio::test]
async fn the_new_image_takes_the_write_role_before_its_first_cache_write() {
    let rig = Rig::new("next-image-role");
    rig.put("cas/before", b"before bytes").await;
    rig.put("cas/after", b"after bytes").await;
    let old = rig.locked();
    let (path, fd) = old.lock_to_hand().unwrap().unwrap();
    old.hand_writes();
    // The exec closes the old image's descriptors: the offer's holds the lock.
    drop(old);
    let new = rig.locked();
    assert_eq!(new.mode(), Mode::ReadOnly);
    std::fs::write(rig.config.path.join(".stage").join("left"), b"x").unwrap();
    let before = new.get("cas/before", None).await.unwrap();
    assert_eq!(before.as_ref(), b"before bytes");
    assert!(
        !rig.has_file("cas/before"),
        "no write before the write role"
    );
    assert_eq!(rig.staged_files(), 1);

    let received = Received::of(fd, &path).unwrap();
    within("the write role", take_write_role(new.clone(), received)).await;
    assert_eq!(new.mode(), Mode::ReadWrite);
    assert_eq!(rig.staged_files(), 0, "the stage was emptied first");
    let after = new.get("cas/after", None).await.unwrap();
    assert_eq!(after.as_ref(), b"after bytes");
    assert!(
        rig.has_file("cas/after"),
        "the first write comes after the take"
    );
    assert!(
        new.lock_to_hand().unwrap().is_some(),
        "it may hand the lock on"
    );
}

/// Asserts that a lock file replaced since it was handed is not taken: the image stays read-only,
/// and the descriptor is closed, so the lock at the path is free for the next tick.
#[tokio::test]
async fn a_handed_lock_whose_file_was_replaced_is_left_to_the_tick() {
    let rig = Rig::new("next-image-replaced");
    let old = rig.locked();
    let (path, fd) = old.lock_to_hand().unwrap().unwrap();
    old.hand_writes();
    drop(old);
    let new = rig.locked();
    let received = Received::of(fd, &path).unwrap();
    std::fs::remove_file(&path).unwrap();
    within("the write role", take_write_role(new.clone(), received)).await;
    assert_eq!(new.mode(), Mode::ReadOnly);
    assert!(try_take(&path).unwrap().is_some());
}

/// Sends a GET of `/readyz` on `stream` and gives the status line of the answer.
async fn readyz_on(stream: &mut tokio::net::TcpStream) -> String {
    let ask = b"GET /readyz HTTP/1.1\r\nhost: tier.test\r\nconnection: close\r\n\r\n";
    stream.write_all(ask).await.unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer).await;
    answer.lines().next().unwrap_or_default().to_owned()
}

/// Yields until `/readyz` at `addr` says `ready read-write`, in a bounded number of
/// probes, none of them timed.
async fn read_write_at(addr: SocketAddr) {
    for _ in 0..10_000 {
        if probe(addr, "/readyz").await == (200, "ready read-write".to_owned()) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "{addr} never said it is ready and read-write: {:?}",
        probe(addr, "/readyz").await
    );
}

/// Asserts a build node's whole hand-over to its next image, in this process: the old image, told
/// that its stop is a hand-over, drains and gives the offer; a client that connects meanwhile is
/// queued on the socket the offer holds, not refused; the next image, started from the offer,
/// serves on the same address (the queued client is answered by it), takes the write role with the
/// handed lock (it says `ready`, read-write) and fills its cache.
#[tokio::test]
async fn a_build_node_hands_its_sockets_and_lock_to_the_next_image() {
    let node = Node::new("next-image-node");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    memory
        .put("cas/one", Bytes::from_static(b"one"))
        .await
        .unwrap();
    let upstream: Arc<dyn Backend> = memory;
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);

    let next = Arc::new(NextImage::default());
    let (stop, stopped) = oneshot::channel::<()>();
    let asking = Arc::clone(&next);
    let stopping = async move {
        let _ = stopped.await;
        asking.ask();
    };
    let (told, bound) = oneshot::channel();
    let imaging = Imaging {
        next: Some(&next),
        handed: None,
    };
    let ways = (Arc::clone(&upstream), Arc::clone(&disk));
    let old = serve_cache_on(
        &settings,
        ways,
        OnSystem::default(),
        stopping,
        |at| {
            let _ = told.send(at.s3);
        },
        imaging,
    );
    let driving = async {
        let addr = bound.await.unwrap();
        read_write_at(addr).await;
        let _ = stop.send(());
        addr
    };
    let (drained, addr) = tokio::join!(old, driving);
    drained.unwrap();
    let image = image_of(&next).await.unwrap();
    let mut queued = tokio::net::TcpStream::connect(addr).await.unwrap();

    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel();
    let imaging = Imaging {
        next: None,
        handed: Some(image.into_handed()),
    };
    let ways = (upstream, disk);
    let stopping = async move {
        let _ = stopped.await;
    };
    let new = serve_cache_on(
        &settings,
        ways,
        OnSystem::default(),
        stopping,
        |at| {
            let _ = told.send(at.s3);
        },
        imaging,
    );
    let driving = async {
        let again = bound.await.unwrap();
        assert_eq!(again, addr, "the next image serves on the same address");
        assert_eq!(readyz_on(&mut queued).await, "HTTP/1.1 200 OK");
        read_write_at(addr).await;
        let key = ObjectKey::new("cas/one").unwrap();
        let all = ByteRange::new(0, 3).unwrap();
        let got = client(addr).get_range(&key, all).await.unwrap();
        assert_eq!(got.as_ref(), b"one");
        let object = node.cache_dir().join("obj");
        let mut filled = false;
        for _ in 0..10_000 {
            filled = walk_has_file(&object);
            if filled {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(filled, "the next image wrote the fill");
        let _ = stop.send(());
    };
    let (drained, ()) = tokio::join!(new, driving);
    drained.unwrap();
}

/// Whether there is a file anywhere under `dir`.
fn walk_has_file(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let path = entry.path();
            path.is_file() || (path.is_dir() && walk_has_file(&path))
        })
    })
}

/// Asserts that the signal that asks for the hand-over (SIGUSR2 on a Mac; raised here at this
/// process, with the handler of this test) ends the serving as a stop does and tells the next
/// image it is asked, which a stop does not.
#[tokio::test]
async fn sigusr2_is_a_stop_that_asks_for_the_next_image() {
    let next = Arc::new(NextImage::default());
    let usr2 = tokio::signal::unix::SignalKind::user_defined2();
    let stop = stop_or_next_on(Arc::clone(&next), Some(usr2)).unwrap();
    assert!(!next.asked());
    let me = rustix::process::getpid();
    rustix::process::kill_process(me, rustix::process::Signal::USR2).unwrap();
    within("the stop", stop).await;
    assert!(next.asked());
}

/// Asserts that a node given a next image whose stop was not asked to hand over drains as a stop
/// does and leaves nothing to hand over.
#[tokio::test]
async fn a_node_whose_stop_is_not_a_hand_over_leaves_no_image() {
    let node = Node::new("next-image-plain-stop");
    let settings = node.settings("", "");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let next = NextImage::default();
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel();
    let imaging = Imaging {
        next: Some(&next),
        handed: None,
    };
    let stopping = async move {
        let _ = stopped.await;
    };
    let served = serve_cache_on(
        &settings,
        (upstream, disk),
        OnSystem::default(),
        stopping,
        |at| {
            let _ = told.send(at.s3);
        },
        imaging,
    );
    let driving = async {
        let addr = bound.await.unwrap();
        read_write_at(addr).await;
        let _ = stop.send(());
        addr
    };
    let (drained, _) = tokio::join!(served, driving);
    drained.unwrap();
    assert!(!next.asked());
    assert!(image_of(&next).await.is_none());
}
