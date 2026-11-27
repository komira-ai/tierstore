//! The trees in memory with no lock: the word that counts a tree's lookups and marks its removal,
//! the sets of pinned blobs, the map of the trees, and what the requests that name a tree in
//! memory take.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Loaded, Lookups, Pinned, Trees};
use crate::server::local_cache::Blob;
use crate::server::treefs::core::fetched::Fetched;
use crate::server::treefs::core::{Core, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{Rig, TREE_IDLE, file, open_cached, sample, sha};
use crate::tree_manifest::TreeDigest;

const RUN: &str = "run-1";

/// A digest whose first byte is `first`.
fn digest(first: u8) -> TreeDigest {
    let mut bytes = [7_u8; 32];
    bytes[0] = first;
    TreeDigest::from_bytes(bytes)
}

/// A rig over the sample tree with `RUN` attached (tree number 1).
fn attached(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    rig
}

fn found(core: &Core, parent: u64, name: &str) -> u64 {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{name}: {other:?}"),
    }
}

/// The tree of the rig's attach, as a `Loaded` with nothing counted.
fn loaded_of(rig: &Rig) -> Loaded {
    let loaded = rig.core.tree(1).unwrap();
    let tree = rig.core.read_tree(&loaded.digest, &Fetched::default());
    let tree = tree.unwrap();
    Loaded::new(loaded.digest, tree, 0)
}

/// Asserts the count: a lookup raises it, a forget takes away what it names, and a forget of more
/// than was counted leaves none counted and marks the tree uncounted only where lookups are
/// counted.
#[test]
fn a_count_rises_with_a_lookup_and_a_larger_forget_marks_the_tree_uncounted() {
    let lookups = Lookups::default();
    assert!(lookups.raise() && lookups.raise() && lookups.raise());
    assert_eq!(lookups.count(), 3);
    assert!(lookups.forget(2, true));
    assert_eq!((lookups.count(), lookups.uncounted()), (1, false));
    assert!(lookups.forget(5, false));
    assert_eq!((lookups.count(), lookups.uncounted()), (0, false));
    assert!(lookups.raise());
    assert!(lookups.forget(5, true));
    assert_eq!((lookups.count(), lookups.uncounted()), (0, true));
    // The mark stays through a later forget that is no larger than the count.
    assert!(lookups.raise());
    assert!(lookups.forget(1, true));
    assert_eq!((lookups.count(), lookups.uncounted()), (0, true));
}

/// Asserts that a tree is marked as removed only when nothing is counted and it is not uncounted,
/// and that a tree so marked takes no count and no forget, until a lookup puts it back.
#[test]
fn a_tree_is_marked_removed_only_at_zero_and_a_mark_refuses_counts_and_forgets() {
    let counted = Lookups::default();
    assert!(counted.raise());
    assert!(!counted.is_forgotten());
    assert!(!counted.remove_if_forgotten());
    assert!(counted.forget(1, true));
    assert!(counted.is_forgotten());
    assert!(counted.remove_if_forgotten());
    assert!(counted.removed());
    assert!(!counted.raise());
    assert!(!counted.forget(1, true));
    assert_eq!(counted.count(), 0);
    counted.raise_and_keep();
    assert_eq!((counted.count(), counted.removed()), (1, false));
    let uncounted = Lookups::default();
    assert!(uncounted.forget(1, true));
    assert!(!uncounted.is_forgotten());
    assert!(!uncounted.remove_if_forgotten());
}

/// Asserts that the sweep's mark is set on a tree whatever it has counted, counted or uncounted,
/// keeping the count and the mark of an uncounted tree, that it is not set twice, and that a
/// lookup that puts the tree back clears it and adds to the count.
#[test]
fn the_sweeps_mark_is_set_whatever_the_count_and_keeps_it() {
    let counted = Lookups::default();
    assert!(counted.raise() && counted.raise());
    assert!(counted.remove());
    assert_eq!(
        (counted.count(), counted.removed(), counted.uncounted()),
        (2, true, false)
    );
    assert!(!counted.remove());
    let uncounted = Lookups::default();
    assert!(uncounted.forget(1, true));
    assert!(uncounted.raise() && uncounted.raise());
    assert!(uncounted.remove());
    assert_eq!((uncounted.count(), uncounted.removed()), (2, true));
    assert!(!uncounted.remove());
    uncounted.raise_and_keep();
    assert_eq!(
        (
            uncounted.count(),
            uncounted.removed(),
            uncounted.uncounted()
        ),
        (3, false, true)
    );
}

/// Asserts that the pinned blobs are found, kept and cleared across every set they are split in.
#[test]
fn pinned_blobs_are_kept_in_every_set_and_cleared_together() {
    let pinned = Pinned::default();
    let blobs: Vec<TreeDigest> = (0..=255).map(digest).collect();
    assert!(blobs.iter().all(|blob| !pinned.contains(blob)));
    blobs.iter().for_each(|blob| pinned.insert(*blob));
    pinned.insert(blobs[3]);
    assert_eq!(pinned.len(), 256);
    assert!(blobs.iter().all(|blob| pinned.contains(blob)));
    pinned.clear();
    assert_eq!(pinned.len(), 0);
    assert!(blobs.iter().all(|blob| !pinned.contains(blob)));
}

/// Asserts that the map keeps the tree it has and drops the one that is offered after it, that a
/// tree is taken out only where it is the tree the map has and is marked removed, and that a
/// lookup puts back a tree that is gone, or counts on the tree the map has now.
#[test]
fn the_map_keeps_one_tree_per_number_and_takes_out_only_a_marked_tree() {
    let rig = attached("tfs-loaded-map");
    let trees = Trees::default();
    assert!(trees.get(5).is_none());
    let first = trees.keep(5, loaded_of(&rig));
    let again = trees.keep(5, loaded_of(&rig));
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(trees.len(), 1);
    assert_eq!(trees.all().len(), 1);
    // A tree that is there by the time the lock of the changes is taken is the one kept.
    assert!(Arc::ptr_eq(&trees.insert(5, loaded_of(&rig)), &first));
    // Not marked: not taken out. Not the one in the map: not taken out.
    assert!(trees.take_out(5, &first).is_none());
    let stranger = Arc::new(loaded_of(&rig));
    assert!(stranger.lookups.remove_if_forgotten());
    assert!(trees.take_out(5, &stranger).is_none());
    assert_eq!(trees.len(), 1);
    // Marked, and the one there: out.
    assert!(first.lookups.remove_if_forgotten());
    assert!(trees.take_out(5, &first).is_some());
    assert_eq!(trees.len(), 0);
    // A lookup that holds it puts it back; another lookup counts on the one that is there.
    trees.put_back(5, &first);
    assert!(Arc::ptr_eq(&trees.get(5).unwrap(), &first));
    assert_eq!((first.lookups.count(), first.lookups.removed()), (1, false));
    trees.put_back(5, &stranger);
    assert!(Arc::ptr_eq(&trees.get(5).unwrap(), &first));
    assert_eq!(first.lookups.count(), 2);
    assert_eq!(
        (stranger.lookups.count(), stranger.lookups.removed()),
        (0, true)
    );
}

/// Asserts that the requests that name a tree in memory take the state lock no time at all: a
/// lookup, `getattr`, `readlink`, `opendir`, a listing, the count of a listing's entry, an open of
/// a blob pinned already, a read, a release, and a forget that leaves a lookup counted. (The
/// attach and the first open of a blob take it: to see whether the tree is attached.)
#[test]
fn requests_that_name_a_tree_in_memory_take_no_state_lock() {
    let rig = attached("tfs-loaded-nolock");
    let core = &rig.core;
    let root = ino::of(1, 0);
    let bin = found(core, root, "bin");
    let link = found(core, root, "lib");
    let big = found(core, root, "big.bin");
    // The first open of a blob pins it, which looks at the attachments.
    let warm = open_cached(core, big, false).unwrap();
    core.release(warm);
    let before = core.state_locks.load(Ordering::Acquire);
    assert!(before > 0, "the attach and the first open took the lock");

    let _ = found(core, root, "bin");
    let _ = found(core, bin, "tool");
    assert!(core.getattr(bin).is_ok());
    assert_eq!(&*core.readlink(link).unwrap(), "bin");
    assert!(core.opendir(root).is_ok());
    core.readdir(root, 0, &mut |_, _| true).unwrap();
    core.count(bin);
    let fh = open_cached(core, big, false).unwrap();
    assert_eq!(core.read(fh, 0, 16).unwrap().len(), 16);
    core.release(fh);
    core.forget(bin, 1);
    assert_eq!(core.state_locks.load(Ordering::Acquire), before);
    assert_eq!(core.trees_in_memory(), 1);
}

/// Asserts that a lookup counted at the moment a forget has marked a tree as removed, and before
/// it takes it out, puts the tree back and keeps the lookup: the tree is in memory, as the same
/// tree, with the lookup counted, and the next forget of it takes it out.
#[test]
fn a_lookup_counted_between_the_mark_and_the_removal_puts_the_tree_back() {
    let rig = attached("tfs-loaded-race");
    let core = &rig.core;
    let bin = found(core, ino::of(1, 0), "bin");
    assert!(core.detach(RUN).is_some());
    let held = core.tree(1).unwrap();
    let counted = Arc::clone(&held);
    *locked(&core.hooks.marked) = Some(Box::new(move |core| {
        assert!(counted.lookups.removed());
        // A forget of a tree marked as removed changes nothing.
        core.forget(bin, 1);
        assert!(counted.lookups.removed());
        core.count_lookup(1, &counted);
    }));
    core.forget(bin, 1);
    *locked(&core.hooks.marked) = None;
    assert_eq!(core.trees_in_memory(), 1);
    assert!(Arc::ptr_eq(&core.trees.get(1).unwrap(), &held));
    assert_eq!((held.lookups.count(), held.lookups.removed()), (1, false));
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts the same for the sweep for idle trees: a lookup counted between its mark and its
/// removal keeps the tree, uncounted, with the count; the next sweep that finds it idle takes it.
#[test]
fn a_lookup_counted_between_the_sweeps_mark_and_removal_keeps_the_tree() {
    let rig = attached("tfs-loaded-sweep-race");
    let core = &rig.core;
    let bin = found(core, ino::of(1, 0), "bin");
    core.forget(bin, 5);
    assert!(core.detach(RUN).is_some());
    let held = core.tree(1).unwrap();
    let counted = Arc::clone(&held);
    rig.clock.advance(TREE_IDLE + Duration::from_millis(1));
    *locked(&core.hooks.marked) = Some(Box::new(move |core| core.count_lookup(1, &counted)));
    assert_eq!(core.drop_idle(1), 0);
    *locked(&core.hooks.marked) = None;
    assert_eq!(core.trees_in_memory(), 1);
    assert_eq!((held.lookups.count(), held.lookups.uncounted()), (1, true));
    assert_eq!(core.drop_idle(1), 1);
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts that a lookup holding a tree that was taken out of memory counts on the read of the tree
/// that is in memory now, and leaves the old read out.
#[test]
fn a_lookup_of_a_tree_that_was_read_again_counts_on_the_new_read() {
    let rig = attached("tfs-loaded-reread");
    let core = &rig.core;
    let bin = found(core, ino::of(1, 0), "bin");
    let old = core.tree(1).unwrap();
    assert!(core.detach(RUN).is_some());
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 0);
    let new = core.tree(1).unwrap();
    assert!(!Arc::ptr_eq(&old, &new));
    core.count_lookup(1, &old);
    assert_eq!(new.lookups.count(), 1);
    assert_eq!((old.lookups.count(), old.lookups.removed()), (0, true));
    assert!(Arc::ptr_eq(&core.trees.get(1).unwrap(), &new));
}

/// Asserts that a forget of more lookups than a tree has counted leaves it counted at zero and
/// uncounted, so that the end of its last attachment does not take it out of memory, and that the
/// lookups counted since are forgotten without taking it out either.
#[test]
fn a_forget_of_more_than_counted_marks_the_tree_uncounted_and_it_stays() {
    let rig = attached("tfs-loaded-uncounted");
    let core = &rig.core;
    let bin = found(core, ino::of(1, 0), "bin");
    core.forget(bin, 5);
    let loaded = core.tree(1).unwrap();
    assert_eq!(
        (loaded.lookups.count(), loaded.lookups.uncounted()),
        (0, true)
    );
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 1);
    core.count(bin);
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 1);
}

/// Asserts that a core that does not count lookups leaves the count alone: a lookup raises none,
/// and a forget of any size marks nothing.
#[test]
fn a_core_that_counts_no_lookups_counts_none_and_marks_nothing() {
    let rig = Rig::uncounted("tfs-loaded-nocount", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    let core = &rig.core;
    let bin = found(core, ino::of(1, 0), "bin");
    let loaded = core.tree(1).unwrap();
    assert_eq!(loaded.lookups.count(), 0);
    core.forget(bin, 9);
    assert!(!loaded.lookups.uncounted());
}

/// Asserts that many trees at once are in memory, each leaves it at the detach of its run, and the
/// map of trees comes out empty.
#[test]
fn many_trees_come_into_memory_and_leave_it_with_their_runs() {
    let rig = Rig::new("tfs-loaded-many", sample());
    let core = &rig.core;
    let digests: Vec<String> = (0..70)
        .map(|i| {
            let content = format!("tree {i}");
            let mut built = rig.upstream.built.lock().unwrap();
            built.add_tree(vec![("f", file(content.as_bytes()))])
        })
        .collect();
    for (i, digest) in digests.iter().enumerate() {
        let _ = core.attach(digest, &format!("run-{i}")).unwrap();
    }
    assert_eq!(core.trees_in_memory(), 70);
    for number in 1..=70 {
        assert!(core.getattr(ino::of(number, 0)).is_ok(), "tree {number}");
    }
    for i in 0..70 {
        assert!(core.detach(&format!("run-{i}")).is_some());
    }
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts that a forget, a drop and an unpin of a tree that is not in memory change nothing: the
/// tree is still not there, and the unpin is sent (the cache, which pinned a blob for that tree of
/// the mount, no longer pins it).
#[test]
fn a_tree_that_is_not_in_memory_is_forgotten_dropped_and_unpinned_with_no_change() {
    let rig = Rig::new("tfs-loaded-absent", sample());
    let core = &rig.core;
    let tool = sha(b"tool bytes");
    let from = format!("cas/l/{tool}");
    let blob = Blob {
        sha256: &tool,
        size: 10,
        from: &from,
        offset: 0,
    };
    rig.work
        .handle()
        .block_on(rig.node.fill_blob(&blob))
        .unwrap();
    let pins = rig
        .node
        .pin("mount-1", &digest(1).to_string(), &[tool])
        .unwrap();
    assert_eq!(pins.pinned, 1);
    assert_eq!(rig.node.pinned_bytes(), 10);
    core.forget(ino::of(9, 3), 1);
    core.drop_if_forgotten(9);
    core.free_tree(9, &digest(1));
    assert_eq!(core.trees_in_memory(), 0);
    assert_eq!(rig.node.pinned_bytes(), 0);
}
