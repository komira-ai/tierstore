//! What waits for what in attaching and detaching: no lock of the core is held across a request
//! to the node's cache, a manifest read or a write of the attachments file, and the pins keep
//! their order against the unpin of their tree. A request is held in flight by the write role of
//! the cache: the core of those tests calls a cache that is read-only until the test gives it the
//! role (every request but a touch and a report waits for it), and what the cache holds is looked
//! at after. A lock is looked at with a try, so that a test that finds one held fails and does not
//! wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::pins::PinLocks;
use super::placed::key_of;
use super::{AttachError, Core, Errno, Looked};
use crate::server::local_cache::testing::{FakeDisk, never_expires, until as eventually};
use crate::server::local_cache::trees::testing::{Bench, fs_at};
use crate::server::local_cache::trees::Created;
use crate::server::local_cache::{CacheConfig, LocalCache, Mode, Tick};
use crate::server::treefs::attach::Attachments;
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::{Live, MOUNT};
use crate::server::treefs::locked;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{Rig, TREE_IDLE, file, open_cached, sample, sha, until};
use crate::tree_manifest::TreeDigest;
use crate::{Backend, TestClock};

const RUN: &str = "run-1";

/// The bytes of `big.bin` of the sample tree, which the busy tree pins.
const BIG: u64 = 300_000;

/// How long the core of a placed test waits for the write role: longer than the test.
const LONG: Duration = Duration::from_secs(30);

/// Runs `action` when dropped: lets the requests that a test holds go on even when the test
/// fails, so that no thread waits for ever.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// A cache over the directory of `rig`, which takes the lock of the directory if it is free and
/// is read-only if it is not.
fn open_locked(rig: &Rig) -> LocalCache {
    let mut config = CacheConfig::new(&rig.cache, 1 << 30, vec![("cas/".to_owned(), true)]);
    never_expires(&mut config);
    let backend: Arc<dyn Backend> = rig.upstream.clone();
    let handle = rig.work.handle();
    let _entered = handle.enter();
    LocalCache::open_locked(
        config,
        backend,
        Arc::new(FakeDisk::new()),
        Arc::new(TestClock::new()),
    )
    .unwrap()
}

/// The cache of a core whose requests wait for the write role: read-only until the test gives it
/// the role, with the pins the holder before it left.
struct Gate {
    cache: LocalCache,
}

impl Gate {
    /// The requests that watch the mode of the cache now: each one waiting for the role.
    fn watching(&self) -> usize {
        self.cache.mode_watchers()
    }

    /// Gives the cache the write role, which the waiting requests then go on with.
    fn give(&self) {
        assert_eq!(self.cache.mode(), Mode::ReadOnly);
        assert_eq!(self.cache.tick().unwrap(), Tick::Took);
    }

    /// [`Gate::give`], if the cache has not the role yet and without a word if it cannot be given.
    fn release(&self) {
        if self.cache.mode() == Mode::ReadOnly {
            let _ = self.cache.tick();
        }
    }
}

/// A rig over the sample tree, attached by `RUN` (tree 1), with two more trees in the upstream.
struct Trees {
    rig: Rig,
    /// The core the test calls: the rig's, or one that calls a cache with no write role yet.
    core: Arc<Core>,
    /// The cache of that core, for a test of requests held in flight.
    gate: Option<Gate>,
    /// The digest of the sample tree.
    busy: String,
    /// The inode of `big.bin` of the sample tree.
    busy_file: u64,
    /// The digests of the two other trees.
    other: String,
    fresh: String,
}

impl Trees {
    fn gate(&self) -> &Gate {
        self.gate.as_ref().expect("a core over a cache with no role")
    }

    /// The bytes the cache of the core keeps pinned.
    fn pinned(&self) -> u64 {
        match &self.gate {
            Some(gate) => gate.cache.pinned_bytes(),
            None => self.rig.node.pinned_bytes(),
        }
    }
}

/// A rig over the sample tree, with two more trees in the upstream, and the digests of the three.
fn rig_of_three(name: &str) -> (Rig, [String; 3]) {
    let rig = Rig::new(name, sample());
    let (busy, other, fresh) = {
        let mut built = rig.upstream.built.lock().unwrap();
        let busy = built.root.clone();
        let other = built.add_tree(vec![("f", file(b"the other tree"))]);
        let fresh = built.add_tree(vec![("g", file(b"the fresh tree"))]);
        (busy, other, fresh)
    };
    (rig, [busy, other, fresh])
}

/// The inode of `name` in the root of the tree `digest` of the core `core`, which has it attached.
fn file_of(core: &Core, digest: &str, name: &str) -> u64 {
    let number = core
        .table
        .find(&TreeDigest::from_hex(digest).unwrap())
        .unwrap();
    match core.lookup(ino::of(number, 0), name).unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{name}: {other:?}"),
    }
}

/// The three trees, assembled around `rig` and `core`: `busy` attached by `RUN`.
fn assembled(rig: Rig, core: Arc<Core>, gate: Option<Gate>, digests: [String; 3]) -> Trees {
    let [busy, other, fresh] = digests;
    let _ = core.attach(&busy, RUN).unwrap();
    let busy_file = file_of(&core, &busy, "big.bin");
    let set = |digest: &str| PinLocks::set_of(&TreeDigest::from_hex(digest).unwrap());
    assert_ne!(set(&busy), set(&other), "the trees are in one set of pins");
    Trees {
        rig,
        core,
        gate,
        busy,
        busy_file,
        other,
        fresh,
    }
}

fn trees(name: &str) -> Trees {
    let (rig, digests) = rig_of_three(name);
    let core = Arc::clone(&rig.core);
    assembled(rig, core, None, digests)
}

/// [`trees`], over a core whose cache is read-only, so that every request that waits for the write
/// role is held in flight until [`Gate::give`]. The cache holds what the trees' files are: the
/// manifests of the three trees and the blobs of `big.bin` of the busy tree and of the file of
/// the other one, filled by the rig's own core first, and pinned for their trees (the pins of an
/// earlier holder of the cache, which this one has when it takes the role).
fn gated(name: &str) -> Trees {
    let (rig, digests) = rig_of_three(name);
    let [busy, other, _] = &digests;
    for digest in &digests {
        let _ = rig.core.attach(digest, "prep").unwrap();
    }
    for (digest, name) in [(busy, "big.bin"), (other, "f")] {
        let at = file_of(&rig.core, digest, name);
        rig.core.release(open_cached(&rig.core, at, false).unwrap());
    }
    assert!(rig.core.detach("prep").is_some());
    let holder = open_locked(&rig);
    let waiting = open_locked(&rig);
    assert_eq!(waiting.mode(), Mode::ReadOnly);
    for (digest, blob) in [(busy, sha(&big())), (other, sha(b"the other tree"))] {
        let _ = holder.pin(MOUNT, digest, &[blob]).unwrap();
    }
    // The holder writes its pins as it gives the lock up, and the cache that takes it reads them.
    let halt = Arc::new(Notify::new());
    halt.notify_one();
    rig.work
        .handle()
        .block_on(holder.hold(Duration::from_secs(1), halt));
    drop(holder);
    let runtime = rig.dir.path().join("run-gated");
    std::fs::create_dir_all(&runtime).unwrap();
    let asks = Arc::new(InProcess::new(
        (waiting.clone(), None),
        rig.work.handle(),
        LONG,
        Arc::new(|_| {}),
    ));
    let core = Arc::new(Core::new(
        rig.core.settings().clone(),
        asks,
        TreeTable::create(&runtime).unwrap(),
        &runtime,
    ));
    let gate = Gate { cache: waiting };
    assembled(rig, core, Some(gate), digests)
}

/// The bytes of `big.bin` of the sample tree.
fn big() -> Vec<u8> {
    (0..300_000u32).map(|i| (i % 251) as u8).collect()
}

/// Asserts, while an unpin of a tree is in flight (`waiting` requests wait for the write role),
/// what no other request waits for: the lifecycle lock is free, an open of a blob of the busy tree
/// asks to pin it beside the unpin (its pin waits for the role too, and no lock of the unpin's
/// tree holds it up), and a lookup that attaches a new tree completes. The open is given back, to
/// be answered once the role is given.
fn nothing_waits<'scope, 'env>(
    t: &'env Trees,
    scope: &'scope std::thread::Scope<'scope, 'env>,
    waiting: usize,
) -> std::thread::ScopedJoinHandle<'scope, Result<u64, Errno>> {
    let core = &t.core;
    assert!(
        core.lifecycle.try_lock().is_ok(),
        "the lifecycle lock is held across the unpin"
    );
    let open = scope.spawn(move || open_cached(core, t.busy_file, false));
    until(|| t.gate().watching() > waiting);
    std::thread::scope(|inner| {
        let attach = inner.spawn(|| core.lookup(ino::TREES, &t.fresh));
        until(|| attach.is_finished());
        assert!(matches!(attach.join().unwrap(), Ok(Looked::Found(_))));
    });
    open
}

/// Asserts that while the sweep of idle trees has its unpin of a tree in flight, the lifecycle lock
/// is free, an open of a blob of another tree asks to pin it, and a lookup that attaches a tree
/// implicitly completes; and that the sweep then ends with the tree unpinned and the busy tree's
/// blob still pinned.
#[test]
fn an_idle_detach_holds_no_lock_but_its_trees_while_its_unpin_is_in_flight() {
    let t = gated("tfs-life-idle");
    let core = &t.core;
    assert!(core.lookup(ino::TREES, &t.other).is_ok());
    t.rig.clock.advance(TREE_IDLE);
    let waiting = t.gate().watching();
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.gate().release());
        let sweep = scope.spawn(|| core.detach_idle());
        until(|| t.gate().watching() > waiting);
        let open = nothing_waits(&t, scope, waiting + 1);
        t.gate().give();
        assert_eq!(sweep.join().unwrap(), 1);
        core.release(open.join().unwrap().unwrap());
    });
    assert_eq!(t.pinned(), BIG);
}

/// Asserts the same for the detach of a run: its unpin in flight holds up neither the lifecycle
/// lock, an open that pins in another tree, nor a lookup that attaches a tree.
#[test]
fn a_detach_holds_no_lock_but_its_trees_while_its_unpin_is_in_flight() {
    let t = gated("tfs-life-detach");
    let core = &t.core;
    let _ = core.attach(&t.other, "run-2").unwrap();
    let waiting = t.gate().watching();
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.gate().release());
        let detach = scope.spawn(|| core.detach("run-2"));
        until(|| t.gate().watching() > waiting);
        let open = nothing_waits(&t, scope, waiting + 1);
        t.gate().give();
        assert_eq!(detach.join().unwrap().unwrap().len(), 0);
        core.release(open.join().unwrap().unwrap());
    });
    assert_eq!(t.pinned(), BIG);
}

/// Asserts that a hold waits for the unpin of a detach decided before it, which runs with the
/// lifecycle lock released: the hold returns only after that unpin was let through, so no unpin
/// of this process follows the pins of a successor.
#[test]
fn a_hold_waits_for_the_unpin_of_a_detach_decided_before_it() {
    let t = gated("tfs-life-hold");
    let core = &t.core;
    let _ = core.attach(&t.other, "run-2").unwrap();
    let waiting = t.gate().watching();
    let let_through = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.gate().release());
        let detach = scope.spawn(|| core.detach("run-2"));
        until(|| t.gate().watching() > waiting);
        let hold = scope.spawn(|| {
            core.hold(true);
            let_through.load(Ordering::SeqCst)
        });
        until(|| core.is_held());
        let_through.store(true, Ordering::SeqCst);
        t.gate().give();
        assert!(
            hold.join().unwrap(),
            "the hold returned before the unpin was let through"
        );
        assert!(detach.join().unwrap().is_some());
    });
    core.hold(false);
    assert_eq!(t.pinned(), BIG);
}

/// Asserts that a hold that comes while the sweep of idle trees has its unpin in flight returns
/// once that unpin is let through, and the sweep ends too: the sweep's drop of idle trees, which
/// takes the lifecycle lock, does not wait for the hold that holds it while the hold waits for
/// the sweep's unpins. Then, the hold let go, a lookup that attaches a tree is answered, as a job's
/// is after a hand-over that did not complete. The two run on threads of their own, not scoped,
/// so that a wait of each for the other fails the test and does not hang it.
#[test]
fn a_hold_during_the_idle_sweeps_unpin_returns_and_the_sweep_ends() {
    let t = gated("tfs-life-hold-idle");
    let core = &t.core;
    assert!(core.lookup(ino::TREES, &t.other).is_ok());
    t.rig.clock.advance(TREE_IDLE);
    let waiting = t.gate().watching();
    let _go = OnDrop(|| t.gate().release());
    let sweeping = Arc::clone(core);
    let sweep = std::thread::spawn(move || sweeping.detach_idle());
    until(|| t.gate().watching() > waiting);
    let holding = Arc::clone(core);
    let hold = std::thread::spawn(move || holding.hold(true));
    // The hold has the lifecycle lock, and waits for the sweep's unpin.
    until(|| core.is_held());
    t.gate().give();
    until(|| hold.is_finished() && sweep.is_finished());
    hold.join().unwrap();
    assert_eq!(sweep.join().unwrap(), 1);
    assert_eq!(t.pinned(), BIG);
    core.hold(false);
    assert!(matches!(
        core.lookup(ino::TREES, &t.fresh),
        Ok(Looked::Found(_))
    ));
}

/// Asserts that a detach takes the lock of the pins of each tree it frees and of no other: a run
/// that attached nothing takes none, and a tree that another run still has takes none.
#[test]
fn a_detach_takes_the_pin_lock_of_each_tree_it_frees_and_of_no_other() {
    let t = trees("tfs-life-locks");
    let core = &t.core;
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
/// it: the unpin is not sent while the pin is in flight (it waits for the lock of the tree's
/// pins, so only the pin waits for the write role), and is made once the pin is answered, so the
/// blob is not left pinned.
#[test]
fn a_pin_in_flight_when_a_detach_starts_is_unpinned_after_it() {
    let t = gated("tfs-life-order");
    let core = &t.core;
    let waiting = t.gate().watching();
    std::thread::scope(|scope| {
        let _go = OnDrop(|| t.gate().release());
        let open = scope.spawn(|| open_cached(core, t.busy_file, false));
        until(|| t.gate().watching() > waiting);
        let digest = TreeDigest::from_hex(&t.busy).unwrap();
        assert!(
            !core.pins.free(&digest),
            "an unpin of the tree could start while its pin is in flight"
        );
        let detach = scope.spawn(|| core.detach(RUN));
        until(|| core.pins.unpins_asked() == 1);
        assert_eq!(
            t.gate().watching(),
            waiting + 1,
            "the unpin was sent while a pin was in flight"
        );
        t.gate().give();
        core.release(open.join().unwrap().unwrap());
        assert!(detach.join().unwrap().is_some());
    });
    assert_eq!(t.pinned(), 0, "the pin followed the unpin");
}

/// Asserts that a tree attached again between the decision of a detach and its unpin is not
/// unpinned: the blob it had pinned stays pinned in the cache, and recorded.
#[test]
fn a_tree_attached_again_before_its_unpin_is_not_unpinned() {
    let t = trees("tfs-life-again");
    let core = &t.core;
    core.release(open_cached(core, t.busy_file, false).unwrap());
    let loaded = core.tree(1).unwrap();
    assert_eq!(loaded.pinned.len(), 1);
    assert_eq!(t.pinned(), BIG);
    let digest = TreeDigest::from_hex(&t.busy).unwrap();
    // The detach has decided and waits for the lock of the tree's pins, which this holds.
    std::thread::scope(|scope| {
        let held = core.pins.unpin(&digest);
        let detach = scope.spawn(|| core.detach(RUN));
        until(|| core.pins.unpins_asked() == 2);
        assert!(core.attachments().is_empty());
        let _ = core.attach(&t.busy, "run-2").unwrap();
        drop(held);
        assert!(detach.join().unwrap().is_some());
    });
    assert_eq!(t.pinned(), BIG);
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

/// Asserts that an attach refused while a successor takes over reads nothing: the upstream is not
/// asked for a manifest.
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

/// The names of the fs locations of the placement of the tests that look at placements: eight, so
/// that two are in different sets of pins whichever the placement's id is.
const PATHS: [&str; 8] = [
    "jobs", "jobs2", "jobs3", "jobs4", "jobs5", "jobs6", "jobs7", "jobs8",
];

/// A placement of the bench's tree at every one of [`PATHS`], and two of them whose trees are in
/// different sets of pins.
fn placed_paths(live: &Live) -> (Created, [&'static str; 2]) {
    let created = live.place(PATHS.iter().map(|path| fs_at(path)).collect());
    let id = created.placement.to_string();
    let set = |path: &str| PinLocks::set_of(&key_of(&id, path).unwrap());
    let first = PATHS[0];
    let second = PATHS
        .iter()
        .copied()
        .find(|path| set(path) != set(first))
        .expect("two paths in different sets of pins");
    (created, [first, second])
}

/// The runs of the mount that hold the placement `created`.
fn held(live: &Live, created: &Created) -> usize {
    live.bench
        .store
        .placement(&created.placement)
        .expect("a placement")
        .attachments
}

/// A cache that is read-only (another instance holds the lock of its directory) until it ticks.
fn read_only(bench: &Bench) -> LocalCache {
    let _holder = bench.rig.locked();
    bench.rig.locked()
}

/// Asserts that while the lookups of two placed paths wait for the node's answer to where their
/// paths are, the lifecycle lock is free and the round trips of the two are in flight together,
/// neither holding up the other; once the cache takes the write role both are answered, and the
/// node holds the placement for the mount.
#[tokio::test(flavor = "multi_thread")]
async fn the_hold_settled_for_one_placed_path_does_not_hold_up_another_path() {
    let live = Live::in_process_with("tfs-life-place", read_only, LONG).await;
    let (created, paths) = placed_paths(&live);
    let watching = live.cache.mode_watchers();
    let lookups = paths.map(|path| {
        let core = Arc::clone(&live.core);
        tokio::task::spawn_blocking(move || core.lookup(ino::ROOT, path))
    });
    eventually("both lookups wait for the write role", || {
        live.cache.mode_watchers() >= watching + 2
    })
    .await;
    assert!(
        live.core.lifecycle.try_lock().is_ok(),
        "the lifecycle lock is held across the round trips"
    );
    assert_eq!(held(&live, &created), 0);
    assert_eq!(live.cache.tick().unwrap(), Tick::Took);
    for lookup in lookups {
        assert!(matches!(lookup.await.unwrap(), Ok(Looked::Found(_))));
    }
    assert_eq!(held(&live, &created), 1);
}

/// Asserts that the settling of the hold of a path waits for an unpin of its own tree, which holds
/// the lock of the tree's pins: it is not made while that lock is held, and it makes the node's
/// hold again (which the test ended meanwhile) once the lock is let go.
#[tokio::test(flavor = "multi_thread")]
async fn the_hold_of_a_path_is_settled_under_the_lock_of_its_tree() {
    let live = Live::new("tfs-life-settle").await;
    let (created, [path, _]) = placed_paths(&live);
    let key = key_of(&created.placement.to_string(), path).unwrap();
    let (store, id) = (live.bench.store.clone(), created.placement.clone());
    live.on_core(move |core| {
        std::thread::scope(|scope| {
            let lock = core.pins.unpin(&key);
            let lookup = scope.spawn(|| core.lookup(ino::ROOT, path));
            until(|| core.pins.unpins_asked() == 2);
            // Asked: the first `place` and the attach are done, and the settling waits.
            assert!(!lookup.is_finished());
            assert!(store.detach(&id, MOUNT));
            drop(lock);
            assert!(matches!(lookup.join().unwrap(), Ok(Looked::Found(_))));
        });
        assert_eq!(store.placement(&id).unwrap().attachments, 1);
    })
    .await;
}

/// Asserts that a hold waits for the settling of the hold of a placed path in progress (waiting
/// for the lock of its tree's pins), and that a lookup after the hold settles nothing: the
/// successor does.
#[tokio::test(flavor = "multi_thread")]
async fn a_hold_waits_for_a_settling_in_progress_and_none_is_settled_after_it() {
    let live = Live::new("tfs-life-hold-place").await;
    let (created, [first, second]) = placed_paths(&live);
    let key = key_of(&created.placement.to_string(), first).unwrap();
    let (store, id) = (live.bench.store.clone(), created.placement.clone());
    live.on_core(move |core| {
        std::thread::scope(|scope| {
            let lock = core.pins.unpin(&key);
            let lookup = scope.spawn(|| core.lookup(ino::ROOT, first));
            // The settling is in progress: it has taken its place in the hold's wait, and waits
            // for the lock.
            until(|| core.pins.unpins_asked() == 2);
            assert!(store.detach(&id, MOUNT));
            let hold = scope.spawn(|| core.hold(true));
            until(|| core.is_held());
            assert!(
                !hold.is_finished(),
                "the hold returned with a settling in flight"
            );
            drop(lock);
            hold.join().unwrap();
            assert!(matches!(lookup.join().unwrap(), Ok(Looked::Found(_))));
        });
        // The settling made the hold again before the hold returned.
        assert_eq!(store.placement(&id).unwrap().attachments, 1);
        // After the hold, a lookup settles nothing: it takes no lock of a tree's pins, and the
        // node's hold is the one its own `place` made.
        assert!(store.detach(&id, MOUNT));
        let settles = core.pins.unpins_asked();
        let _ = core.lookup(ino::ROOT, second);
        assert_eq!(core.pins.unpins_asked(), settles);
        assert_eq!(store.placement(&id).unwrap().attachments, 1);
        core.hold(false);
    })
    .await;
}

/// Asserts that the `place` of a settling is made under the lock of its placement, which every
/// location of the placement shares: it waits while an `unplace` decided for another location
/// holds the lock.
#[tokio::test(flavor = "multi_thread")]
async fn the_hold_of_a_placement_is_placed_under_the_lock_of_the_placement() {
    let live = Live::new("tfs-life-placement").await;
    let (created, [first, second]) = placed_paths(&live);
    let other = key_of(&created.placement.to_string(), second).unwrap();
    let (store, id) = (live.bench.store.clone(), created.placement.clone());
    live.on_core(move |core| {
        std::thread::scope(|scope| {
            let lock = core.pins.unplace(&other);
            let lookup = scope.spawn(|| core.lookup(ino::ROOT, first));
            until(|| core.pins.placements_asked() == 1);
            // Asked: the first `place` and the attach are done, and the settling waits.
            assert!(!lookup.is_finished());
            assert!(store.detach(&id, MOUNT));
            drop(lock);
            assert!(matches!(lookup.join().unwrap(), Ok(Looked::Found(_))));
        });
        assert_eq!(store.placement(&id).unwrap().attachments, 1);
    })
    .await;
}

/// Asserts that the attachments file is written with the state lock free, at an attach, a detach
/// and a restore, and holds what the attachments were.
#[test]
fn the_attachments_file_is_written_with_the_state_lock_free() {
    let t = trees("tfs-life-file");
    let core = &t.core;
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
    let core = &t.core;
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
/// the blob pinned when it has it, asks the cache for no pin of its own: nothing is pinned.
#[test]
fn an_open_that_waited_for_an_unpin_finds_the_blob_pinned_and_pins_nothing() {
    let t = trees("tfs-life-waited");
    let core = &t.core;
    let digest = TreeDigest::from_hex(&t.busy).unwrap();
    let loaded = core.tree(1).unwrap();
    std::thread::scope(|scope| {
        let held = core.pins.unpin(&digest);
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
    assert_eq!(t.pinned(), 0);
}

/// Asserts that a restore of a tree that is in memory already keeps that tree, and attaches it.
#[test]
fn a_restore_of_a_tree_in_memory_keeps_it() {
    let t = trees("tfs-life-restore");
    let core = &t.core;
    let before = core.tree(1).unwrap();
    let busy = TreeDigest::from_hex(&t.busy).unwrap();
    core.restore(vec![(busy, "run-4".to_owned())], 1);
    assert!(Arc::ptr_eq(&core.tree(1).unwrap(), &before));
    assert_eq!(core.attachments().len(), 2);
}
