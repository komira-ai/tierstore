//! The lease body: two holders of one name, a restarted process and the one it replaced, are not
//! mistaken for each other by the content tag, and the check that a holder still holds the lease.

use std::sync::Arc;

use crate::faulty::{FaultyBackend, Op};
use crate::journal::LEASE_KEY;
use crate::lease::Lease;
use crate::{Backend, Error, MemoryBackend, TestClock};

const TTL: u64 = 1000;

/// A hot tier on a clock the test sets, and two leases of the holder name `p` over it, as a
/// process and its restart are.
fn two_holders() -> (Arc<MemoryBackend>, Arc<TestClock>, Lease, Lease) {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
    let a = Lease::new(hot.clone(), clock.clone(), "p", TTL);
    let b = Lease::new(hot.clone(), clock.clone(), "p", TTL);
    (hot, clock, a, b)
}

async fn tag(hot: &MemoryBackend) -> Option<String> {
    hot.head(LEASE_KEY).await.ok().map(|meta| meta.tag)
}

/// Asserts that a holder whose lease expired and was taken over by a restart with the same name
/// cannot renew the new holder's lease. A takes the lease (its first `acquire` and a renewal
/// write generations 1 and 2) and holds the tag of `p\n2`. B's take over also writes generation
/// 2, because its `acquire` counts a body for the create-only put that fails and the renewal
/// that replaces the object counts the next. Defect it catches: A's body and B's are the same
/// bytes, so the tag is the same, and A's renewal replaces the new holder's object.
#[tokio::test]
async fn a_stale_holder_of_the_same_name_cannot_renew_the_new_holders_lease() {
    let (hot, clock, mut a, mut b) = two_holders();
    assert!(a.ensure().await);
    assert!(a.ensure().await);
    let stale = tag(&hot).await.unwrap();

    clock.set(30_000);
    assert!(b.ensure().await);
    let current = tag(&hot).await.unwrap();
    assert_ne!(current, stale);

    assert!(!a.ensure().await);
    assert_eq!(tag(&hot).await.unwrap(), current);
    assert!(b.ensure().await);
}

/// Asserts that the old holder's release leaves the new holder's lease in place. Defect it
/// catches: the old holder sees a tag equal to its own and deletes the new holder's object. A
/// is at generation 2 when it is taken over, as B is when its take over writes, so the bodies
/// and the tags are equal (see the test above).
#[tokio::test]
async fn a_stale_holder_of_the_same_name_cannot_release_the_new_holders_lease() {
    let (hot, clock, mut a, mut b) = two_holders();
    assert!(a.ensure().await);
    assert!(a.ensure().await);

    clock.set(30_000);
    assert!(b.ensure().await);
    let current = tag(&hot).await.unwrap();

    a.release().await;
    assert_eq!(tag(&hot).await, Some(current));
}

/// Asserts that two leases of one holder name write different bodies for the same count of
/// writes. Defect it catches: a body made of the name and the count alone, which every process
/// of that name reproduces.
#[tokio::test]
async fn two_leases_of_one_name_write_different_first_bodies() {
    let (hot, _clock, mut a, _b) = two_holders();
    assert!(a.ensure().await);
    let first = hot.get(LEASE_KEY, None).await.unwrap();

    let other_clock = Arc::new(TestClock::new());
    let other_hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&other_clock)));
    let mut c = Lease::new(other_hot.clone(), other_clock, "p", TTL);
    assert!(c.ensure().await);
    let second = other_hot.get(LEASE_KEY, None).await.unwrap();

    assert_ne!(first, second);
}

/// Asserts that a holder that has not taken the lease does not hold it.
#[tokio::test]
async fn still_held_is_false_before_the_lease_is_taken() {
    let (hot, _clock, mut a, _b) = two_holders();
    assert!(!a.still_held().await.unwrap());
    assert_eq!(tag(&hot).await, None);
}

/// Asserts that a holder whose object is as it wrote it still holds the lease, and that asking
/// does not renew it (the tag of the object is the same after).
#[tokio::test]
async fn still_held_is_true_for_the_holder_and_does_not_renew() {
    let (hot, _clock, mut a, _b) = two_holders();
    assert!(a.ensure().await);
    let before = tag(&hot).await;
    assert!(a.still_held().await.unwrap());
    assert!(a.still_held().await.unwrap());
    assert_eq!(tag(&hot).await, before);
}

/// Asserts that a holder that was taken over finds it has lost the lease, forgets it (its next
/// `ensure` cannot renew the new holder's object) and leaves the new holder's object alone.
#[tokio::test]
async fn still_held_is_false_and_forgets_after_a_take_over() {
    let (hot, clock, mut a, mut b) = two_holders();
    assert!(a.ensure().await);
    clock.set(30_000);
    assert!(b.ensure().await);
    let current = tag(&hot).await;

    assert!(!a.still_held().await.unwrap());
    assert!(!a.ensure().await);
    assert_eq!(tag(&hot).await, current);
    assert!(b.still_held().await.unwrap());
}

/// Asserts that a holder whose lease object is gone does not hold the lease, and takes it anew.
#[tokio::test]
async fn still_held_is_false_when_the_object_is_gone() {
    let (hot, _clock, mut a, _b) = two_holders();
    assert!(a.ensure().await);
    hot.delete(LEASE_KEY).await.unwrap();
    assert!(!a.still_held().await.unwrap());
    assert!(a.ensure().await);
    assert!(a.still_held().await.unwrap());
}

/// Asserts that a HEAD that fails is given back as an error, and that the holder keeps its tag:
/// the next answer is that it still holds the lease.
#[tokio::test]
async fn still_held_gives_a_head_error_and_keeps_the_tag() {
    let clock = Arc::new(TestClock::new());
    let mem = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
    let hot = Arc::new(FaultyBackend::new(mem));
    let mut a = Lease::new(hot.clone(), clock, "p", TTL);
    assert!(a.ensure().await);
    let down = Error::Unavailable("on purpose".to_owned());
    hot.fail_next_on(Op::Head, LEASE_KEY, down.clone());
    assert_eq!(a.still_held().await.unwrap_err(), down);
    assert!(a.still_held().await.unwrap());
}
