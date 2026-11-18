//! What waits for what in attaching and detaching: no lock of the core is held across a round trip
//! on the fill socket, a manifest read or a write of the attachments file, and the pins keep their
//! order against the unpin of their tree. The round trips are held in flight by the gates of the
//! fake fill socket, and a lock is looked at with a try, so that a test that finds one held fails
//! and does not wait.

use std::sync::{Arc, Mutex};

use super::pins::PinLocks;
use super::placed::key_of;
use super::{AttachError, Core, Looked};
use crate::server::local_cache::trees::image_path;
use crate::server::treefs::attach::Attachments;
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{
    Built, Rig, TREE_IDLE, file, image_bytes, open_cached, sample, until,
};
use crate::tree_manifest::TreeDigest;

const RUN: &str = "run-1";

/// Runs `action` when dropped: lets the round trips that a test holds go on even when the test
/// fails, so that no thread waits for ever.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// A rig over the sample tree, attached by `RUN` (tree 1), with two more trees in the upstream.
struct Trees {
    rig: Rig,
    /// The digest of the sample tree.
    busy: String,
    /// The inode of `big.bin` of the sample tree.
    busy_file: u64,
    /// The digests of the two other trees.
    other: String,
    fresh: String,
}

fn trees(name: &str) -> Trees {
    let rig = Rig::new(name, sample());
    let (busy, other, fresh) = {
        let mut built = rig.upstream.built.lock().unwrap();
        let busy = built.root.clone();
        let other = built.add_tree(vec![("f", file(b"the other tree"))]);
        let fresh = built.add_tree(vec![("g", file(b"the fresh tree"))]);
        (busy, other, fresh)
    };
    let _ = rig.core.attach(&busy, RUN).unwrap();
    let root = ino::of(1, 0);
    let busy_file = match rig.core.lookup(root, "big.bin").unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("big.bin: {other:?}"),
    };
    let set = |digest: &str| PinLocks::set_of(&TreeDigest::from_hex(digest).unwrap());
    assert_ne!(set(&busy), set(&other), "the trees are in one set of pins");
    Trees {
        rig,
        busy,
        busy_file,
        other,
        fresh,
    }
}

/// Asserts, while an unpin of the tree `t.other` is in flight, what no other request waits for: the
/// lifecycle lock is free, an open of a blob of the busy tree pins it (its pin round trip runs and
/// the open completes), and a lookup that attaches a new tree completes.
fn nothing_waits(t: &Trees) {
    let core = &t.rig.core;
    assert!(
        core.lifecycle.try_lock().is_ok(),
        "the lifecycle lock is held across the unpin"
    );
    let pins = t.rig.upstream.pins_received();
    std::thread::scope(|scope| {
        let open = scope.spawn(|| open_cached(core, t.busy_file, false));
        until(|| open.is_finished());
        core.release(open.join().unwrap().unwrap());
    });
    assert_eq!(t.rig.upstream.pins_received(), pins + 1);
    std::thread::scope(|scope| {
        let attach = scope.spawn(|| core.lookup(ino::TREES, &t.fresh));
        until(|| attach.is_finished());
        assert!(matches!(attach.join().unwrap(), Ok(Looked::Found(_))));
    });
}

/// Asserts that while the sweep of idle trees has its unpin of a tree in flight, the lifecycle lock
/// is free, an open of a blob of another tree pins it, and a lookup that attaches a tree
/// implicitly completes; and that the sweep then ends with the tree unpinned.
#[test]
fn an_idle_detach_holds_no_lock_but_its_trees_while_its_unpin_is_in_flight() {
    let t = trees("tfs-life-idle");
    let core = &t.rig.core;
    assert!(core.lookup(ino::TREES, &t.other).is_ok());
    t.rig.clock.advance(TREE_IDLE);
    t.rig.upstream.gate_verb("unpin");
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.rig.upstream.permit_verb("unpin", 1_000));
        let sweep = scope.spawn(|| core.detach_idle());
        t.rig.upstream.wait_for_verb("unpin", 1);
        nothing_waits(&t);
        t.rig.upstream.permit_verb("unpin", 1);
        assert_eq!(sweep.join().unwrap(), 1);
    });
    let unpins = t.rig.upstream.asked("unpin");
    assert_eq!(unpins.len(), 1);
    assert!(unpins[0].ends_with(&t.other), "{unpins:?}");
}

/// Asserts the same for the detach of a run: its unpin in flight holds up neither the lifecycle
/// lock, an open that pins in another tree, nor a lookup that attaches a tree.
#[test]
fn a_detach_holds_no_lock_but_its_trees_while_its_unpin_is_in_flight() {
    let t = trees("tfs-life-detach");
    let core = &t.rig.core;
    let _ = core.attach(&t.other, "run-2").unwrap();
    t.rig.upstream.gate_verb("unpin");
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.rig.upstream.permit_verb("unpin", 1_000));
        let detach = scope.spawn(|| core.detach("run-2"));
        t.rig.upstream.wait_for_verb("unpin", 1);
        nothing_waits(&t);
        t.rig.upstream.permit_verb("unpin", 1);
        assert_eq!(detach.join().unwrap().unwrap().len(), 0);
    });
    let unpins = t.rig.upstream.asked("unpin");
    assert_eq!(unpins.len(), 1);
    assert!(unpins[0].ends_with(&t.other), "{unpins:?}");
}

/// Asserts that a hold waits for the unpin of a detach decided before it, which runs with the
/// lifecycle lock released: the hold returns only after that unpin was let through, so no unpin
/// of this process follows the pins of a successor.
#[test]
fn a_hold_waits_for_the_unpin_of_a_detach_decided_before_it() {
    let t = trees("tfs-life-hold");
    let core = &t.rig.core;
    let _ = core.attach(&t.other, "run-2").unwrap();
    t.rig.upstream.gate_verb("unpin");
    let let_through = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.rig.upstream.permit_verb("unpin", 1_000));
        let detach = scope.spawn(|| core.detach("run-2"));
        t.rig.upstream.wait_for_verb("unpin", 1);
        let hold = scope.spawn(|| {
            core.hold(true);
            let_through.load(std::sync::atomic::Ordering::SeqCst)
        });
        until(|| core.is_held());
        let_through.store(true, std::sync::atomic::Ordering::SeqCst);
        t.rig.upstream.permit_verb("unpin", 1);
        assert!(
            hold.join().unwrap(),
            "the hold returned before the unpin was let through"
        );
        assert!(detach.join().unwrap().is_some());
    });
    core.hold(false);
    assert_eq!(t.rig.upstream.asked("unpin").len(), 1);
}

/// Asserts that a detach takes the lock of the pins of each tree it frees and of no other: a run
/// that attached nothing takes none, and a tree that another run still has takes none.
#[test]
fn a_detach_takes_the_pin_lock_of_each_tree_it_frees_and_of_no_other() {
    let t = trees("tfs-life-locks");
    let core = &t.rig.core;
    assert!(core.detach("nobody").is_none());
    assert_eq!(core.pins.unpins_asked(), 0);
    let _ = core.attach(&t.busy, "run-2").unwrap();
    let _ = core.attach(&t.other, "run-2").unwrap();
    assert!(core.detach("run-2").is_some());
    assert_eq!(core.pins.unpins_asked(), 1);
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.pins.unpins_asked(), 2);
}

/// Asserts that a pin in flight when a detach of its tree starts is unpinned by that detach after
/// it: the unpin is not sent while the pin is in flight, and is sent once the pin is answered.
#[test]
fn a_pin_in_flight_when_a_detach_starts_is_unpinned_after_it() {
    let t = trees("tfs-life-order");
    let core = &t.rig.core;
    t.rig.upstream.gate_pins();
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.rig.upstream.permit_pins(1_000));
        let open = scope.spawn(|| open_cached(core, t.busy_file, false));
        until(|| t.rig.upstream.pins_received() == 1);
        let digest = TreeDigest::from_hex(&t.busy).unwrap();
        assert!(
            !core.pins.free(&digest),
            "an unpin of the tree could start while its pin is in flight"
        );
        let detach = scope.spawn(|| core.detach(RUN));
        until(|| core.pins.unpins_asked() == 1);
        assert!(
            t.rig.upstream.asked("unpin").is_empty(),
            "the unpin was sent while a pin was in flight"
        );
        t.rig.upstream.permit_pins(1);
        core.release(open.join().unwrap().unwrap());
        assert!(detach.join().unwrap().is_some());
    });
    let lines = locked(&t.rig.upstream.requests).clone();
    let at = |verb: &str| lines.iter().position(|line| line.starts_with(verb));
    assert!(at("pin ") < at("unpin "), "{lines:?}");
    assert_eq!(t.rig.upstream.asked("unpin").len(), 1);
}

/// Asserts that a tree attached again between the decision of a detach and its unpin is not
/// unpinned: no unpin is sent, and the blobs it had pinned stay recorded.
#[test]
fn a_tree_attached_again_before_its_unpin_is_not_unpinned() {
    let t = trees("tfs-life-again");
    let core = &t.rig.core;
    core.release(open_cached(core, t.busy_file, false).unwrap());
    let loaded = core.tree(1).unwrap();
    assert_eq!(loaded.pinned.len(), 1);
    let digest = TreeDigest::from_hex(&t.busy).unwrap();
    // The detach has decided and waits for the lock of the tree's pins, which this holds.
    let held = core.pins.unpin(&digest);
    std::thread::scope(|scope| {
        let detach = scope.spawn(|| core.detach(RUN));
        until(|| core.pins.unpins_asked() == 2);
        assert!(core.attachments().is_empty());
        let _ = core.attach(&t.busy, "run-2").unwrap();
        drop(held);
        assert!(detach.join().unwrap().is_some());
    });
    assert!(t.rig.upstream.asked("unpin").is_empty());
    assert_eq!(loaded.pinned.len(), 1);
    assert_eq!(core.attachments(), [(digest, "run-2".to_owned())]);
    assert_eq!(core.trees_in_memory(), 1);
}

/// Asserts that an attach that is reading the tree's manifests holds no lock: the lifecycle lock is
/// free, so a hold of the table of tree numbers and the attachments file does not wait for it, and
/// the attach, which finishes after the hold, is refused as every attach after it is.
#[test]
fn an_attach_reading_manifests_holds_no_lifecycle_lock() {
    let rig = Rig::new("tfs-life-read", sample());
    let core = &rig.core;
    let fresh = {
        let mut built = rig.upstream.built.lock().unwrap();
        built.add_tree(vec![("g", file(b"the fresh tree"))])
    };
    rig.upstream.gate_manifests();
    std::thread::scope(|scope| {
        let _go = OnDrop(|| rig.upstream.permit_manifests(1_000));
        let attach = scope.spawn(|| core.attach(&fresh, RUN));
        rig.upstream.wait_for_manifest_fills(1);
        assert!(
            core.lifecycle.try_lock().is_ok(),
            "the lifecycle lock is held while the manifests are read"
        );
        core.hold(true);
        rig.upstream.permit_manifests(1_000);
        assert!(matches!(attach.join().unwrap(), Err(AttachError::Replaced)));
    });
    assert!(core.attachments().is_empty());
    core.hold(false);
    assert!(core.attach(&fresh, RUN).is_ok());
    assert_eq!(core.attachments().len(), 1);
}

/// Asserts that an attach refused while a successor takes over reads nothing: the fill socket is
/// not asked for a manifest.
#[test]
fn an_attach_refused_while_held_reads_no_manifest() {
    let rig = Rig::new("tfs-life-held", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    rig.core.hold(true);
    assert!(matches!(
        rig.core.attach(&root, RUN),
        Err(AttachError::Replaced)
    ));
    assert!(rig.upstream.asked("fill").is_empty());
}

/// A rig whose node places the image of [`image_bytes`] at each of `paths`, with the image in the
/// cache directory.
fn placed(name: &str, paths: &[&str]) -> Rig {
    const ID: &str = "0123456789abcdef0123456789abcdef";
    let rig = Rig::new(name, Built::of(Vec::new()));
    let bytes = image_bytes();
    let tree = TreeDigest::of(&bytes);
    let at = image_path(&rig.cache, &tree);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(&at, &bytes).unwrap();
    let listed = serde_json::to_string(paths).unwrap();
    let mut answers = locked(&rig.upstream.answers);
    let _ = answers.insert("place".to_owned(), format!("tree {ID}"));
    let _ = answers.insert("image".to_owned(), format!("image {tree} {listed}"));
    drop(answers);
    let sets: Vec<usize> = paths
        .iter()
        .map(|path| PinLocks::set_of(&key_of(ID, path).unwrap()))
        .collect();
    assert_ne!(sets[0], sets[1], "the paths are in one set of pins");
    rig
}

/// Asserts that while the hold of one placed path is settled (its `place` round trip in flight),
/// the lifecycle lock is free and a lookup of another placed path makes its own round trip at
/// once: two are in flight together.
#[test]
fn the_hold_settled_for_one_placed_path_does_not_hold_up_another_path() {
    let rig = placed("tfs-life-place", &["jobs", "jobs2"]);
    let core = &rig.core;
    rig.upstream.gate_verb("place");
    std::thread::scope(|scope| {
        let _go = OnDrop(|| rig.upstream.permit_verb("place", 1_000));
        let first = scope.spawn(|| core.lookup(ino::ROOT, "jobs"));
        let second = scope.spawn(|| core.lookup(ino::ROOT, "jobs2"));
        // The two lookups ask where their paths are, and the two answers go first; then each
        // settles its hold.
        rig.upstream.wait_for_verb("place", 2);
        rig.upstream.permit_verb("place", 2);
        // Both settle: each asks for the placement again, with the other in flight.
        until(|| rig.upstream.received("place") >= 4);
        assert_eq!(rig.upstream.in_flight("place"), 2);
        assert!(
            core.lifecycle.try_lock().is_ok(),
            "the lifecycle lock is held across the settling of a hold"
        );
        rig.upstream.permit_verb("place", 2);
        assert!(matches!(first.join().unwrap(), Ok(Looked::Found(_))));
        assert!(matches!(second.join().unwrap(), Ok(Looked::Found(_))));
    });
}

/// Asserts that the settling of the hold of a path waits for an unpin of its own tree, which holds
/// the lock of the tree's pins: it is not made while that lock is held.
#[test]
fn the_hold_of_a_path_is_settled_under_the_lock_of_its_tree() {
    let rig = placed("tfs-life-settle", &["jobs", "jobs2"]);
    let core = &rig.core;
    let key = key_of("0123456789abcdef0123456789abcdef", "jobs").unwrap();
    let held = core.pins.unpin(&key);
    std::thread::scope(|scope| {
        let lookup = scope.spawn(|| core.lookup(ino::ROOT, "jobs"));
        until(|| core.pins.unpins_asked() == 2);
        // Asked: the first `place` and the attach are done, and the settling waits.
        assert_eq!(rig.upstream.asked("place").len(), 1);
        drop(held);
        assert!(matches!(lookup.join().unwrap(), Ok(Looked::Found(_))));
    });
    assert_eq!(rig.upstream.asked("place").len(), 2);
}

/// Asserts that the attachments file is written with the state lock free, at an attach, a detach
/// and a restore, and holds what the attachments were.
#[test]
fn the_attachments_file_is_written_with_the_state_lock_free() {
    let t = trees("tfs-life-file");
    let core = &t.rig.core;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    *locked(&core.hooks.saving) = Some(Box::new(move |core: &Core| {
        locked(&log).push(core.state.try_lock().is_ok());
    }));
    let _ = core.attach(&t.other, "run-2").unwrap();
    assert!(core.detach("run-2").is_some());
    let fresh = TreeDigest::from_hex(&t.fresh).unwrap();
    core.restore(vec![(fresh, "run-3".to_owned())], 1);
    assert_eq!(*locked(&seen), [true, true, true]);
    let busy = TreeDigest::from_hex(&t.busy).unwrap();
    let saved = Attachments::saved(&t.rig.runtime).unwrap();
    assert_eq!(saved, [(busy, RUN.to_owned()), (fresh, "run-3".to_owned())]);
}

/// Asserts that an attach whose file cannot be written is rolled back, with the state lock free
/// again: a new attachment is gone, and an attachment that was there stays.
#[test]
fn an_attach_whose_file_cannot_be_written_is_rolled_back() {
    let t = trees("tfs-life-rollback");
    let core = &t.rig.core;
    let busy = TreeDigest::from_hex(&t.busy).unwrap();
    std::fs::remove_dir_all(&t.rig.runtime).unwrap();
    let why = core.attach(&t.other, "run-2").unwrap_err();
    assert!(matches!(why, AttachError::File(_)), "{why}");
    assert_eq!(core.attachments(), [(busy, RUN.to_owned())]);
    let again = core.attach(&t.busy, RUN).unwrap_err();
    assert!(matches!(again, AttachError::File(_)), "{again}");
    assert_eq!(core.attachments(), [(busy, RUN.to_owned())]);
    assert!(core.state.try_lock().is_ok());
}

/// Asserts that an open that waited for the lock of its tree's pins, which an unpin held, and finds
/// the blob pinned when it has it, sends no pin of its own.
#[test]
fn an_open_that_waited_for_an_unpin_finds_the_blob_pinned_and_pins_nothing() {
    let t = trees("tfs-life-waited");
    let core = &t.rig.core;
    let digest = TreeDigest::from_hex(&t.busy).unwrap();
    let loaded = core.tree(1).unwrap();
    let held = core.pins.unpin(&digest);
    std::thread::scope(|scope| {
        let open = scope.spawn(|| open_cached(core, t.busy_file, false));
        until(|| core.pins.pins_asked() == 1);
        // The blob is pinned by another open while this one waits.
        let entry = loaded.tree.entry(ino::split(t.busy_file).1).unwrap();
        let crate::tree_manifest::EntryKind::File { digest: blob, .. } = entry.kind else {
            panic!("not a file");
        };
        loaded.pinned.insert(blob);
        drop(held);
        core.release(open.join().unwrap().unwrap());
    });
    assert_eq!(t.rig.upstream.pins_received(), 0);
}

/// Asserts that a restore of a tree that is in memory already keeps that tree, and attaches it.
#[test]
fn a_restore_of_a_tree_in_memory_keeps_it() {
    let t = trees("tfs-life-restore");
    let core = &t.rig.core;
    let before = core.tree(1).unwrap();
    let busy = TreeDigest::from_hex(&t.busy).unwrap();
    core.restore(vec![(busy, "run-4".to_owned())], 1);
    assert!(Arc::ptr_eq(&core.tree(1).unwrap(), &before));
    assert_eq!(core.attachments().len(), 2);
}
