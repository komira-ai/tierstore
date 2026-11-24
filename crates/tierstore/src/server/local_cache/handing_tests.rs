//! The cache's side of the hand-over of its lock: the offer of the descriptor, the end of the
//! writes at `ready`, the wait for the writes in flight, the unlock when the successor is gone,
//! and the successor's take of the write role.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::super::super::testing::{Rig, age, tree, wait_until};
use super::super::super::{Mode, Tick};
use crate::Backend as _;
use crate::server::lock::{Received, try_take};

/// What starts or ends a write in flight, for a test.
type Switch<'a> = Box<dyn Fn() + 'a>;

/// The lock file of `rig`'s directory.
fn lock_of(rig: &Rig) -> std::path::PathBuf {
    rig.config.path.join(".lock")
}

/// Asserts that a cache that holds the lock offers a copy of its descriptor and the path, which
/// holds the same lock; that one that waits for the lock, or takes none, offers nothing; and that
/// one whose own predecessor still holds a descriptor offers nothing until it is gone. It would
/// catch an offer of a lock the instance does not hold, and a lock handed on while the process it
/// came from could still unlock it.
#[tokio::test]
async fn only_a_cache_that_holds_the_lock_alone_offers_it() {
    let rig = Rig::new("tscache-hand-offer");
    assert!(rig.cache.lock_to_hand().unwrap().is_none(), "takes none");
    let holder = rig.locked();
    let waiter = rig.locked();
    assert!(waiter.lock_to_hand().unwrap().is_none(), "waits for it");
    let (path, fd) = holder.lock_to_hand().unwrap().unwrap();
    assert_eq!(path, lock_of(&rig));
    let received = Received::of(fd, &path).unwrap();
    holder.hand_writes();
    drop(holder);
    // The old holder is gone and closed its descriptor without an unlock: the copy holds it.
    assert!(try_take(&path).unwrap().is_none());
    assert_eq!(waiter.take_handed(received).unwrap(), Tick::Took);
    assert!(
        waiter.lock_to_hand().unwrap().is_none(),
        "the predecessor may hold it"
    );
    waiter.predecessor_gone();
    assert!(waiter.lock_to_hand().unwrap().is_some());
}

/// Asserts that at `ready` the cache starts no write: it is read-only, its mode's watchers are
/// told, a miss read through it fills nothing and changes nothing in the directory, and it keeps
/// the lock (a third cache over the directory is read-only) and never takes it again at a tick,
/// even once it is free. It would catch an old instance that goes on filling while its successor
/// holds the lock, or that takes the lock back.
#[tokio::test]
async fn the_old_starts_no_cache_write_after_ready() {
    let rig = Rig::new("tscache-hand-ready");
    let old = rig.locked();
    let mut modes = old.watch_mode();
    assert_eq!(*modes.borrow_and_update(), Mode::ReadWrite);
    rig.put("cas/a", b"a miss").await;
    old.hand_writes();
    assert_eq!(old.mode(), Mode::ReadOnly);
    assert!(modes.has_changed().unwrap());
    assert_eq!(*modes.borrow_and_update(), Mode::ReadOnly);
    age(&rig.config.path);
    let before = tree(&rig.config.path);
    assert_eq!(old.get("cas/a", None).await.unwrap().as_ref(), b"a miss");
    old.writes_ended().await;
    assert_eq!(tree(&rig.config.path), before, "the miss wrote nothing");
    assert_eq!(
        rig.locked().mode(),
        Mode::ReadOnly,
        "the lock is still held"
    );
    assert_eq!(old.tick().unwrap(), Tick::Waiting);
    old.unlock_handed().unwrap();
    assert_eq!(
        old.tick().unwrap(),
        Tick::Waiting,
        "it never takes it again"
    );
    assert_eq!(old.mode(), Mode::ReadOnly);
    assert_eq!(rig.locked().mode(), Mode::ReadWrite, "free for the next");
}

/// Asserts that the wait for the writes in flight returns only once each kind of write has ended:
/// a fill held at the upstream, a pass of the eviction, a put, a keep and a fill of a range, each
/// on its own. It would catch a `written` said while the old instance still writes to the
/// directory its successor is about to scan.
#[tokio::test(start_paused = true)]
async fn the_writes_in_flight_end_before_the_wait_does() {
    let rig = Rig::new("tscache-hand-writes");
    let old = rig.locked();
    rig.put("cas/held", b"held at the upstream").await;
    let gate = rig.tamper.hold_key("cas/held");
    let reading = tokio::spawn({
        let old = old.clone();
        async move { old.get("cas/held", None).await }
    });
    let inner = Arc::clone(&old.inner);
    wait_until("the fill is in the table", || {
        !super::super::super::locked(&inner.flights).is_empty()
    })
    .await;
    old.hand_writes();
    let waiting = tokio::spawn({
        let old = old.clone();
        async move { old.writes_ended().await }
    });
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished(), "a fill is in flight");
    // The fill gives up at its next look; the read then goes to the upstream again.
    gate.add_permits(2);
    assert_eq!(
        reading.await.unwrap().unwrap().as_ref(),
        b"held at the upstream"
    );
    waiting.await.unwrap();

    // The other writes, one at a time.
    let writes: [(&str, Switch<'_>, Switch<'_>); 4] = [
        (
            "an eviction pass",
            Box::new(|| inner.evicting.store(true, Ordering::SeqCst)),
            Box::new(|| inner.evicting.store(false, Ordering::SeqCst)),
        ),
        (
            "a put",
            Box::new(|| {
                let mut writers = super::super::super::locked(&inner.writers);
                let _ = writers.insert("cas/put".to_owned(), Default::default());
            }),
            Box::new(|| {
                let _ = super::super::super::locked(&inner.writers).remove("cas/put");
            }),
        ),
        (
            "a keep",
            Box::new(|| {
                let _ = inner.keeps.forget_permits(1);
            }),
            Box::new(|| inner.keeps.add_permits(1)),
        ),
        (
            "a fill of a range",
            Box::new(|| inner.extents.set_test_flight(true)),
            Box::new(|| inner.extents.set_test_flight(false)),
        ),
    ];
    for (what, begin, end) in writes {
        begin();
        assert!(inner.writing(), "{what} is a write");
        let waiting = tokio::spawn({
            let old = old.clone();
            async move { old.writes_ended().await }
        });
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        assert!(!waiting.is_finished(), "{what} in flight");
        end();
        waiting.await.unwrap();
    }
}

/// Asserts the successor's take of the write role with a received lock: a cache that waits for
/// the lock takes it, is read-write and holds it (dropped, it frees it); one that holds the lock
/// already, or takes none, only closes the descriptor; and one whose lock file was replaced
/// refuses it and stays read-only. It would catch a successor that writes beside its predecessor,
/// or that trusts a lock of a file nothing looks at.
#[tokio::test]
async fn a_waiting_cache_takes_the_write_role_with_the_lock_it_was_handed() {
    let rig = Rig::new("tscache-hand-take");
    let old = rig.locked();
    let new = rig.locked();
    assert_eq!(new.mode(), Mode::ReadOnly);
    let (path, fd) = old.lock_to_hand().unwrap().unwrap();
    let received = Received::of(fd, &path).unwrap();
    old.hand_writes();
    assert_eq!(new.take_handed(received).unwrap(), Tick::Took);
    assert_eq!(new.mode(), Mode::ReadWrite);
    assert_eq!(new.tick().unwrap(), Tick::Held);
    // A second descriptor, to a cache that holds the lock already: only closed.
    let again = Received::of(std::fs::File::open(&path).unwrap().into(), &path).unwrap();
    assert_eq!(new.take_handed(again).unwrap(), Tick::Held);
    let lockless = Received::of(std::fs::File::open(&path).unwrap().into(), &path).unwrap();
    assert_eq!(rig.cache.take_handed(lockless).unwrap(), Tick::Held);
    drop(old);
    assert!(try_take(&path).unwrap().is_none(), "the new one holds it");
    drop(new);
    assert!(
        try_take(&path).unwrap().is_some(),
        "and frees it when it goes"
    );

    // The lock file replaced between the check and the take.
    let rig = Rig::new("tscache-hand-replaced");
    let old = rig.locked();
    let new = rig.locked();
    let (path, fd) = old.lock_to_hand().unwrap().unwrap();
    let received = Received::of(fd, &path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"").unwrap();
    let refused = new.take_handed(received).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the lock file was replaced since it was handed over"
    );
    assert_eq!(new.mode(), Mode::ReadOnly);
}

/// Asserts that a cache that handed nothing over unlocks nothing: a holder that is asked to
/// unlock a handed lock keeps its own, and a cache handed at `ready` that held no lock (it waited
/// for it, lost it, or takes none) is read-only and holds nothing, and never takes the lock once it
/// is free. It would catch an unlock of a lock the instance still writes under, and an old instance
/// that writes again beside its successor.
#[tokio::test]
async fn only_a_handed_lock_is_unlocked() {
    let rig = Rig::new("tscache-hand-unlock");
    let holder = rig.locked();
    holder.unlock_handed().unwrap();
    assert_eq!(rig.locked().mode(), Mode::ReadOnly, "still the holder's");
    let waiter = rig.locked();
    waiter.hand_writes();
    assert_eq!(waiter.mode(), Mode::ReadOnly);
    assert_eq!(holder.tick().unwrap(), Tick::Held);
    // A cache that lost the lock before `ready` does not take it back once it is free.
    waiter.hand_writes();
    waiter.unlock_handed().unwrap();
    drop(holder);
    assert_eq!(waiter.tick().unwrap(), Tick::Waiting);
    assert_eq!(waiter.mode(), Mode::ReadOnly);
    assert_eq!(rig.cache.mode(), Mode::ReadWrite);
    rig.cache.hand_writes();
    assert_eq!(
        rig.cache.tick().unwrap(),
        Tick::Waiting,
        "a lockless one too"
    );
}
