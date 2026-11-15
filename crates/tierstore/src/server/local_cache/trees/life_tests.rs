//! What keeps a placement and what ends one: leases on the injected clock, attachments, open
//! reads, the parent of a tree, release, the sweep and its loop, and the pins of all of it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::super::PinAnswer;
use super::super::testing::{Rig, digest, pattern};
use super::files::placement_file;
use super::testing::{Bench, built, fs_at, s3_at, tree_path};
use super::{
    CreateRequest, LifeError, PlacementId, ReadError, SweepReport, TreeStore, TreesConfig,
};
use crate::{NewEntry, NewKind, TreeDigest};

const HOUR: u64 = 3600 * 1000;

async fn placed(name: &str) -> (Bench, PlacementId) {
    let bench = Bench::new(name).await;
    bench.fill_all().await;
    let made = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1"), s3_at("jobs/1/")]))
        .unwrap();
    (bench, made.placement)
}

fn unknown() -> PlacementId {
    PlacementId::parse(&"cd".repeat(16)).unwrap()
}

/// Asserts that a lease holds a placement until its time on the injected clock, that a lookup
/// stops finding it once the lease is over even before anything is swept, that the sweep then
/// takes the placement and its tree, its file, its directory and its pins, and that a sweep that
/// finds nothing to take says so.
#[tokio::test]
async fn a_lease_holds_a_placement_until_its_time_and_the_sweep_takes_it() {
    let (bench, id) = placed("trees-lease").await;
    let store = &bench.store;
    assert_eq!(store.sweep(), SweepReport::default());
    bench.rig.clock.advance(4 * HOUR - 1);
    assert!(store.locate_fs("jobs/1").is_some());
    assert!(store.resolve(&id, "").is_ok());
    bench.rig.clock.advance(1);
    assert!(
        store.locate_fs("jobs/1").is_none() && store.locate_s3("build-cas", "jobs/1/a").is_none()
    );
    assert_eq!(store.resolve(&id, ""), Err(ReadError::NoPlacement));
    assert!(store.placement(&id).is_none());
    assert_eq!(store.attach(&id, "run"), Err(LifeError::UnknownPlacement));
    assert_eq!(store.sizes(), (1, 1));
    assert_eq!(
        store.sweep(),
        SweepReport {
            placements: 1,
            trees: 1
        }
    );
    assert_eq!(store.sizes(), (0, 0));
    assert!(!placement_file(&bench.rig.config.path, &id).exists());
    assert!(!tree_path(&bench.rig, &bench.built().root).exists());
    assert_eq!(bench.rig.cache.pinned_bytes(), 0);
    assert_eq!(store.sweep(), SweepReport::default());
}

/// Asserts that an attachment keeps a placement past its lease (found by location and resolvable),
/// that two runs hold it until both detach, that a run that was never attached detaches nothing,
/// and that the last detach takes the placement and its tree.
#[tokio::test]
async fn ttl_attachment_and_parent_references_keep_a_tree() {
    let (bench, id) = placed("trees-references").await;
    let store = &bench.store;
    store.attach(&id, "run-1").unwrap();
    store.attach(&id, "run-2").unwrap();
    store.attach(&id, "run-2").unwrap();
    assert_eq!(store.placement(&id).unwrap().attachments, 2);
    bench.rig.clock.advance(5 * HOUR);
    assert_eq!(store.sweep(), SweepReport::default());
    assert!(store.locate_fs("jobs/1/bin/tool").is_some());
    assert!(store.resolve(&id, "bin/tool").is_ok());
    assert!(!store.detach(&id, "run-3"));
    assert!(store.detach(&id, "run-1"));
    assert!(!store.detach(&id, "run-1"));
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.detach(&id, "run-2"));
    assert_eq!(store.sizes(), (0, 0));
    assert!(!store.detach(&id, "run-2"));

    // A tree held by a parent stays when its own placement ends, and goes with the parent's.
    let base = store.create(bench.request(vec![fs_at("base")])).unwrap();
    let parent = store
        .create(CreateRequest {
            built: built(vec![NewEntry {
                path: "tc".to_owned(),
                kind: NewKind::Tree { digest: base.tree },
            }]),
            at: vec![fs_at("jobs/2")],
            ttl: Some(Duration::from_secs(2 * 3600)),
            ready: false,
        })
        .unwrap();
    bench.rig.clock.advance(HOUR * 3);
    // The base's lease (4h) is not over; the parent's (2h) is, and its tree goes with it.
    assert_eq!(
        store.sweep(),
        SweepReport {
            placements: 1,
            trees: 1
        }
    );
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.resolve(&parent.placement, "").is_err());
    let again = store
        .create(CreateRequest {
            built: built(vec![NewEntry {
                path: "tc".to_owned(),
                kind: NewKind::Tree { digest: base.tree },
            }]),
            at: vec![fs_at("jobs/3")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    bench.rig.clock.advance(2 * HOUR);
    // Now the base's own lease is over and the parent's is not: the base is kept for it.
    assert_eq!(
        store.sweep(),
        SweepReport {
            placements: 1,
            trees: 0
        }
    );
    assert_eq!(store.sizes(), (1, 2));
    assert!(store.resolve(&again.placement, "tc/bin/tool").is_ok());
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);
    bench.rig.clock.advance(3 * HOUR);
    assert_eq!(
        store.sweep(),
        SweepReport {
            placements: 1,
            trees: 2
        }
    );
    assert_eq!(store.sizes(), (0, 0));
    assert_eq!(bench.rig.cache.pinned_bytes(), 0);
}

/// Asserts that a read open on a placement holds it past its lease, that opening one of a
/// placement that is not there is refused, and that the placement goes when the last read ends.
#[tokio::test]
async fn an_open_read_holds_a_placement_until_it_is_dropped() {
    let (bench, id) = placed("trees-read-hold").await;
    let store = &bench.store;
    let first = store.open_read(&id).unwrap();
    let second = store.open_read(&id).unwrap();
    assert_eq!(format!("{first:?}"), format!("ReadHold({id})"));
    assert_eq!(store.placement(&id).unwrap().reads, 2);
    bench.rig.clock.advance(9 * HOUR);
    assert_eq!(store.sweep(), SweepReport::default());
    assert!(store.resolve(&id, "bin/tool").is_ok());
    drop(first);
    assert_eq!(store.sizes(), (1, 1));
    drop(second);
    assert_eq!(store.sizes(), (0, 0));
    assert_eq!(
        store.open_read(&id).unwrap_err(),
        LifeError::UnknownPlacement
    );
    assert!(
        LifeError::UnknownPlacement
            .to_string()
            .contains("placement")
    );
}

/// Asserts that release removes the locations at once (a key and a path are ordinary again and the
/// file of the placement is gone) while an attachment, or a read, still resolves the placement by
/// id and keeps its tree, that it ends with the last holder, and that a second release, or one of a
/// placement that is not there, is refused.
#[tokio::test]
async fn release_removes_the_locations_and_the_placement_lasts_for_its_holders() {
    let (bench, id) = placed("trees-release").await;
    let store = &bench.store;
    store.attach(&id, "run").unwrap();
    let hold = store.open_read(&id).unwrap();
    store.release(&id).unwrap();
    assert!(
        store.locate_fs("jobs/1").is_none() && store.locate_s3("build-cas", "jobs/1/x").is_none()
    );
    assert!(!placement_file(&bench.rig.config.path, &id).exists());
    let info = store.placement(&id).unwrap();
    assert!(info.released && info.attachments == 1 && info.reads == 1);
    assert!(store.resolve(&id, "bin/tool").is_ok());
    assert_eq!(store.release(&id), Err(LifeError::UnknownPlacement));
    assert_eq!(store.attach(&id, "other"), Err(LifeError::UnknownPlacement));
    assert_eq!(
        store.open_read(&id).unwrap_err(),
        LifeError::UnknownPlacement
    );
    assert!(store.detach(&id, "run"));
    assert_eq!(store.sizes(), (1, 1));
    drop(hold);
    assert_eq!(store.sizes(), (0, 0));
    assert_eq!(store.release(&unknown()), Err(LifeError::UnknownPlacement));
    // The place is free again.
    assert!(store.create(bench.request(vec![fs_at("jobs/1")])).is_ok());
}

/// Asserts that a placement released while a run holds it is placed again by a create of the same
/// tree and locations: the placement is new, appears again, keeps its holder and the time it was
/// first made.
#[tokio::test]
async fn a_released_placement_that_is_held_is_placed_again_with_its_holders() {
    let (bench, id) = placed("trees-revive").await;
    let store = &bench.store;
    store.attach(&id, "run").unwrap();
    store.release(&id).unwrap();
    bench.rig.clock.advance(1000);
    let again = store
        .create(bench.request(vec![fs_at("jobs/1"), s3_at("jobs/1/")]))
        .unwrap();
    assert_eq!(again.placement, id);
    assert!(again.created && !again.new_tree);
    let info = store.placement(&id).unwrap();
    assert!(!info.released);
    assert_eq!(
        (info.attachments, info.created_ms, info.expires_ms),
        (1, 0, 1000 + 4 * HOUR)
    );
    assert!(store.locate_fs("jobs/1/bin").is_some());
    // The released one gave its place to nobody in between: another tree could not take it.
    assert_eq!(store.sizes(), (1, 1));
}

/// Asserts that releasing a placement leaves its pack in the cache and unpinned, so that it is
/// evicted by the next pass that needs room: before, a pass that wants the room skips the pinned
/// object and takes another; after, the pass takes it.
#[tokio::test]
async fn release_unpins_and_leaves_packs_evictable() {
    let rig = Rig::with("trees-evictable", |config| {
        config.budget = 1000;
        config.high_water = 90;
        config.low_water = 75;
        config.critical = 100;
        config.free_floor = 0;
        config.pin_limit = 1000;
    });
    rig.disk.set_space(1000, 1000);
    let store = TreeStore::open(rig.cache.clone(), TreesConfig::default()).unwrap();
    let mut keys = Vec::new();
    let mut bodies = Vec::new();
    for n in 0..4u8 {
        let mut body = pattern(250);
        body[0] = n;
        let key = format!("cas/l/{}", digest(&body));
        rig.put(&key, &body).await;
        keys.push(key);
        bodies.push(body);
    }
    assert_eq!(rig.get(&keys[0]).await.unwrap().len(), 250);
    let tree = built(vec![NewEntry {
        path: "f".to_owned(),
        kind: NewKind::File {
            mode: None,
            sha256: TreeDigest::of(&bodies[0]),
            size: 250,
            from: None,
        },
    }]);
    let made = store
        .create(CreateRequest {
            built: tree,
            at: vec![fs_at("a")],
            ttl: None,
            ready: true,
        })
        .unwrap();
    assert_eq!(made.pins.pinned, 1);
    assert_eq!(rig.cache.pinned_bytes(), 250);
    for key in &keys[1..] {
        rig.get(key).await.unwrap();
    }
    // The pinned pack, the least recently used, survived the pass the fills ran; another went.
    assert!(rig.has_file(&keys[0]) && !rig.has_file(&keys[1]));
    store.release(&made.placement).unwrap();
    assert_eq!(rig.cache.pinned_bytes(), 0);
    assert!(
        rig.has_file(&keys[0]),
        "the pack stays in the cache after the release"
    );
    rig.get(&keys[1]).await.unwrap();
    assert!(
        !rig.has_file(&keys[0]),
        "and is the first the next pass takes"
    );
}

/// Asserts that `pin` pins what the cache holds now (a tree made before its bytes were fetched
/// pins them once they are), that `mark_ready` marks the tree and pins, is idempotent, and that
/// both refuse a tree that is not there, and `mark_ready` a cache that holds no lock.
#[tokio::test]
async fn pin_and_mark_ready_pin_what_the_cache_holds_by_then() {
    let bench = Bench::new("trees-pin").await;
    let made = bench.store.create(bench.request(vec![fs_at("a")])).unwrap();
    assert_eq!(made.pins.absent, 1);
    let nothing = bench.store.pin(&made.tree).unwrap();
    assert_eq!((nothing.pinned, nothing.absent), (0, 1));
    bench.fill_all().await;
    let pinned = bench.store.mark_ready(&made.tree).unwrap();
    assert_eq!(
        pinned,
        PinAnswer {
            pinned: 1,
            refused: 0,
            absent: 0
        }
    );
    assert!(bench.store.placement(&made.placement).unwrap().ready);
    assert!(tree_path(&bench.rig, &made.tree).join("ready").is_file());
    assert_eq!(bench.store.mark_ready(&made.tree).unwrap().pinned, 1);
    assert_eq!(bench.store.pin(&made.tree).unwrap().pinned, 1);
    let missing = TreeDigest::of(b"missing");
    assert_eq!(bench.store.pin(&missing), Err(LifeError::UnknownTree));
    assert_eq!(
        bench.store.mark_ready(&missing),
        Err(LifeError::UnknownTree)
    );
    assert!(LifeError::UnknownTree.to_string().contains("tree"));
    let holder = bench.rig.locked();
    let waiting = Bench::reopen(bench.rig.locked());
    assert_eq!(waiting.mark_ready(&made.tree), Err(LifeError::ReadOnly));
    assert_eq!(waiting.release(&unknown()), Err(LifeError::ReadOnly));
    assert!(LifeError::ReadOnly.to_string().contains("lock"));
    drop(holder);
}

/// Asserts that a store that is not allowed to change the directory (the cache holds no lock)
/// sweeps nothing and removes nothing, but still answers by time: a lease that is over is not
/// found by location.
#[tokio::test]
async fn a_store_without_the_lock_sweeps_nothing() {
    let bench = Bench::new("trees-ro-sweep").await;
    bench.fill_all().await;
    let made = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1")]))
        .unwrap();
    let holder = bench.rig.locked();
    let waiting = Bench::reopen(bench.rig.locked());
    assert_eq!(waiting.sizes(), (1, 1));
    assert!(waiting.locate_fs("jobs/1").is_some());
    assert!(waiting.resolve(&made.placement, "bin/tool").is_ok());
    bench.rig.clock.advance(5 * HOUR);
    assert_eq!(waiting.sweep(), SweepReport::default());
    assert!(waiting.locate_fs("jobs/1").is_none());
    assert_eq!(waiting.sizes(), (1, 1));
    assert!(placement_file(&bench.rig.config.path, &made.placement).is_file());
    // An attachment and a read still count, in memory, and end without touching the disk.
    assert_eq!(
        waiting.attach(&made.placement, "run"),
        Err(LifeError::UnknownPlacement)
    );
    drop(holder);
}

/// Asserts what the loop does when the cache takes the lock after the store was opened without it:
/// the store reads the directory again (a tree the holder made since is there), keeps the
/// attachment it had, and is allowed to create; and that, once it holds the lock, a step sweeps.
#[tokio::test]
async fn a_step_reads_the_directory_when_the_cache_takes_the_lock_and_then_sweeps() {
    let bench = Bench::new("trees-step").await;
    bench.fill_all().await;
    let made = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1")]))
        .unwrap();
    let holder = bench.rig.locked();
    let waiting_cache = bench.rig.locked();
    let store = Bench::reopen(waiting_cache.clone());
    store.attach(&made.placement, "run").unwrap();
    // Still without the lock: a step changes nothing, and a create is refused.
    store.step();
    assert_eq!(
        store
            .create(bench.request(vec![fs_at("jobs/2")]))
            .unwrap_err(),
        super::CreateError::ReadOnly
    );
    drop(holder);
    assert!(waiting_cache.tick().unwrap() == super::super::Tick::Took);
    // The cache has the lock, the store has not read the directory since: refused until the step.
    assert!(store.create(bench.request(vec![fs_at("jobs/2")])).is_err());
    store.step();
    assert_eq!(store.placement(&made.placement).unwrap().attachments, 1);
    let second = store.create(bench.request(vec![fs_at("jobs/2")])).unwrap();
    store.step();
    bench.rig.clock.advance(9 * HOUR);
    store.step();
    // The attached placement stays; the other, with no holder, is gone.
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.placement(&second.placement).is_none());
}

/// Asserts that the loop sweeps every interval until it is told to stop, and that a store that is
/// off makes nothing of a step.
#[tokio::test(start_paused = true)]
async fn the_loop_sweeps_on_its_interval_and_stops_when_told() {
    let bench = Bench::new("trees-loop").await;
    bench.fill_all().await;
    bench
        .store
        .create(bench.request(vec![fs_at("jobs/1")]))
        .unwrap();
    let halt = Arc::new(Notify::new());
    let looping = {
        let store = bench.store.clone();
        let halt = Arc::clone(&halt);
        tokio::spawn(async move { store.maintain(Duration::from_secs(10), halt).await })
    };
    bench.rig.clock.advance(5 * HOUR);
    for _ in 0..50 {
        tokio::time::advance(Duration::from_secs(10)).await;
        if bench.store.sizes() == (0, 0) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(bench.store.sizes(), (0, 0));
    halt.notify_one();
    looping.await.unwrap();

    let off = TreesConfig {
        enabled: false,
        ..TreesConfig::default()
    };
    let idle = Bench::with("trees-loop-off", off).await;
    idle.store.step();
    assert_eq!(idle.store.sizes(), (0, 0));
}

/// Asserts that a store that is off opens without touching the directory: no `trees` and no
/// `placements` in it, nothing read, nothing placed.
#[tokio::test]
async fn a_store_that_is_off_makes_no_directory() {
    let off = TreesConfig {
        enabled: false,
        ..TreesConfig::default()
    };
    let bench = Bench::with("trees-off", off).await;
    assert!(!bench.rig.config.path.join("trees").exists());
    assert!(!bench.rig.config.path.join("placements").exists());
    assert_eq!(bench.store.sizes(), (0, 0));
    assert!(!bench.store.config().enabled);
    assert!(bench.store.locate_fs("a").is_none());
}

/// Asserts that a marker that cannot be written (the tree's directory is gone) is no error of
/// `mark_ready`, that a tree already marked is not marked twice, and that a tree that is not
/// there is refused.
#[tokio::test]
async fn mark_ready_is_once_and_a_marker_that_cannot_be_written_is_not_an_error() {
    let (bench, _) = placed("trees-mark").await;
    let tree = bench.built().root;
    std::fs::remove_dir_all(tree_path(&bench.rig, &tree)).unwrap();
    assert!(bench.store.mark_ready(&tree).is_ok());
    assert!(bench.store.mark_ready(&tree).is_ok());
    let unknown = TreeDigest::of(b"no such tree");
    assert_eq!(
        bench.store.mark_ready(&unknown).unwrap_err(),
        LifeError::UnknownTree
    );
}

/// Asserts that a step that cannot read the directory (the `trees` directory is a file) says so
/// and leaves the store as it was: still without the lock, and still refusing a create.
#[tokio::test]
async fn a_step_that_cannot_read_the_directory_leaves_the_store_read_only() {
    let bench = Bench::new("trees-step-fails").await;
    let holder = bench.rig.locked();
    let waiting = bench.rig.locked();
    let store = Bench::reopen(waiting.clone());
    let trees = bench.rig.config.path.join("trees");
    std::fs::remove_dir_all(&trees).unwrap();
    std::fs::write(&trees, b"a file").unwrap();
    drop(holder);
    assert!(waiting.tick().unwrap() == super::super::Tick::Took);
    store.step();
    assert_eq!(
        store
            .create(bench.request(vec![fs_at("jobs/1")]))
            .unwrap_err(),
        super::CreateError::ReadOnly
    );
}

/// Asserts that a placement held by an attachment and a read, whose lease is over when the
/// directory is read again, is not made again, and that dropping the read then takes nothing.
#[tokio::test]
async fn a_held_placement_whose_lease_ended_is_not_placed_again_by_a_reread() {
    let (bench, id) = placed("trees-held-ended").await;
    let store = &bench.store;
    store.attach(&id, "run").unwrap();
    let read = store.open_read(&id).unwrap();
    bench.rig.clock.advance(9 * HOUR);
    store.load().unwrap();
    assert!(store.placement(&id).is_none());
    assert_eq!(store.sizes(), (0, 0));
    drop(read);
    assert_eq!(store.sizes(), (0, 0));
}
