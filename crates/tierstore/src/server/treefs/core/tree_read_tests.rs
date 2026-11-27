//! The bound of one read of a tree's manifests ([`TREE_READ`]): a lookup of `trees/<digest>`
//! whose fills the node's cache does not answer is answered `EIO`, however many manifests the tree
//! has; the fills of one tree share the one bound on the core's clock, which only the test moves; and
//! a cache that takes the write role within the wait still serves the read.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{Core, Errno, Looked, Settings};
use super::TREE_READ;
use crate::server::local_cache::{CacheConfig, LocalCache, Mode, Tick};
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::ino;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{
    Built, OWNER, Rig, TREE_IDLE, TREE_LIST_RAM, dir, file, sample, until,
};
use crate::{Backend, Clock, Mono, TestClock};

/// The milliseconds of the bound.
fn bound_ms() -> u64 {
    u64::try_from(TREE_READ.as_millis()).unwrap()
}

/// A core over the rig's cache directory and clock, with its own table of tree numbers, that calls
/// the cache through `asks`.
fn core_calling(rig: &Rig, asks: Arc<InProcess>) -> Arc<Core> {
    let runtime = rig.dir.path().join("run-bound");
    std::fs::create_dir_all(&runtime).unwrap();
    Arc::new(Core::new(
        Settings {
            owner: OWNER,
            cache_path: rig.cache.clone(),
            mountpoint: rig.mountpoint.clone(),
            mount_id: "mount-bound".to_owned(),
            generation: 1,
            count_lookups: true,
            tree_idle: TREE_IDLE,
            tree_list_ram_bytes: TREE_LIST_RAM,
            clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        },
        asks,
        TreeTable::create(&runtime).unwrap(),
        &runtime,
    ))
}

/// A core over the rig's node cache and clock. Each pause the core takes between its tries moves
/// the rig's clock on by the pause, and `paused` is told how many such pauses were taken so far.
pub(super) fn core_over(rig: &Rig, paused: impl Fn(u32) + Send + Sync + 'static) -> Arc<Core> {
    let clock: Arc<Mono> = Arc::clone(&rig.clock);
    let count = Mutex::new(0u32);
    let pause = Arc::new(move |pause: Duration| {
        clock.advance(pause);
        let mut count = count.lock().unwrap();
        *count += 1;
        paused(*count);
    });
    core_calling(rig, rig.asks_pausing(pause))
}

/// Two caches over the rig's cache directory, whose upstream is the rig's: the first holds the
/// lock of the directory, and so the write role; the second is read-only until the first is
/// dropped and the second ticks.
fn read_only_pair(rig: &Rig) -> (LocalCache, LocalCache) {
    let handle = rig.work.handle();
    let _entered = handle.enter();
    let open = || {
        let mut config =
            CacheConfig::new(rig.cache.clone(), 1 << 30, vec![("cas/".to_owned(), true)]);
        crate::server::local_cache::testing::never_expires(&mut config);
        let backend: Arc<dyn Backend> = rig.upstream.clone();
        LocalCache::open_locked(
            config,
            backend,
            Arc::new(crate::server::local_cache::testing::FakeDisk::new()),
            Arc::new(TestClock::new()),
        )
        .unwrap()
    };
    let holder = open();
    let waiting = open();
    assert_eq!(holder.mode(), Mode::ReadWrite);
    assert_eq!(waiting.mode(), Mode::ReadOnly);
    (holder, waiting)
}

/// Asserts that a lookup of a tree none of whose manifests the cache holds, while the cache stays
/// read-only (another process holds the lock of its directory) for the whole of the wait, is `EIO`:
/// nothing is filled, and no tree is numbered or attached.
#[test]
fn a_lookup_with_the_cache_read_only_for_the_whole_wait_is_answered_eio() {
    let rig = Rig::new("tfs-bound-readonly", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let (_holder, waiting) = read_only_pair(&rig);
    let asks = Arc::new(InProcess::new(
        (waiting, None),
        rig.work.handle(),
        Duration::from_millis(1),
        Arc::new(|_| {}),
    ));
    let core = core_calling(&rig, asks);
    assert_eq!(core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert!(rig.upstream.asked("fill").is_empty());
    assert_eq!(rig.clock.now(), 0);
    assert!(bound_ms() < 60_000);
    assert!(core.implicit().is_empty());
}

/// Asserts that the fills of one tree share the one bound: in a tree four levels deep, whose root
/// the cache holds, read a level at a time with one manifest at each, and each fill answered 20 s
/// later on the clock, the fills of the first two levels below the root are answered within 30 s of
/// the start (the second with 10 s left) and the third is not asked, so the lookup is `EIO`, where
/// a bound for each fill would have read the whole tree in 60 s.
#[test]
fn the_manifests_of_one_tree_share_one_bound() {
    let deep = Built::of(vec![(
        "a",
        dir(vec![("b", dir(vec![("c", dir(vec![("d", file(b"x"))]))]))]),
    )]);
    let rig = Rig::new("tfs-bound-shared", deep);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    assert_eq!(rig.upstream.built.lock().unwrap().manifests.len(), 4);
    // The root is in the cache before the lookup, so that its fills are those of the levels below.
    rig.upstream.gate_manifests();
    rig.upstream.permit_manifests(1);
    rig.work
        .handle()
        .block_on(rig.node.fill_manifest(&root))
        .unwrap();
    let core = core_over(&rig, |_| {});
    let step = Duration::from_secs(20);
    let looked = std::thread::scope(|scope| {
        let lookup = scope.spawn(|| core.lookup(ino::TREES, &root));
        rig.upstream.wait_for_manifest_fills(2);
        rig.clock.advance(step);
        rig.upstream.permit_manifests(1);
        rig.upstream.wait_for_manifest_fills(3);
        rig.clock.advance(step);
        rig.upstream.permit_manifests(1);
        lookup.join().unwrap()
    });
    assert_eq!(looked, Err(Errno::Io));
    let lines = rig.upstream.asked("fill");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], format!("fill {root}"));
    assert_eq!(rig.clock.now(), 40_000);
    assert!(core.implicit().is_empty());
}

/// Asserts that a cache that takes the write role within the wait (the process that held the lock
/// of its directory ended) serves the read: the lookup, which waits for the role, resolves the tree
/// once the cache has it, and nothing was filled while it was read-only.
#[test]
fn a_cache_that_takes_the_write_role_within_the_wait_serves_the_lookup() {
    let rig = Rig::new("tfs-bound-back", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let (holder, waiting) = read_only_pair(&rig);
    let watching = waiting.mode_watchers();
    let asks = Arc::new(InProcess::new(
        (waiting.clone(), None),
        rig.work.handle(),
        Duration::from_secs(30),
        Arc::new(|_| {}),
    ));
    let core = core_calling(&rig, asks);
    let found = std::thread::scope(|scope| {
        let lookup = scope.spawn(|| core.lookup(ino::TREES, &root));
        until(|| waiting.mode_watchers() > watching);
        assert!(!lookup.is_finished());
        assert!(rig.upstream.asked("fill").is_empty());
        drop(holder);
        assert_eq!(waiting.tick().unwrap(), Tick::Took);
        lookup.join().unwrap()
    });
    let Looked::Found(entry) = found.unwrap() else {
        panic!("not found");
    };
    assert_eq!(entry.attr.ino, 1 << 24);
    assert_eq!(rig.clock.now(), 0);
    assert_eq!(core.implicit(), vec![1]);
}
