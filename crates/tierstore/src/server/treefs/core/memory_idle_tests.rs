//! The bound on the trees in memory: a tree that nothing has attached leaves memory at the sweep
//! once no request has named it for `fs.tree_idle_secs`, whatever lookups of it the kernel
//! holds, and is read again, answering the same, when a request names it after that; a tree that
//! is attached stays. And the line a look says of the trees in memory.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{Core, Entry, Errno, Looked};
use super::{Footprint, lists, memory_line};
use crate::server::local_cache::trees::Mapped;
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{Logged, Rig, TREE_IDLE, logged, sample};

const RUN: &str = "run-1";

/// The inode of the root of the tree numbered 1.
fn root_ino() -> u64 {
    ino::of(1, 0)
}

/// A rig over the sample tree with `RUN` attached (tree number 1), and the inode of `bin` below
/// its root, looked up once (one lookup counted).
fn attached(name: &str) -> (Rig, u64) {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    let bin = found(&rig.core, root_ino(), "bin");
    (rig, bin)
}

fn found(core: &Core, parent: u64, name: &str) -> u64 {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{name}: {other:?}"),
    }
}

/// What the core answers of `bin`: its attributes, the lookup of `tool` in it, and its listing.
fn answers(core: &Core, bin: u64) -> (Entry, Looked, Vec<(u64, String, Entry)>) {
    let mut listing = Vec::new();
    core.readdir(bin, 0, &mut |next, item| {
        listing.push((next, item.name, item.entry));
        true
    })
    .unwrap();
    (
        core.getattr(bin).unwrap(),
        core.lookup(bin, "tool").unwrap(),
        listing,
    )
}

/// Asserts that a tree whose run has detached, and whose inodes the kernel still holds (a lookup
/// counted and never forgotten), stays in memory until no request has named it for
/// `fs.tree_idle_secs` and leaves at the sweep that then finds it idle; that a forget of it while
/// it is out of memory changes nothing; and that the next request reads it again and is answered
/// as before. Catches a sweep that leaves a counted tree in memory until the kernel forgets it,
/// which it does only under memory pressure, so that the process grew with every tree any job
/// named.
#[test]
fn a_counted_tree_detached_and_idle_leaves_at_the_sweep_and_is_read_again_the_same() {
    let (rig, bin) = attached("tfs-idle-counted");
    let core = &rig.core;
    let before = answers(core, bin);
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 1, "a lookup is counted");
    rig.clock.advance(TREE_IDLE - Duration::from_millis(1));
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 1, "not idle for long enough yet");
    rig.clock.advance(Duration::from_millis(1));
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 0);
    core.forget(bin, 1);
    core.forget(root_ino(), 3);
    assert_eq!(core.trees_in_memory(), 0);
    assert_eq!(core.times_read(1), 1);
    assert_eq!(answers(core, bin), before);
    assert_eq!(core.times_read(1), 2);
    assert_eq!(core.trees_in_memory(), 1);
}

/// Asserts that a request that names an inode of a tree the sweep took out, when the tree is gone
/// by then (its manifests are in neither the cache nor the upstream), is `ENOENT`, as after a
/// restart, and leaves nothing in memory.
#[test]
fn a_tree_gone_after_the_sweep_took_it_out_is_no_entry() {
    let (rig, bin) = attached("tfs-idle-gone");
    let core = &rig.core;
    assert!(core.detach(RUN).is_some());
    rig.clock.advance(TREE_IDLE);
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 0);
    rig.upstream.built.lock().unwrap().manifests.clear();
    std::fs::remove_dir_all(&rig.cache).unwrap();
    std::fs::create_dir_all(&rig.cache).unwrap();
    assert_eq!(core.getattr(bin), Err(Errno::NoEnt));
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts that a forget of a tree read again after the sweep took it out, whose lookups the
/// kernel made before (so this read counted none of them), keeps it in memory and answering, and
/// that the sweep takes it out again once it is idle.
#[test]
fn a_forget_of_a_tree_read_again_keeps_it_until_the_next_idle_sweep() {
    let (rig, bin) = attached("tfs-idle-reread-forget");
    let core = &rig.core;
    assert!(core.detach(RUN).is_some());
    rig.clock.advance(TREE_IDLE);
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 0);
    assert_eq!(core.getattr(bin).unwrap().attr.ino, bin);
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 1);
    assert_eq!(core.getattr(bin).unwrap().attr.ino, bin);
    rig.clock.advance(TREE_IDLE);
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 0);
    assert_eq!(core.times_read(1), 2);
}

/// Asserts that a sweep that comes while a forget is taking a tree out (the tree is marked as
/// removed, and not yet out of the map) leaves that tree to the forget: it takes nothing, and the
/// forget then takes the tree out once.
#[test]
fn a_sweep_leaves_a_tree_a_forget_is_taking_out_to_it() {
    let (rig, bin) = attached("tfs-idle-forget-race");
    let core = &rig.core;
    assert!(core.detach(RUN).is_some());
    rig.clock.advance(TREE_IDLE);
    let swept = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&swept);
    // The sweep runs inside the hook, under the hook's lock: it must not reach a removal of its
    // own (which runs the hook), and it does not, since the rig's one tree is marked already.
    *locked(&core.hooks.marked) = Some(Box::new(move |core| {
        *locked(&seen) = Some((core.drop_idle(0), core.trees_in_memory()));
    }));
    core.forget(bin, 1);
    *locked(&core.hooks.marked) = None;
    assert_eq!(*locked(&swept), Some((0, 1)));
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts that a tree a run has attached stays in memory at every sweep however long no request
/// names it, with its lookups counted or all forgotten, and leaves at the first sweep after the
/// run detaches once it is idle.
#[test]
fn a_tree_a_run_has_attached_never_leaves_at_the_sweep() {
    let (rig, bin) = attached("tfs-idle-run");
    let core = &rig.core;
    rig.clock.advance(TREE_IDLE * 10);
    let _ = core.detach_idle();
    assert_eq!(core.drop_idle(0), 0);
    assert_eq!(core.trees_in_memory(), 1);
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 1, "a run has it");
    let _ = core.detach_idle();
    assert_eq!(core.drop_idle(0), 0);
    assert_eq!(core.trees_in_memory(), 1);
    assert_eq!(core.times_read(1), 1);
    assert_eq!(core.detach(RUN).map(|got| got.failures.len()), Some(0));
    assert_eq!(
        core.trees_in_memory(),
        0,
        "a forgotten tree leaves at the detach"
    );
}

/// Asserts that a tree a lookup of `trees/<digest>` attached, whose inodes the kernel still holds,
/// leaves memory at the sweep that ends its attachment, and not before; and that the next lookup
/// reads it again and attaches it again.
#[test]
fn a_tree_a_lookup_attached_leaves_memory_with_its_attachment() {
    let rig = Rig::new("tfs-idle-implicit", sample());
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let tree = found(core, ino::TREES, &root);
    let _ = found(core, tree, "bin");
    let number = ino::split(tree).0;
    rig.clock.advance(TREE_IDLE - Duration::from_millis(1));
    assert_eq!(core.detach_idle(), 0);
    assert_eq!((core.trees_in_memory(), core.implicit()), (1, vec![number]));
    rig.clock.advance(Duration::from_millis(1));
    assert_eq!(core.detach_idle(), 1);
    assert_eq!((core.trees_in_memory(), core.implicit()), (0, vec![]));
    assert_eq!(found(core, ino::TREES, &root), tree);
    assert_eq!((core.trees_in_memory(), core.implicit()), (1, vec![number]));
    // The first read, which numbered the tree, is not one of those counted.
    assert_eq!(core.times_read(number), 1);
}

/// The lines said at info while `run` runs that say the trees in memory.
fn memory_lines(run: impl FnOnce()) -> Vec<String> {
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::INFO);
    run();
    drop(guard);
    log.text()
        .lines()
        .filter(|line| line.contains(" in memory, "))
        .map(str::to_owned)
        .collect()
}

/// Asserts the line a look says of the trees in memory: at the first look, then only when what it
/// says changed (a look with nothing changed says nothing): the trees, those attached, their
/// entries and their bytes in MiB rounded up, down to no tree at all.
#[test]
fn a_look_says_the_trees_in_memory_when_they_change() {
    let (rig, _) = attached("tfs-idle-said");
    let core = &rig.core;
    let loaded = core.tree(1).unwrap();
    let entries = loaded.tree.len();
    let mib = loaded.tree.heap_bytes().div_ceil(1 << 20);
    drop(loaded);
    let lines = memory_lines(|| {
        core.look_around();
        core.look_around();
        assert!(core.detach(RUN).is_some());
        core.look_around();
        rig.clock.advance(TREE_IDLE);
        let _ = core.detach_idle();
        core.look_around();
        core.look_around();
    });
    let lists = "; tree lists: 0 mapped, 0 MiB of 512 MiB; 0 released for the cap and 0 idle \
                 since the last sweep";
    assert_eq!(
        lines,
        [
            format!(
                "tierstore fs: 1 tree in memory, 1 of them attached: {entries} entries, \
                 about {mib} MiB{lists}"
            ),
            format!(
                "tierstore fs: 1 tree in memory, 0 of them attached: {entries} entries, \
                 about {mib} MiB{lists}"
            ),
            format!(
                "tierstore fs: 0 trees in memory, 0 of them attached: 0 entries, about 0 MiB{lists}"
            ),
        ]
    );
}

/// Asserts that the bytes of the line are MiB rounded up: none for none, one for a byte, one for a
/// MiB, two for a byte more; that one tree is said in the singular; and that the clause of the
/// tree lists says the lists mapped, their bytes and the cap in MiB rounded up the same way, and
/// the lists released for the cap and idle since the look before.
#[test]
fn the_memory_line_rounds_the_bytes_up_to_mib() {
    let none = lists::Said {
        cap: 4096,
        ..lists::Said::default()
    };
    let line = |bytes: u64| {
        memory_line(Footprint {
            trees: 2,
            attached: 1,
            entries: 7,
            bytes,
            lists: none,
        })
    };
    let words = "tierstore fs: 2 trees in memory, 1 of them attached: 7 entries, about";
    let empty = "; tree lists: 0 mapped, 0 MiB of 1 MiB; 0 released for the cap and 0 idle since \
                 the last sweep";
    assert_eq!(line(0), format!("{words} 0 MiB{empty}"));
    assert_eq!(line(1), format!("{words} 1 MiB{empty}"));
    assert_eq!(line(1 << 20), format!("{words} 1 MiB{empty}"));
    assert_eq!(line((1 << 20) + 1), format!("{words} 2 MiB{empty}"));
    let one = memory_line(Footprint {
        trees: 1,
        attached: 0,
        entries: 3,
        bytes: 0,
        lists: lists::Said {
            mapped: Mapped {
                lists: 3,
                bytes: (2 << 20) + 1,
            },
            cap: 2 << 30,
            released: (4, 5),
        },
    });
    assert_eq!(
        one,
        "tierstore fs: 1 tree in memory, 0 of them attached: 3 entries, about 0 MiB; tree lists: \
         3 mapped, 3 MiB of 2048 MiB; 4 released for the cap and 5 idle since the last sweep"
    );
}
