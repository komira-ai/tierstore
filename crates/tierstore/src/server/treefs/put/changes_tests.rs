//! The waits of the commits: a change of two things that share a shard tells it once.

use std::sync::atomic::Ordering;

use super::{Changes, Topic};
use crate::TestClock;

/// Asserts that a change of a file and of a key that fall in one shard takes that shard's lock and
/// wakes its waits once, and that the wait on the file sees the change.
#[test]
fn a_change_of_two_things_in_one_shard_tells_it_once() {
    let key = "cas/b/build/out";
    let id = (0..10_000_u64)
        .find(|id| Changes::index(Topic::File(*id)) == Changes::index(Topic::Key(key)))
        .unwrap();
    let changes = Changes::new();
    let clock = TestClock::default();
    let seen = changes.seen();
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| changes.wait(Topic::File(id), seen, u64::MAX, &clock));
        while changes.waits.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        changes.notify(&[Topic::File(id), Topic::Key(key)]);
        assert!(waiting.join().unwrap());
    });
    assert_eq!(changes.told.load(Ordering::Acquire), 1);
}
