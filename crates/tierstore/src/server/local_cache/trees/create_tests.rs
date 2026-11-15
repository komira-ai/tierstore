//! Making a tree and its placement: what a create writes and pins, the refresh of one that is
//! there, and each refusal.

use std::time::Duration;

use super::files::{RECORDS, SOURCES, placement_file};
use super::testing::{Bench, built, fs_at, s3_at, tree_path};
use super::{CreateError, CreateRequest, Created, Location, PlacementId, TreesConfig};
use crate::{NewEntry, NewKind, TreeDigest};

fn tree_entry(path: &str, digest: TreeDigest) -> NewEntry {
    NewEntry {
        path: path.to_owned(),
        kind: NewKind::Tree { digest },
    }
}

/// Asserts what a create of the small tree at two locations makes: the answer (the tree's digest,
/// the placement named by the tree and locations, the entries it was made of, a lease of the
/// default four hours), the records and sources files and the placement file in the cache
/// directory, the pack pinned once however many files it holds, and nothing left in staging.
#[tokio::test]
async fn create_writes_the_tree_and_its_placement_and_pins_the_pack_once() {
    let bench = Bench::new("trees-create").await;
    bench.fill_all().await;
    let at = vec![s3_at("jobs/1/root/"), fs_at("jobs/1/root")];
    let made = bench.store.create(bench.request(at.clone())).unwrap();
    let tree = bench.built().root;
    assert_eq!(made.tree, tree);
    assert_eq!(made.placement, PlacementId::of(&tree, &at));
    assert!(made.created && made.new_tree);
    assert_eq!((made.files, made.bytes, made.packs), (4, 6000, 1));
    assert_eq!(made.expires_ms, 4 * 3600 * 1000);
    assert_eq!(
        (made.pins.pinned, made.pins.refused, made.pins.absent),
        (1, 0, 0)
    );
    let dir = tree_path(&bench.rig, &tree);
    assert!(dir.join(RECORDS).is_file() && dir.join(SOURCES).is_file());
    assert!(!dir.join("ready").exists());
    assert!(placement_file(&bench.rig.config.path, &made.placement).is_file());
    let trees = bench.rig.config.path.join("trees");
    let names: Vec<_> = std::fs::read_dir(&trees).unwrap().collect();
    assert_eq!(names.len(), 1);
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);
    assert_eq!(bench.store.sizes(), (1, 1));
}

/// Asserts that a pack the cache does not hold is not pinned (counted absent), and that a tree
/// asked to be ready is marked so.
#[tokio::test]
async fn a_pack_the_cache_lacks_is_not_pinned_and_ready_marks_the_tree() {
    let bench = Bench::new("trees-create-absent").await;
    let made = bench.store.create(bench.request(vec![fs_at("a")])).unwrap();
    assert_eq!((made.pins.pinned, made.pins.absent), (0, 1));
    assert_eq!(bench.rig.cache.pinned_bytes(), 0);
    let ready = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("b")])
    };
    let again = bench.store.create(ready).unwrap();
    assert!(again.created && !again.new_tree);
    assert!(tree_path(&bench.rig, &made.tree).join("ready").is_file());
    assert!(bench.store.placement(&again.placement).unwrap().ready);
    let refreshed = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("b")])
    };
    assert!(!bench.store.create(refreshed).unwrap().created);
}

/// Asserts that a tree made ready at its first create is marked, and pinned.
#[tokio::test]
async fn a_new_tree_made_ready_is_marked_and_pinned() {
    let bench = Bench::new("trees-create-ready").await;
    bench.fill_all().await;
    let request = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("a")])
    };
    let made = bench.store.create(request).unwrap();
    assert!(tree_path(&bench.rig, &made.tree).join("ready").is_file());
    assert_eq!(made.pins.pinned, 1);
}

/// Asserts that a create of the same tree at the same locations is one placement: the answer says
/// it was not made, the lease moves to the new time and ttl, and no tree, pin or file is added; and
/// that the same tree at other locations is a new placement of the tree the node has.
#[tokio::test]
async fn the_same_tree_at_the_same_locations_refreshes_and_at_others_places_again() {
    let bench = Bench::new("trees-refresh").await;
    bench.fill_all().await;
    let first = bench.store.create(bench.request(vec![fs_at("a")])).unwrap();
    bench.rig.clock.advance(1000);
    let request = CreateRequest {
        ttl: Some(Duration::from_secs(60)),
        ..bench.request(vec![fs_at("a")])
    };
    let again = bench.store.create(request).unwrap();
    assert_eq!(again.placement, first.placement);
    assert!(!again.created && !again.new_tree);
    assert_eq!(again.expires_ms, 1000 + 60_000);
    assert_eq!(again.pins.pinned, 0);
    assert_eq!(
        bench.store.placement(&first.placement).unwrap().expires_ms,
        61_000
    );
    assert_eq!(bench.store.sizes(), (1, 1));
    let other = bench.store.create(bench.request(vec![fs_at("b")])).unwrap();
    assert!(other.created && !other.new_tree);
    assert_ne!(other.placement, first.placement);
    assert_eq!(bench.store.sizes(), (2, 1));
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);
}

/// Asserts that a ttl longer than `trees.ttl_max` is taken as `ttl_max`.
#[tokio::test]
async fn a_ttl_over_the_most_is_taken_as_the_most() {
    let bench = Bench::new("trees-ttl-max").await;
    let request = CreateRequest {
        ttl: Some(Duration::from_secs(1000 * 3600)),
        ..bench.request(vec![fs_at("a")])
    };
    assert_eq!(
        bench.store.create(request).unwrap().expires_ms,
        24 * 3600 * 1000
    );
}

/// Asserts the refusals of the locations: none at all, the same twice in one request, one inside
/// another in one request, one equal to another placement's (a different tree there), one inside it
/// and one around it; each refused by its kind naming the location, and nothing made by any.
#[tokio::test]
async fn a_location_that_meets_another_is_refused_and_nothing_is_made() {
    let bench = Bench::new("trees-refuse-locations").await;
    let none = bench.store.create(bench.request(vec![])).unwrap_err();
    assert_eq!(none, CreateError::NoLocation);
    let twice = bench
        .store
        .create(bench.request(vec![fs_at("a"), fs_at("a")]));
    assert_eq!(twice.unwrap_err(), CreateError::Taken(fs_at("a")));
    let nested = bench
        .store
        .create(bench.request(vec![s3_at("a/"), s3_at("a/b/")]));
    assert_eq!(nested.unwrap_err(), CreateError::Overlaps(s3_at("a/b/")));
    let around = bench
        .store
        .create(bench.request(vec![s3_at("a/b/"), s3_at("a/")]));
    assert_eq!(around.unwrap_err(), CreateError::Overlaps(s3_at("a/")));
    assert_eq!(bench.store.sizes(), (0, 0));
    assert_eq!(
        std::fs::read_dir(bench.rig.config.path.join("trees"))
            .unwrap()
            .count(),
        0
    );

    let held = bench
        .store
        .create(bench.request(vec![s3_at("jobs/1/"), fs_at("j/1")]))
        .unwrap();
    let other = |at: Vec<Location>| CreateRequest {
        built: built(vec![NewEntry {
            path: "x".to_owned(),
            kind: NewKind::Dir { mode: None },
        }]),
        at,
        ttl: None,
        ready: false,
    };
    let taken = bench.store.create(other(vec![s3_at("jobs/1/")]));
    assert_eq!(taken.unwrap_err(), CreateError::Taken(s3_at("jobs/1/")));
    let inside = bench.store.create(other(vec![fs_at("j/1/x")]));
    assert_eq!(inside.unwrap_err(), CreateError::Overlaps(fs_at("j/1/x")));
    let over = bench.store.create(other(vec![s3_at("jobs/")]));
    assert_eq!(over.unwrap_err(), CreateError::Overlaps(s3_at("jobs/")));
    // The same tree at a set that shares one location with the held placement is not a refresh.
    let shared = bench
        .store
        .create(bench.request(vec![fs_at("j/1"), fs_at("k")]));
    assert_eq!(shared.unwrap_err(), CreateError::Taken(fs_at("j/1")));
    assert_eq!(bench.store.sizes(), (1, 1));
    assert!(bench.store.placement(&held.placement).is_some());
    // Other buckets and the other interface are other places.
    let elsewhere = Location::s3("other", "jobs/1/").unwrap();
    assert!(
        bench
            .store
            .create(other(vec![elsewhere, fs_at("jobs/1")]))
            .is_ok()
    );
}

/// Asserts the refusals that are not about a location: trees off, a tree of more file bytes than
/// the limit, more placements than the limit, a tree held by digest that is not on the node, and a
/// cache that holds no lock; each by its kind, and the text of each says what is wrong.
#[tokio::test]
async fn a_create_that_cannot_be_made_is_refused_by_kind() {
    let off = TreesConfig {
        enabled: false,
        ..TreesConfig::default()
    };
    let bench = Bench::with("trees-refuse-off", off).await;
    let refused = bench
        .store
        .create(bench.request(vec![fs_at("a")]))
        .unwrap_err();
    assert_eq!(refused, CreateError::Disabled);
    assert!(refused.to_string().contains("off"));
    assert!(!bench.rig.config.path.join("trees").exists());

    let small = TreesConfig {
        max_bytes: 5999,
        max_placements: 1,
        ..TreesConfig::default()
    };
    let bench = Bench::with("trees-refuse-limits", small).await;
    let large = bench
        .store
        .create(bench.request(vec![fs_at("a")]))
        .unwrap_err();
    assert_eq!(
        large,
        CreateError::TooLarge {
            bytes: 6000,
            max: 5999
        }
    );
    assert!(large.to_string().contains("6000"));
    let one = || CreateRequest {
        built: built(vec![NewEntry {
            path: "d".to_owned(),
            kind: NewKind::Dir { mode: None },
        }]),
        at: vec![fs_at("one")],
        ttl: None,
        ready: false,
    };
    bench.store.create(one()).unwrap();
    let more = CreateRequest {
        at: vec![fs_at("two")],
        ..one()
    };
    assert_eq!(
        bench.store.create(more).unwrap_err(),
        CreateError::TooMany(1)
    );

    let bench = Bench::new("trees-refuse-child").await;
    let missing = TreeDigest::of(b"not a tree here");
    let holds = CreateRequest {
        built: built(vec![tree_entry("below", missing)]),
        at: vec![fs_at("a")],
        ttl: None,
        ready: false,
    };
    let unknown = bench.store.create(holds).unwrap_err();
    assert_eq!(unknown, CreateError::UnknownChild(missing));
    assert!(unknown.to_string().contains(&missing.to_string()));
    assert_eq!(bench.store.sizes(), (0, 0));
}

/// Asserts that a cache that holds no lock makes nothing: a second cache over the directory is
/// read-only while the first holds the lock, and its store refuses the create.
#[tokio::test]
async fn a_cache_that_holds_no_lock_makes_no_tree() {
    let bench = Bench::new("trees-refuse-readonly").await;
    let holder = bench.rig.locked();
    let waiting = bench.rig.locked();
    let store = Bench::reopen(waiting);
    let refused = store.create(bench.request(vec![fs_at("a")])).unwrap_err();
    assert_eq!(refused, CreateError::ReadOnly);
    assert!(refused.to_string().contains("lock"));
    drop(holder);
}

/// Asserts that a tree holds another by digest: the parent is made with the child's files read
/// through the child's own records and sources, the parent's closure names both, the child's
/// pack is pinned by the child and the parent's by the parent, and that the parent keeps the
/// child alive after the child's own placement is released.
#[tokio::test]
async fn a_tree_holds_another_by_digest_and_keeps_it() {
    let bench = Bench::new("trees-child").await;
    bench.fill_all().await;
    let base = bench
        .store
        .create(bench.request(vec![fs_at("base")]))
        .unwrap();
    let parent = bench
        .store
        .create(CreateRequest {
            built: built(vec![
                tree_entry("toolchain", base.tree),
                NewEntry {
                    path: "own".to_owned(),
                    kind: NewKind::Symlink {
                        target: "toolchain".to_owned(),
                    },
                },
            ]),
            at: vec![fs_at("jobs/1")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    assert_eq!((parent.files, parent.packs, parent.pins.pinned), (0, 0, 0));
    assert_eq!(bench.store.sizes(), (2, 2));
    bench.store.release(&base.placement).unwrap();
    // The base placement is gone, the base tree stays: the parent holds it.
    assert_eq!(bench.store.sizes(), (1, 2));
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);
    bench.store.release(&parent.placement).unwrap();
    assert_eq!(bench.store.sizes(), (0, 0));
    assert_eq!(bench.rig.cache.pinned_bytes(), 0);
}

/// Asserts that a failure to write the tree's directory is an error of kind `Io` that leaves
/// nothing: not the tree, not its placement, and no staging directory.
#[tokio::test]
async fn a_tree_that_cannot_be_written_leaves_nothing() {
    let bench = Bench::new("trees-io-tree").await;
    let trees = bench.rig.config.path.join("trees");
    std::fs::remove_dir_all(&trees).unwrap();
    std::fs::write(&trees, b"a file where the directory was").unwrap();
    let refused = bench
        .store
        .create(bench.request(vec![fs_at("a")]))
        .unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert!(refused.to_string().starts_with("writing the tree: "));
    assert_eq!(bench.store.sizes(), (0, 0));
}

/// Asserts that a failure to write the placement's file is an error of kind `Io` and takes the
/// tree it just made away again, directory and pins; that one on a refresh is an error too and
/// leaves the placement; and that a tree the node already had stays.
#[tokio::test]
async fn a_placement_that_cannot_be_written_leaves_no_tree_it_made() {
    let bench = Bench::new("trees-io-placement").await;
    bench.fill_all().await;
    let placements = bench.rig.config.path.join("placements");
    std::fs::remove_dir_all(&placements).unwrap();
    std::fs::write(&placements, b"a file where the directory was").unwrap();
    let refused = bench
        .store
        .create(bench.request(vec![fs_at("a")]))
        .unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert_eq!(bench.store.sizes(), (0, 0));
    assert!(!tree_path(&bench.rig, &bench.built().root).exists());
    assert_eq!(bench.rig.cache.pinned_bytes(), 0);
    // A refresh that cannot write fails too, and so does a new placement of a tree the node has.
    std::fs::remove_file(&placements).unwrap();
    std::fs::create_dir(&placements).unwrap();
    let made = bench.store.create(bench.request(vec![fs_at("a")])).unwrap();
    std::fs::remove_dir_all(&placements).unwrap();
    std::fs::write(&placements, b"a file").unwrap();
    assert!(bench.store.create(bench.request(vec![fs_at("a")])).is_err());
    assert!(bench.store.placement(&made.placement).is_some());
    assert!(bench.store.create(bench.request(vec![fs_at("b")])).is_err());
    assert_eq!(bench.store.sizes(), (1, 1));
    assert!(tree_path(&bench.rig, &made.tree).exists());
}

/// Asserts that the answer of a create counts a file once for each entry and a pack once, and that
/// a file with no source is the whole object `cas/l/<sha256>`.
#[tokio::test]
async fn a_file_with_no_source_is_the_whole_object_named_by_its_digest() {
    let bench = Bench::new("trees-whole").await;
    let body = vec![7u8; 100];
    let sha = TreeDigest::of(&body);
    let request = CreateRequest {
        built: built(vec![NewEntry {
            path: "f".to_owned(),
            kind: NewKind::File {
                mode: None,
                sha256: sha,
                size: 100,
                from: None,
            },
        }]),
        at: vec![fs_at("a")],
        ttl: None,
        ready: false,
    };
    let made: Created = bench.store.create(request).unwrap();
    assert_eq!((made.files, made.bytes, made.packs), (1, 100, 1));
    assert_eq!((made.pins.absent, made.pins.pinned), (1, 0));
}
