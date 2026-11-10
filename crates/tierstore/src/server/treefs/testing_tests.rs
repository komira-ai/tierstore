//! The helpers of the tests that tests can hold to what they say.

use super::{reasons, skipped, unavailable, until, with_fuse};

/// Asserts the reasons a mount cannot be made, in the order they are checked, and that the machine's
/// own answer is one of them or none.
#[test]
fn the_reasons_a_mount_cannot_be_made_are_named() {
    assert_eq!(reasons(true, true), None);
    assert_eq!(
        reasons(false, true),
        Some("/dev/fuse cannot be opened for reading and writing")
    );
    assert_eq!(reasons(true, false), Some("fusermount3 is not installed"));
    assert_eq!(
        reasons(false, false),
        Some("/dev/fuse cannot be opened for reading and writing")
    );
    assert!(matches!(unavailable(), None | Some(_)));
}

/// Asserts that a test is run where a mount can be made and a reason is said where it cannot.
#[test]
fn a_test_that_needs_a_mount_runs_or_says_why_not() {
    skipped("this line is the form a skip takes");
    let mut ran = false;
    with_fuse(|| ran = true);
    assert_eq!(ran, unavailable().is_none());
}

/// Asserts that an event another thread brings is waited for, and that one that is already there
/// is not waited for.
#[test]
fn an_event_is_waited_for_by_looking_again() {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setter = std::sync::Arc::clone(&flag);
    let thread =
        std::thread::spawn(move || setter.store(true, std::sync::atomic::Ordering::Release));
    until(|| flag.load(std::sync::atomic::Ordering::Acquire));
    thread.join().unwrap();
    until(|| true);
}
