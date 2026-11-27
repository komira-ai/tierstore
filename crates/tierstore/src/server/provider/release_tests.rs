//! A release removes the source's placement at once, even while `tierstore fs`'s mount holds it,
//! so that the placements alive follow the sources not yet released, not the sources the mount
//! looked up within `fs.tree_idle_secs`. Each source here is held as the mount holds it once a
//! lookup of the source's path found the placement (`treefs/in_process.rs`, `InProcess::place`).

use super::testing::{AGENT, READ_ONLY, Rig, source, unavailable};
use crate::server::local_cache::trees::{LifeError, MountPlace, PlacementId, TreesConfig};
use crate::{ListEntry, ListKind, Mode, TreeList};

/// The name the mount holds the placements under.
const FS_MOUNT: &str = "fs-mount";

/// The distinct trees the long tests place and release, about twice `trees.max_placements`.
const TREES: u32 = 2000;

/// A tree of one empty file, `f<i>`: a different tree for each `i`, with no file to read.
fn tree(i: u32) -> TreeList {
    TreeList {
        entries: vec![ListEntry {
            path: format!("f{i}"),
            kind: ListKind::File {
                mode: Mode::new(0o644).expect("a mode"),
                size: 0,
                source: None,
                sha256: None,
            },
        }],
    }
}

/// Takes the hold the mount takes at a lookup of the path of the source `index` of `run`: the
/// placement there, attached for the mount. Its id.
fn looked_up(rig: &Rig, run: &str, index: u32) -> PlacementId {
    let store = &rig.bench.store;
    let MountPlace::Placement(id) = store.mount_place(&Rig::place_of(run, index)) else {
        panic!("no placement at the path of {run}/{index}");
    };
    store.attach(&id, FS_MOUNT).expect("the mount holds it");
    id
}

/// Prepares the source `index` of `run` with the tree list `key`, mounts it, and holds it as the
/// mount's lookup does. The placement's id.
async fn held(rig: &Rig, run: &str, index: u32, key: &str) -> PlacementId {
    rig.provider
        .prepare(source(run, index), key, READ_ONLY)
        .expect("prepared");
    assert_eq!(
        rig.provider.mount(source(run, index)).await,
        Ok(Rig::path_of(run, index)),
        "the mount of {run}/{index}"
    );
    looked_up(rig, run, index)
}

/// Asserts that 2,000 distinct trees, each prepared, mounted, held by the mount and released one
/// after another under the default `trees.max_placements` (1024), all mount, and that after each
/// release no placement and no tree is left.
#[tokio::test]
async fn distinct_trees_past_the_cap_released_one_after_another_all_mount() {
    let rig = Rig::new("prov-release-many").await;
    assert_eq!(TreesConfig::default().max_placements, 1024);
    let store = &rig.bench.store;
    for i in 0..TREES {
        let key = rig.put_list_v2(&tree(i)).await;
        let run = format!("js.{i}");
        let id = held(&rig, &run, 0, &key).await;
        assert_eq!(store.sizes(), (1, 1));
        assert_eq!(rig.provider.release(source(&run, 0)), Ok(()));
        assert_eq!(store.sizes(), (0, 0), "tree {i} is left after its release");
        assert!(store.placement(&id).is_none());
    }
}

/// Asserts that, for 2,000 distinct trees one after another, a tree two sources share, each held
/// by the mount, stays while one source is released (the other still mounts and its placement
/// still reads the tree), and goes with the release of the last: every mount succeeds, and after
/// each pair of releases nothing is left.
#[tokio::test]
async fn a_tree_two_sources_share_goes_with_the_last_release() {
    let rig = Rig::new("prov-release-shared").await;
    let store = &rig.bench.store;
    for i in 0..TREES {
        let key = rig.put_list_v2(&tree(i)).await;
        let run = format!("js.{i}");
        let first = held(&rig, &run, 0, &key).await;
        let second = held(&rig, &run, 1, &key).await;
        assert_ne!(first, second);
        assert_eq!(store.sizes(), (2, 1));
        assert_eq!(rig.provider.release(source(&run, 0)), Ok(()));
        assert_eq!(store.sizes(), (1, 1), "tree {i} after the first release");
        assert!(store.placement(&first).is_none());
        assert_eq!(
            rig.provider.mount(source(&run, 1)).await,
            Ok(Rig::path_of(&run, 1))
        );
        assert!(store.resolve(&second, &format!("f{i}")).is_ok());
        assert_eq!(rig.provider.release(source(&run, 1)), Ok(()));
        assert_eq!(store.sizes(), (0, 0), "tree {i} after the last release");
    }
}

/// Asserts that a read open on the placement when its source is released keeps the placement
/// until the read ends, though its path no longer resolves, the mount's hold is gone and the mount
/// cannot take it again.
#[tokio::test]
async fn a_read_open_at_the_release_keeps_the_placement_until_it_ends() {
    let rig = Rig::new("prov-release-read").await;
    let store = &rig.bench.store;
    let id = held(&rig, "js.1", 0, &rig.key).await;
    let read = store.open_read(&id).expect("a read");
    assert_eq!(rig.provider.release(source("js.1", 0)), Ok(()));
    assert!(store.locate_fs(&Rig::place_of("js.1", 0)).is_none());
    let info = store.placement(&id).expect("held by the read");
    assert!(info.released);
    assert_eq!((info.attachments, info.reads), (0, 1));
    assert_eq!(
        store.attach(&id, FS_MOUNT),
        Err(LifeError::UnknownPlacement)
    );
    assert_eq!(store.sizes(), (1, 1));
    drop(read);
    assert_eq!(store.sizes(), (0, 0));
}

/// Asserts that a sweep removes at once the placements of the sources it releases that the mount
/// holds.
#[tokio::test]
async fn a_sweep_removes_the_placements_the_mount_holds_at_once() {
    let rig = Rig::new("prov-release-sweep").await;
    held(&rig, "js.1", 0, &rig.key).await;
    held(&rig, "js.2", 0, &rig.key).await;
    assert_eq!(rig.bench.store.sizes(), (2, 1));
    assert_eq!(rig.provider.sweep(AGENT, &[]), Ok(2));
    assert_eq!(rig.bench.store.sizes(), (0, 0));
}

/// Asserts that the cap still counts the sources not released: with `trees.max_placements` at 2
/// and two sources held, a third is unavailable, and once one of the two is released the third is
/// placed by its next `Mount`.
#[tokio::test]
async fn a_source_past_the_cap_mounts_once_a_held_source_is_released() {
    let config = TreesConfig {
        max_placements: 2,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-release-cap", config, |_| {}).await;
    held(&rig, "js.1", 0, &rig.key).await;
    held(&rig, "js.2", 0, &rig.key).await;
    rig.provider
        .prepare(source("js.3", 0), &rig.key, READ_ONLY)
        .expect("prepared");
    let over = rig.provider.mount(source("js.3", 0)).await.unwrap_err();
    assert_eq!(
        unavailable(over),
        "the tree was not made: too many placements: 2 are alive"
    );
    assert_eq!(rig.provider.release(source("js.1", 0)), Ok(()));
    assert_eq!(
        rig.provider.mount(source("js.3", 0)).await,
        Ok(Rig::path_of("js.3", 0))
    );
    assert_eq!(rig.bench.store.sizes(), (2, 1));
}
