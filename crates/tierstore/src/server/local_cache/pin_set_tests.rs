//! The set of pins and its place in the index: whose pins count once, what takes them away, and
//! when the file is due. No disk, no cache.

use super::index::Index;
use super::pin_set::{Owner, PinSet, Pinned, PinsFile, SAVE_EVERY, TreePins};

fn owner(mount: &str, tree: &str) -> Owner {
    Owner {
        mount: mount.to_owned(),
        tree: tree.to_owned(),
    }
}

/// Asserts that a blob that two trees pin is charged once, stays pinned while either pins it,
/// and that a pin of a blob the cache does not hold is `Absent`, not recorded.
#[test]
fn a_blob_two_trees_pin_is_charged_once_and_is_pinned_while_either_pins_it() {
    let mut pins = PinSet::default();
    let (a, b) = (owner("m", "a"), owner("m", "b"));
    assert_eq!(pins.pin(&a, "x", Some(100), 1000), Pinned::Yes);
    assert_eq!(pins.pin(&b, "x", Some(100), 1000), Pinned::Yes);
    assert_eq!(pins.pin(&b, "x", Some(100), 1000), Pinned::Yes);
    assert_eq!(pins.bytes(), 100);
    assert_eq!(pins.pin(&a, "y", None, 1000), Pinned::Absent);
    assert!(!pins.is_pinned("y"));

    assert_eq!(pins.unpin(&a), 1);
    assert!(pins.is_pinned("x"));
    assert_eq!(pins.bytes(), 100);
    assert_eq!(pins.unpin(&a), 0);
    assert_eq!(pins.unpin(&b), 1);
    assert!(!pins.is_pinned("x"));
    assert_eq!(pins.bytes(), 0);
}

/// Asserts the limit: a pin that would pass it is refused whole, one that fits exactly is taken,
/// and a blob that is pinned already costs nothing when another tree pins it past the limit.
#[test]
fn a_pin_past_the_limit_is_refused_and_one_that_fits_exactly_is_taken() {
    let mut pins = PinSet::default();
    let (a, b) = (owner("m", "a"), owner("m", "b"));
    assert_eq!(pins.pin(&a, "x", Some(60), 100), Pinned::Yes);
    assert_eq!(pins.pin(&a, "y", Some(41), 100), Pinned::Refused);
    assert_eq!(pins.pin(&a, "y", Some(40), 100), Pinned::Yes);
    assert_eq!(pins.bytes(), 100);
    assert_eq!(pins.pin(&b, "x", Some(60), 100), Pinned::Yes);
    assert_eq!(pins.pin(&b, "z", Some(1), 100), Pinned::Refused);
    assert_eq!(pins.pin(&b, "z", Some(u64::MAX), u64::MAX), Pinned::Refused);
    assert_eq!(pins.bytes(), 100);
}

/// Asserts that `retain_live` takes away the pins of every tree of another mount and of every
/// tree of the mount not in the list, and no other.
#[test]
fn retain_live_drops_other_mounts_and_trees_not_listed() {
    let mut pins = PinSet::default();
    for (mount, tree, blob) in [
        ("m", "a", "1"),
        ("m", "b", "2"),
        ("m", "c", "3"),
        ("old", "a", "4"),
    ] {
        assert_eq!(
            pins.pin(&owner(mount, tree), blob, Some(10), 1000),
            Pinned::Yes
        );
    }
    let live = vec!["a".to_owned(), "c".to_owned()];
    assert_eq!(pins.retain_live("m", &live), 2);
    assert!(pins.is_pinned("1") && pins.is_pinned("3"));
    assert!(!pins.is_pinned("2") && !pins.is_pinned("4"));
    assert_eq!(pins.bytes(), 20);
    assert_eq!(pins.retain_live("m", &live), 0);
}

/// Asserts that forgetting an object takes its pins from every tree, drops a tree left with none,
/// and does nothing for one that is not pinned.
#[test]
fn forgetting_an_object_takes_its_pins_from_every_tree() {
    let mut pins = PinSet::default();
    let (a, b) = (owner("m", "a"), owner("m", "b"));
    for (who, blob) in [(&a, "x"), (&b, "x"), (&b, "y")] {
        assert_eq!(pins.pin(who, blob, Some(5), 1000), Pinned::Yes);
    }
    pins.forget("z");
    pins.forget("x");
    assert_eq!(pins.bytes(), 5);
    let file = pins.to_file();
    let tree = |t: &str, blobs: &[&str]| TreePins {
        mount: "m".to_owned(),
        tree: t.to_owned(),
        blobs: blobs.iter().map(|b| (*b).to_owned()).collect(),
    };
    assert_eq!(
        file,
        PinsFile {
            trees: vec![tree("b", &["y"])]
        }
    );
}

/// Asserts the file is due when the set changed and a second has passed since the last write (or
/// the clock went back), at once when forced, never when nothing changed, and again after a write
/// that failed.
#[test]
fn the_file_is_due_once_a_second_when_the_set_changed() {
    let mut pins = PinSet::default();
    assert!(pins.due(5000, true).is_none());
    assert_eq!(pins.pin(&owner("m", "a"), "x", Some(1), 10), Pinned::Yes);
    assert_eq!(pins.due(5000, false).map(|f| f.trees.len()), Some(1));
    assert!(pins.due(5000, false).is_none());
    assert_eq!(pins.pin(&owner("m", "b"), "x", Some(1), 10), Pinned::Yes);
    assert!(pins.due(5000 + SAVE_EVERY - 1, false).is_none());
    assert!(pins.is_dirty());
    assert!(pins.due(5000 + SAVE_EVERY - 1, true).is_some());
    assert_eq!(pins.pin(&owner("m", "c"), "x", Some(1), 10), Pinned::Yes);
    assert!(pins.due(5000 + 2 * SAVE_EVERY - 2, false).is_none());
    assert!(pins.due(5000 + 2 * SAVE_EVERY - 1, false).is_some());
    // The clock was set back: the file is due, not held until the old time comes round.
    assert_eq!(pins.pin(&owner("m", "d"), "x", Some(1), 10), Pinned::Yes);
    assert!(pins.due(10, false).is_some());
    pins.unsaved();
    assert!(pins.due(10 + SAVE_EVERY, false).is_some());
}

/// Asserts that an object written over (the index inserts one of the name again) keeps its pins
/// and the pinned bytes follow its new size, and that removing it takes the pins away.
#[test]
fn an_object_written_over_keeps_its_pins_and_a_removed_one_loses_them() {
    let mut index = Index::default();
    index.insert("x", (100, "t".to_owned(), 0), None);
    let size = index.get("x").map(|entry| entry.size);
    assert_eq!(
        index.pins.pin(&owner("m", "a"), "x", size, 1000),
        Pinned::Yes
    );
    assert_eq!(index.pins.bytes(), 100);
    index.insert("x", (250, "u".to_owned(), 0), None);
    assert!(index.pins.is_pinned("x"));
    assert_eq!(index.pins.bytes(), 250);
    index.insert("y", (7, "t".to_owned(), 0), None);
    assert_eq!(index.pins.bytes(), 250);
    assert!(index.remove("x").is_some());
    assert!(!index.pins.is_pinned("x"));
    assert_eq!(index.pins.bytes(), 0);
    assert!(index.remove("x").is_none());
}
