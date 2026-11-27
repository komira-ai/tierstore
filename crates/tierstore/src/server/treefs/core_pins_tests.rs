//! The pins of the opens of a core that calls the cache in process: the opens of different blobs
//! are each inside their pin at once, an open of a blob pinned already waits for none of them, and
//! none of it writes to the runtime directory. A pin waits for the cache to be read-write, so a
//! cache that has lost the write role holds the pins in flight until it takes it back.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::{blob_of, fill_blob};
use crate::server::local_cache::testing::{FakeDisk, never_expires};
use crate::server::local_cache::{CacheConfig, LocalCache, Mode, Tick};
use crate::server::treefs::core::Core;
use crate::server::treefs::in_process::InProcess;
use crate::server::treefs::ino;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{Rig, open_cached, sample, until};
use crate::{Backend, TestClock};

/// A rig over the sample tree whose cache directory holds the blobs of `lens` bytes, filled by the
/// node's cache, and a core with the tree attached that calls a cache of its own over that directory,
/// which holds the lock of it: its runtime directory, and the cache.
fn locked_core(name: &str, lens: &[usize]) -> (Rig, Arc<Core>, PathBuf, LocalCache) {
    let rig = Rig::new(name, sample());
    for len in lens {
        fill_blob(&rig, &blob_of(&rig, *len));
    }
    let mut config = CacheConfig::new(rig.cache.clone(), 1 << 30, vec![("cas/".to_owned(), true)]);
    never_expires(&mut config);
    let backend: Arc<dyn Backend> = rig.upstream.clone();
    let handle = rig.work.handle();
    let cache = {
        let _entered = handle.enter();
        LocalCache::open_locked(
            config,
            backend,
            Arc::new(FakeDisk::new()),
            Arc::new(TestClock::new()),
        )
        .unwrap()
    };
    assert_eq!(cache.mode(), Mode::ReadWrite);
    let runtime = rig.dir.path().join("run-locked");
    std::fs::create_dir_all(&runtime).unwrap();
    let asks = Arc::new(InProcess::new(
        (cache.clone(), None),
        handle,
        Duration::from_secs(30),
        Arc::new(|_| {}),
    ));
    let core = Arc::new(Core::new(
        rig.core.settings().clone(),
        asks,
        TreeTable::create(&runtime).unwrap(),
        &runtime,
    ));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = core.attach(&root, "run-1").unwrap();
    (rig, core, runtime, cache)
}

/// Takes the write role from `cache`: the lock file of its directory is removed and its tick finds
/// it lost. Its requests that wait for the write role then wait until [`take_the_write_role`].
fn lose_the_write_role(rig: &Rig, cache: &LocalCache) {
    std::fs::remove_file(rig.cache.join(".lock")).unwrap();
    assert_eq!(cache.tick().unwrap(), Tick::Lost);
    assert_eq!(cache.mode(), Mode::ReadOnly);
}

/// Gives `cache` the write role again.
fn take_the_write_role(cache: &LocalCache) {
    assert_eq!(cache.tick().unwrap(), Tick::Took);
    assert_eq!(cache.mode(), Mode::ReadWrite);
}

/// Every name in `runtime` with its inode and modification time, and the directory's own
/// modification time: a file written, or replaced by a rename, changes it.
fn runtime_state(runtime: &Path) -> (Vec<(String, u64, SystemTime)>, SystemTime) {
    let mut names: Vec<_> = std::fs::read_dir(runtime)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            let name = entry.file_name().into_string().unwrap();
            (name, meta.ino(), meta.modified().unwrap())
        })
        .collect();
    names.sort();
    let dir = std::fs::metadata(runtime).unwrap().modified().unwrap();
    (names, dir)
}

/// Asserts that the first opens of two blobs of an attached tree have their pins in flight in the
/// cache at once (both wait for its write role), and that a later open of a pinned blob asks
/// nothing while those pins are unanswered: it is answered with no pin asked, and is no wait of the
/// others. Once the cache takes the role back, both pins are made. Catches pins that hold one lock
/// of the core across their round trip, which makes every open of the mount wait for each other
/// open's pin in turn.
#[test]
fn the_pins_of_two_opens_are_asked_at_once_and_an_open_of_a_pinned_blob_waits_for_neither() {
    let (rig, core, _, cache) = locked_core("tfs-core-pins-at-once", &[3000, 10, 6]);
    let pinned = open_cached(&core, ino::of(1, 3), false).unwrap();
    assert_eq!(cache.pinned_bytes(), 3000);
    assert_eq!(core.pins.pins_asked(), 1);
    lose_the_write_role(&rig, &cache);
    let watching = cache.mode_watchers();
    let (again, handles) = std::thread::scope(|scope| {
        let opening: Vec<_> = [4, 7]
            .into_iter()
            .map(|index| {
                let core = Arc::clone(&core);
                scope.spawn(move || open_cached(&core, ino::of(1, index), false).unwrap())
            })
            .collect();
        until(|| cache.mode_watchers() == watching + 2);
        let again = open_cached(&core, ino::of(1, 3), false).unwrap();
        assert_eq!(core.pins.pins_asked(), 3);
        assert_eq!(cache.mode_watchers(), watching + 2);
        assert_eq!(cache.pinned_bytes(), 3000);
        take_the_write_role(&cache);
        let handles: Vec<u64> = opening
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        (again, handles)
    });
    assert_eq!(cache.pinned_bytes(), 3016);
    for fh in handles.into_iter().chain([pinned, again]) {
        core.release(fh);
    }
}

/// Asserts that the first opens of four different blobs, from four threads, are all inside their
/// pin at once, past their fill, with none finished (a lock shared by the opens and held to their
/// end would let one through at a time); and that those opens and their last releases, also from
/// four threads, write nothing to the runtime directory: no table of opens is rewritten at a first
/// open or a last release under a lock every open shares.
#[test]
fn first_opens_and_last_releases_of_different_blobs_run_at_once_and_write_nothing() {
    let (rig, core, runtime, cache) =
        locked_core("tfs-core-opens-at-once", &[300_000, 3000, 10, 6]);
    let before = runtime_state(&runtime);
    lose_the_write_role(&rig, &cache);
    let watching = cache.mode_watchers();
    let blobs = [1, 3, 4, 7];
    let handles: Vec<u64> = std::thread::scope(|scope| {
        let opening: Vec<_> = blobs
            .into_iter()
            .map(|index| {
                let core = Arc::clone(&core);
                scope.spawn(move || open_cached(&core, ino::of(1, index), false).unwrap())
            })
            .collect();
        until(|| cache.mode_watchers() == watching + blobs.len());
        assert_eq!(core.open_files(), 0);
        take_the_write_role(&cache);
        opening
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect()
    });
    assert_eq!(core.open_files(), blobs.len());
    assert_eq!(cache.pinned_bytes(), 303_016);
    let releasing: Vec<_> = handles
        .into_iter()
        .map(|fh| {
            let core = Arc::clone(&core);
            std::thread::spawn(move || core.release(fh))
        })
        .collect();
    for thread in releasing {
        thread.join().unwrap();
    }
    assert_eq!(core.open_files(), 0);
    assert_eq!(runtime_state(&runtime), before);
    assert!(!runtime.join("backing").exists());
}
