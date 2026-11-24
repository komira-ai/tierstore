//! The cache lock after `ready`: the running instance's `written` and its unlock when the
//! successor is gone, and the successor's wait for `written` and its take of the write role.

use std::os::unix::net::UnixStream as StdStream;

use tokio::net::UnixStream;

use super::super::testing::within;
use super::super::wire;
use super::{Predecessor, Wrote, after_ready, follow};
use crate::server::local_cache::testing::Rig;
use crate::server::local_cache::{LocalCache, Mode};
use crate::server::lock::{Received, try_take};

/// A connected pair of the conversation's streams: the running instance's end, the successor's.
fn pair() -> (UnixStream, UnixStream) {
    let (a, b) = StdStream::pair().unwrap();
    for end in [&a, &b] {
        end.set_nonblocking(true).unwrap();
    }
    (
        UnixStream::from_std(a).unwrap(),
        UnixStream::from_std(b).unwrap(),
    )
}

/// The old instance's cache, handed at `ready`, and the descriptor its successor received.
fn handed(rig: &Rig) -> (LocalCache, Received) {
    let old = rig.locked();
    let (path, fd) = old.lock_to_hand().unwrap().unwrap();
    let received = Received::of(fd, &path).unwrap();
    old.hand_writes();
    (old, received)
}

/// Asserts the successor's wait: frames that are not `written` are skipped, `written` ends it,
/// and a connection that ends instead ends it too, as the end of the instance; the wait for the
/// end returns when the instance closes. It would catch a successor that takes the write role at
/// a stray frame, or that never takes it when the instance crashed.
#[tokio::test]
async fn the_successor_waits_for_written_or_the_end() {
    let (old, new) = pair();
    let predecessor = std::sync::Arc::new(Predecessor(new));
    let waiting = tokio::spawn({
        let predecessor = std::sync::Arc::clone(&predecessor);
        async move { predecessor.written().await }
    });
    wire::send(&old, &["something else".to_owned()], &[])
        .await
        .unwrap();
    // The stray frame is read and skipped before the next is sent: one frame to a read, as the
    // conversation has it.
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished(), "a stray frame is not written");
    wire::send(&old, &["written".to_owned()], &[])
        .await
        .unwrap();
    assert_eq!(within("written", waiting).await.unwrap(), Wrote::Written);
    drop(old);
    within("the end", predecessor.ended()).await;
    let (old, new) = pair();
    drop(old);
    assert_eq!(
        within("the end", Predecessor(new).written()).await,
        Wrote::Ended
    );
}

/// Asserts the running instance after `bye` when its writes end first: it says `written`, keeps
/// the lock while the successor holds the connection, and unlocks once the successor is gone, so
/// that a third process takes the lock then and not before. It would catch an old instance that
/// never says `written`, or that unlocks under a successor that lives.
#[tokio::test]
async fn written_is_said_and_the_lock_is_unlocked_when_the_successor_is_gone() {
    let rig = Rig::new("handoff-written-said");
    let lock = rig.config.path.join(".lock");
    let (old, received) = handed(&rig);
    let (near, far) = pair();
    let running = tokio::spawn(after_ready(near, old.clone()));
    let frame = within("written", wire::recv(&far)).await.unwrap();
    assert_eq!(frame.lines, ["written"]);
    assert!(try_take(&lock).unwrap().is_none(), "the successor holds it");
    assert!(!running.is_finished());
    // A frame after `written` is no end, and is skipped.
    wire::send(&far, &["something".to_owned()], &[])
        .await
        .unwrap();
    crate::server::handoff::testing::settle().await;
    assert!(!running.is_finished());
    // The successor dies: its descriptor of the lock and its end of the connection close.
    drop((far, received));
    within("the unlock", running).await.unwrap();
    assert!(try_take(&lock).unwrap().is_some(), "free for the next");
}

/// Asserts the running instance after `bye` when the successor is gone before the writes end: no
/// `written` is said, the instance waits for its writes in flight, and then unlocks. It would
/// catch an unlock while the old instance still writes, which a successor started again would
/// then scan beside.
#[tokio::test(start_paused = true)]
async fn a_successor_gone_before_written_is_unlocked_for_once_the_writes_end() {
    let rig = Rig::new("handoff-written-gone");
    let lock = rig.config.path.join(".lock");
    let (old, received) = handed(&rig);
    let (near, far) = pair();
    old.set_evicting(true);
    let running = tokio::spawn(after_ready(near, old.clone()));
    drop((far, received));
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(!running.is_finished(), "a write is in flight");
    assert!(try_take(&lock).unwrap().is_none());
    old.set_evicting(false);
    within("the unlock", running).await.unwrap();
    assert!(try_take(&lock).unwrap().is_some());
}

/// Asserts the successor's side after `bye`: on `written` it takes the write role with the lock
/// it was handed and offers it to no successor of its own until the instance it came from is
/// gone; on the end of the connection it takes the write role at once; and a lock file replaced
/// meanwhile is not taken, so it stays read-only for its tick. It would catch a successor that
/// writes before its predecessor stopped, or that hands on a lock its predecessor may unlock.
#[tokio::test]
async fn the_successor_takes_the_write_role_on_written_or_the_end() {
    let rig = Rig::new("handoff-follow-written");
    let (old, received) = handed(&rig);
    let new = rig.locked();
    assert_eq!(new.mode(), Mode::ReadOnly);
    let (near, far) = pair();
    let following = tokio::spawn(follow(Predecessor(far), received, new.clone()));
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert_eq!(new.mode(), Mode::ReadOnly, "nothing said yet");
    wire::send(&near, &["written".to_owned()], &[])
        .await
        .unwrap();
    crate::server::local_cache::testing::wait_until("the write role", || {
        new.mode() == Mode::ReadWrite
    })
    .await;
    assert!(
        new.lock_to_hand().unwrap().is_none(),
        "the predecessor lives"
    );
    drop((near, old));
    within("the end of the predecessor", following)
        .await
        .unwrap();
    assert!(new.lock_to_hand().unwrap().is_some());

    let rig = Rig::new("handoff-follow-ended");
    let (old, received) = handed(&rig);
    let new = rig.locked();
    let (near, far) = pair();
    drop((near, old));
    within("the take", follow(Predecessor(far), received, new.clone())).await;
    assert_eq!(new.mode(), Mode::ReadWrite);
    assert!(new.lock_to_hand().unwrap().is_some());

    let rig = Rig::new("handoff-follow-replaced");
    let (_old, received) = handed(&rig);
    let new = rig.locked();
    let lock = rig.config.path.join(".lock");
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&lock, b"").unwrap();
    let (near, far) = pair();
    drop(near);
    within(
        "the refusal",
        follow(Predecessor(far), received, new.clone()),
    )
    .await;
    assert_eq!(new.mode(), Mode::ReadOnly);
}

/// Asserts that a `written` that cannot be sent (the successor shut its reading side) ends the
/// running instance's part without the unlock: the successor may still live, holding its
/// descriptor, and a third process does not take the lock. It would catch an unlock under a
/// successor that then takes the write role believing it holds a lock nobody holds.
#[tokio::test]
async fn a_written_that_cannot_be_sent_leaves_the_lock_locked() {
    let rig = Rig::new("handoff-written-unsent");
    let lock = rig.config.path.join(".lock");
    let (old, received) = handed(&rig);
    let (a, b) = StdStream::pair().unwrap();
    b.shutdown(std::net::Shutdown::Read).unwrap();
    a.set_nonblocking(true).unwrap();
    let near = UnixStream::from_std(a).unwrap();
    within("the end of its part", after_ready(near, old.clone())).await;
    assert!(try_take(&lock).unwrap().is_none(), "kept for the successor");
    drop((b, received, old));
    assert!(try_take(&lock).unwrap().is_some());
}
