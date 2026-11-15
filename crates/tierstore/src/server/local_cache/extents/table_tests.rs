//! The entries by extents in the index: holds, the ranges they record, and what removal leaves.

use super::super::index::Index;
use super::super::pin_set::Owner;
use super::map::ExtentMap;
use super::table::Object;

/// The object of a pack of `size` bytes.
fn object(size: u64) -> Object {
    Object {
        key: "cas/pack/one".to_owned(),
        size,
        tag: "t".to_owned(),
        modified: 1,
    }
}

const HEX: &str = "ab";

/// Asserts that a fill's hold makes the entry (empty) and gives its serial, that a second hold
/// is the same entry, that an entry with no range is no size to pin, that ending the last hold of
/// an entry with no range says it is for nothing, and that a hold of a name a whole object has
/// is refused.
#[test]
fn a_hold_makes_the_entry_and_the_last_release_of_an_empty_one_says_so() {
    let mut index = Index::default();
    let first = index.extent_hold(HEX, &object(100)).unwrap();
    assert_eq!(index.extent_hold(HEX, &object(100)), Some(first));
    assert_eq!(index.held_size(HEX), None);
    assert!(!index.extent_release(HEX, first));
    assert!(index.extent_release(HEX, first));
    assert!(!index.extent_release(HEX, first + 1));
    assert!(!index.extent_release("cd", 1));
    assert!(index.extent_drop(HEX).is_some());
    assert!(index.extent_drop(HEX).is_none());

    index.insert("whole", (10, "t".to_owned(), 1), None);
    assert_eq!(index.extent_hold("whole", &object(10)), None);
}

/// Asserts that a range is recorded only for the entry the fill held: the serial of another
/// entry, and a name that has none, are refused; that the answer says the bytes added and whether
/// the entry is whole; that the cache's bytes count the entry's filled bytes once; and that use
/// of a name that has no entry is none.
#[test]
fn a_range_is_recorded_for_the_entry_that_was_held() {
    let mut index = Index::default();
    let serial = index.extent_hold(HEX, &object(100)).unwrap();
    assert_eq!(index.extent_add(HEX, serial + 1, (0, 10)), None);
    assert_eq!(index.extent_add("cd", serial, (0, 10)), None);
    assert_eq!(index.extent_add(HEX, serial, (0, 40)), Some((40, false)));
    assert_eq!(index.extent_add(HEX, serial, (20, 60)), Some((20, false)));
    assert_eq!(index.bytes(), 60);
    assert_eq!(index.held_size(HEX), Some(60));
    assert_eq!(index.extent_add(HEX, serial, (60, 100)), Some((40, true)));
    assert_eq!(index.bytes(), 100);
    assert!(index.extent_used(HEX, 5).is_some());
    assert!(index.extent_used("cd", 5).is_none());
}

/// Asserts that taking an entry out keeps its pins (the whole object that replaces it is pinned
/// by the same trees) and dropping it takes them away, except where a whole object of the name is
/// in the index too, whose pins stay; that an entry that a fill holds or a tree pins is not
/// evicted, and one that is neither is; and that an entry found on the disk is counted.
#[test]
fn removal_keeps_or_takes_the_pins_as_the_caller_says() {
    let owner = Owner {
        mount: "m".to_owned(),
        tree: "t".to_owned(),
    };
    let mut index = Index::default();
    let serial = index.extent_hold(HEX, &object(100)).unwrap();
    let _ = index.extent_add(HEX, serial, (0, 50));
    let _ = index.pins.pin(&owner, HEX, index.held_size(HEX), 1000);
    assert!(index.extent_evict(HEX).is_none());
    let _ = index.extent_release(HEX, serial);
    assert!(index.extent_evict(HEX).is_none());
    assert!(index.pins.is_pinned(HEX));
    assert!(index.extent_take(HEX).is_some());
    assert!(index.pins.is_pinned(HEX));
    assert_eq!(index.bytes(), 0);

    let mut found = ExtentMap::default();
    let _ = found.insert(0, 30);
    index.extent_found(HEX, object(100), found);
    assert_eq!((index.partials.len(), index.partials.bytes()), (1, 30));
    index.insert(HEX, (100, "t".to_owned(), 1), None);
    assert!(index.extent_drop(HEX).is_some());
    assert!(index.pins.is_pinned(HEX));
    assert!(index.extent_evict("cd").is_none());

    let mut index = Index::default();
    let mut found = ExtentMap::default();
    let _ = found.insert(0, 30);
    index.extent_found(HEX, object(100), found);
    assert!(index.extent_evict(HEX).is_some());
}
