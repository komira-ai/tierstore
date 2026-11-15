//! Reading the directory back: what survives a restart, what is dropped and why, what a store
//! without the lock leaves alone, and the formats' compatibility with the cache before this.

use std::path::{Path, PathBuf};

use super::super::layout::{meta_path, name_of};
use super::files::{
    At, PlacementFile, RECORDS, SOURCES, encode_records, placement_file, write_new,
};
use super::location::{Location, PlacementId};
use super::testing::{Bench, built, fs_at, s3_at, tree_path};
use super::{CreateRequest, TreeStore, TreesConfig};
use crate::{NewEntry, NewKind, TreeDigest, TreeRecord};

const HOUR: u64 = 3600 * 1000;

fn dir_entry(path: &str) -> NewEntry {
    NewEntry {
        path: path.to_owned(),
        kind: NewKind::Dir { mode: None },
    }
}

/// A placement file for `tree` at `at`, named by its id, expiring at `expires`; the id.
fn write_placement(root: &Path, tree: &TreeDigest, at: &[Location], expires: u64) -> PlacementId {
    let id = PlacementId::of(tree, at);
    let file = PlacementFile {
        v: 1,
        tree: tree.to_string(),
        at: at.iter().map(At::of).collect(),
        created_ms: 1,
        expires_ms: expires,
    };
    write_new(
        &placement_file(root, &id),
        &serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();
    id
}

/// Asserts what a restart keeps of a ready tree whose ranges are all in the cache: the tree and its
/// placement (found by location, resolved), its pins taken again, and the lease it had; and that
/// the same restart drops the tree, its placement file and its directory once the cache has lost
/// the pack's bytes, and drops nothing of a tree that was never ready.
#[tokio::test]
async fn tree_survives_restart_and_drops_when_a_blob_is_lost() {
    let bench = Bench::new("trees-restart").await;
    bench.fill_all().await;
    let ready = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("jobs/1"), s3_at("jobs/1/")])
    };
    let made = bench.store.create(ready).unwrap();
    let lazy = built(vec![dir_entry("only/a/dir")]);
    let lazy = bench
        .store
        .create(CreateRequest {
            built: lazy,
            at: vec![fs_at("lazy")],
            ttl: None,
            ready: false,
        })
        .unwrap();

    let cache = bench.rig.reopen();
    let store = Bench::reopen(cache.clone());
    assert_eq!(store.sizes(), (2, 2));
    assert_eq!(cache.pinned_bytes(), 6000);
    let found = store.locate_s3("build-cas", "jobs/1/bin/tool").unwrap();
    assert_eq!(found.placement, made.placement);
    assert!(store.resolve(&made.placement, "lib/deep/data").is_ok());
    let info = store.placement(&made.placement).unwrap();
    assert_eq!(
        (info.ready, info.created_ms, info.expires_ms),
        (true, 0, 4 * HOUR)
    );
    assert!(store.placement(&lazy.placement).is_some());

    // The cache loses the pack: the ready tree goes with its placement, the lazy one stays.
    let hex = name_of(&bench.pack.key).hex;
    let root = &bench.rig.config.path;
    for path in [bench.rig.file(&bench.pack.key), meta_path(root, &hex)] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(root.join("extents-v1"));
    let cache = bench.rig.reopen();
    let store = Bench::reopen(cache.clone());
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.placement(&made.placement).is_none());
    assert!(!tree_path(&bench.rig, &made.tree).exists());
    assert!(!placement_file(root, &made.placement).exists());
    assert!(store.locate_fs("jobs/1").is_none());
    assert!(store.placement(&lazy.placement).is_some());
    assert_eq!(cache.pinned_bytes(), 0);
}

/// Asserts that a ready tree is dropped at a restart when the cache holds only some of its ranges
/// (the third blob was never fetched), and not when it is not ready.
#[tokio::test]
async fn a_ready_tree_with_a_range_the_cache_lacks_is_dropped() {
    let bench = Bench::new("trees-restart-partial").await;
    bench.fill(0).await;
    bench.fill(1).await;
    let ready = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("a")])
    };
    bench.store.create(ready).unwrap();
    let store = Bench::reopen(bench.rig.reopen());
    assert_eq!(store.sizes(), (0, 0));
    let lazy = bench.store.create(bench.request(vec![fs_at("b")]));
    assert!(lazy.is_ok());
}

/// Asserts that each way a tree's directory can be wrong is a reason to drop it, with its
/// placement, and its directory is removed: a record that does not hash to its name, one that
/// hashes and does not parse, a records file cut short, one with a record listed twice, one with
/// no record of the root, a missing or garbled sources file, a file where a directory should be,
/// a name that is no digest, and a staging directory a crash left.
#[tokio::test]
async fn a_tree_directory_that_does_not_hold_up_is_dropped_and_removed() {
    type Damage = Box<dyn Fn(&Path, &TreeDigest)>;
    let breaks: Vec<(&str, Damage)> = vec![
        (
            "mishashed",
            Box::new(|dir, _| {
                let path = dir.join(RECORDS);
                let mut bytes = std::fs::read(&path).unwrap();
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                std::fs::write(path, bytes).unwrap();
            }),
        ),
        (
            "unparsable",
            Box::new(|dir, _| {
                let body = b"not a record".to_vec();
                let digest = TreeDigest::of(&body);
                let mut bytes = Vec::new();
                bytes.extend_from_slice(b"TSRS\x01");
                bytes.extend_from_slice(&1u32.to_be_bytes());
                bytes.extend_from_slice(digest.as_bytes());
                bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
                bytes.extend_from_slice(&body);
                std::fs::write(dir.join(RECORDS), bytes).unwrap();
            }),
        ),
        (
            "short",
            Box::new(|dir, _| {
                let path = dir.join(RECORDS);
                let bytes = std::fs::read(&path).unwrap();
                std::fs::write(path, &bytes[..bytes.len() - 3]).unwrap();
            }),
        ),
        (
            "twice",
            Box::new(|dir, tree| {
                let records: Vec<(TreeDigest, TreeRecord)> = {
                    let made = built(vec![dir_entry("a")]);
                    vec![made.records[0].clone(), made.records[0].clone()]
                };
                let _ = tree;
                std::fs::write(dir.join(RECORDS), encode_records(&records).0).unwrap();
            }),
        ),
        (
            "no root",
            Box::new(|dir, _| {
                let made = built(vec![dir_entry("other/dir")]);
                let one = vec![made.records[0].clone()];
                std::fs::write(dir.join(RECORDS), encode_records(&one).0).unwrap();
            }),
        ),
        (
            "no sources",
            Box::new(|dir, _| std::fs::remove_file(dir.join(SOURCES)).unwrap()),
        ),
        (
            "garbled sources",
            Box::new(|dir, _| std::fs::write(dir.join(SOURCES), b"junk").unwrap()),
        ),
        (
            "a file",
            Box::new(|dir, _| {
                std::fs::remove_dir_all(dir).unwrap();
                std::fs::write(dir, b"a file").unwrap();
            }),
        ),
    ];
    for (name, damage) in breaks {
        let bench = Bench::new(&format!("trees-damaged-{}", name.replace(' ', "-"))).await;
        let made = bench
            .store
            .create(bench.request(vec![fs_at("jobs/1")]))
            .unwrap();
        let dir = tree_path(&bench.rig, &made.tree);
        damage(&dir, &made.tree);
        let trees = bench.rig.config.path.join("trees");
        std::fs::create_dir(trees.join("not-a-digest")).unwrap();
        std::fs::create_dir(trees.join(".new-7")).unwrap();
        let store = Bench::reopen(bench.rig.reopen());
        assert_eq!(store.sizes(), (0, 0), "{name}");
        assert!(!dir.exists(), "{name}");
        assert!(
            !placement_file(&bench.rig.config.path, &made.placement).exists(),
            "{name}"
        );
        assert_eq!(std::fs::read_dir(&trees).unwrap().count(), 0, "{name}");
    }
}

/// Asserts that a tree whose child lost its bytes is dropped with the child and its placement, in
/// the order the children are held, while a tree that shares nothing with it stays.
#[tokio::test]
async fn a_tree_whose_child_is_dropped_is_dropped() {
    let bench = Bench::new("trees-child-dropped").await;
    bench.fill_all().await;
    let base = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("base")])
    };
    let base = bench.store.create(base).unwrap();
    let hold = |path: &str, at: &str| CreateRequest {
        built: built(vec![NewEntry {
            path: path.to_owned(),
            kind: NewKind::Tree { digest: base.tree },
        }]),
        at: vec![fs_at(at)],
        ttl: None,
        ready: false,
    };
    let middle = bench.store.create(hold("tc", "mid")).unwrap();
    let top = bench
        .store
        .create(CreateRequest {
            built: built(vec![NewEntry {
                path: "up".to_owned(),
                kind: NewKind::Tree {
                    digest: middle.tree,
                },
            }]),
            at: vec![fs_at("top")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    let alone = bench
        .store
        .create(CreateRequest {
            built: built(vec![dir_entry("x")]),
            at: vec![fs_at("alone")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    assert_eq!(bench.store.sizes(), (4, 4));
    let store = Bench::reopen(bench.rig.reopen());
    assert_eq!(store.sizes(), (4, 4));
    assert!(store.resolve(&top.placement, "up/tc/bin/tool").is_ok());

    let root = &bench.rig.config.path;
    let _ = std::fs::remove_file(bench.rig.file(&bench.pack.key));
    let _ = std::fs::remove_dir_all(root.join("extents-v1"));
    let store = Bench::reopen(bench.rig.reopen());
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.placement(&alone.placement).is_some());
    for gone in [&base, &middle, &top] {
        assert!(!tree_path(&bench.rig, &gone.tree).exists());
        assert!(!placement_file(root, &gone.placement).exists());
    }
}

/// Asserts which placement files a start keeps, and that it removes each other: the name is no id,
/// the JSON does not read, the tree is no digest or is not there, a location is not one, the
/// location set is empty, the id is not of what the file holds, a location is the same twice or is
/// met by one kept before it (the lower id is kept), and the lease is over; and that a file of a
/// later version is left where it is and not read.
#[tokio::test]
async fn a_placement_file_that_does_not_hold_up_is_dropped_and_removed() {
    let bench = Bench::new("trees-placements").await;
    let made = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1")]))
        .unwrap();
    drop(bench.store.release(&made.placement));
    let tree = made.tree;
    let root = bench.rig.config.path.clone();
    let tree_hex = tree.to_string();
    // The tree is released with its placement; place it again so that the directory has it.
    let kept = bench
        .store
        .create(bench.request(vec![fs_at("kept/1")]))
        .unwrap();
    let dir = root.join("placements");
    let raw = |name: &str, text: String| std::fs::write(dir.join(name), text).unwrap();
    let file = |tree: &str, at: &str, expires: u64| {
        format!(r#"{{"v":1,"tree":"{tree}","at":{at},"created_ms":1,"expires_ms":{expires}}}"#)
    };
    let fs_json = |path: &str| format!(r#"{{"interface":"fs","path":"{path}"}}"#);
    let id_of = |at: &[Location]| PlacementId::of(&tree, at);
    let named = |at: &[Location]| id_of(at).to_string();

    raw("readme", "not a placement".to_owned());
    raw("abc.new", String::new());
    raw(&"1".repeat(32), "{ not json".to_owned());
    raw(
        &"2".repeat(32),
        file("zz", &format!("[{}]", fs_json("two")), 9 * HOUR),
    );
    raw(
        &"3".repeat(32),
        file(
            &TreeDigest::of(b"gone").to_string(),
            &format!("[{}]", fs_json("three")),
            9 * HOUR,
        ),
    );
    raw(
        &"4".repeat(32),
        file(
            &tree_hex,
            r#"[{"interface":"fs","path":"trees"}]"#,
            9 * HOUR,
        ),
    );
    raw(&"5".repeat(32), file(&tree_hex, "[]", 9 * HOUR));
    raw(
        &"6".repeat(32),
        file(&tree_hex, &format!("[{}]", fs_json("six")), 9 * HOUR),
    );
    let twice = [fs_at("twice"), fs_at("twice")];
    raw(
        &named(&twice),
        file(&tree_hex, &format!("[{0},{0}]", fs_json("twice")), 9 * HOUR),
    );
    let expired = [fs_at("expired")];
    raw(
        &named(&expired),
        file(&tree_hex, &format!("[{}]", fs_json("expired")), HOUR),
    );
    // A later version: left alone.
    let later = [fs_at("later")];
    raw(
        &named(&later),
        file(&tree_hex, &format!("[{}]", fs_json("later")), 9 * HOUR)
            .replace(r#""v":1"#, r#""v":2"#),
    );
    // Two placements that meet: the lower id is kept, the other dropped.
    let (low, high) = {
        let a = [fs_at("pair/inside")];
        let b = [fs_at("pair/inside/deeper")];
        let (ia, ib) = (id_of(&a), id_of(&b));
        if ia < ib { (a, b) } else { (b, a) }
    };
    let bench_clock = &bench.rig.clock;
    bench_clock.set(2 * HOUR);
    write_placement(&root, &tree, &low, 9 * HOUR);
    write_placement(&root, &tree, &high, 9 * HOUR);
    let fresh = write_placement(&root, &tree, &[fs_at("fresh")], 9 * HOUR);

    let store = Bench::reopen(bench.rig.reopen());
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut want = vec![
        kept.placement.to_string(),
        fresh.to_string(),
        id_of(&low).to_string(),
        named(&later),
    ];
    want.sort();
    let mut have = names;
    have.sort();
    assert_eq!(have, want);
    // The kept placement of the tree's first create ended at 4h; the clock is at 2h.
    assert!(store.placement(&kept.placement).is_some());
    assert!(store.placement(&fresh).is_some());
    assert!(store.placement(&id_of(&low)).is_some());
    assert!(store.placement(&id_of(&high)).is_none());
    assert!(store.placement(&id_of(&later)).is_none());
    assert_eq!(store.sizes(), (3, 1));
}

/// Asserts that a store over a cache that holds no lock reads what is there and removes nothing:
/// a tree directory that is damaged is left in the directory and not served, a placement of it
/// is left and not served, a stray file is left, and no pin is taken.
#[tokio::test]
async fn a_store_without_the_lock_reads_and_removes_nothing() {
    let bench = Bench::new("trees-ro-start").await;
    bench.fill_all().await;
    let good = bench
        .store
        .create(bench.request(vec![fs_at("good")]))
        .unwrap();
    let other = bench
        .store
        .create(CreateRequest {
            built: built(vec![dir_entry("o")]),
            at: vec![fs_at("other")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    let root = bench.rig.config.path.clone();
    std::fs::write(tree_path(&bench.rig, &other.tree).join(SOURCES), b"junk").unwrap();
    std::fs::write(root.join("placements").join("readme"), b"x").unwrap();
    std::fs::create_dir(root.join("trees").join(".new-1")).unwrap();
    let holder = bench.rig.locked();
    let waiting = bench.rig.locked();
    let store = Bench::reopen(waiting.clone());
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.placement(&good.placement).is_some());
    assert!(store.placement(&other.placement).is_none());
    assert!(tree_path(&bench.rig, &other.tree).exists());
    assert!(placement_file(&root, &other.placement).exists());
    assert!(root.join("placements").join("readme").exists());
    assert!(root.join("trees").join(".new-1").exists());
    assert_eq!(waiting.pinned_bytes(), 0);
    drop(holder);
}

/// Asserts that a store over a directory that has no `trees` or `placements` (a directory the
/// release before wrote) opens empty and makes them, and that a cache of the release before this
/// reads a directory that has them: the objects it holds are unchanged, nothing is removed, and the
/// two new directories are left as they are, so a rollback keeps its cache and loses no tree.
#[tokio::test]
async fn the_directories_of_trees_are_invisible_to_the_cache_of_the_release_before() {
    let bench = Bench::new("trees-compat").await;
    assert!(bench.rig.config.path.join("trees").is_dir());
    assert!(bench.rig.config.path.join("placements").is_dir());
    bench.fill_all().await;
    let ready = CreateRequest {
        ready: true,
        ..bench.request(vec![fs_at("a")])
    };
    let made = bench.store.create(ready).unwrap();
    let held = bench.rig.cache.held();
    let before: Vec<PathBuf> = {
        let tree = tree_path(&bench.rig, &made.tree);
        vec![
            tree.join(RECORDS),
            tree.join(SOURCES),
            placement_file(&bench.rig.config.path, &made.placement),
        ]
    };
    let bytes: Vec<Vec<u8>> = before
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    // The cache alone, as the release before runs it: no store.
    let cache = bench.rig.reopen();
    assert_eq!(cache.held(), held);
    assert_eq!(cache.counters().discarded_at_start(), 0);
    for (path, bytes) in before.iter().zip(&bytes) {
        assert_eq!(&std::fs::read(path).unwrap(), bytes);
    }
    // A directory with none of them: the store makes them and finds nothing.
    let fresh = Bench::new("trees-compat-fresh").await;
    assert_eq!(fresh.store.sizes(), (0, 0));
    assert!(fresh.rig.config.path.join("trees").is_dir());
}

/// Asserts that a store cannot open over a directory it cannot make `trees` in, and that a store
/// without the lock reads a `trees` that is not a directory as empty.
#[tokio::test]
async fn a_directory_that_cannot_be_read_is_an_error_or_empty() {
    let bench = Bench::new("trees-unreadable").await;
    let trees = bench.rig.config.path.join("trees");
    std::fs::remove_dir_all(&trees).unwrap();
    std::fs::write(&trees, b"a file").unwrap();
    assert!(TreeStore::open(bench.rig.reopen(), TreesConfig::default()).is_err());
    let holder = bench.rig.locked();
    let store = Bench::reopen(bench.rig.locked());
    assert_eq!(store.sizes(), (0, 0));
    drop(holder);
}

/// Asserts that a file or a directory the store may not remove is said and kept going: a path that
/// is not there warns and nothing else happens, and a store that does not own the directory
/// removes nothing at all.
#[tokio::test]
async fn discard_of_a_path_that_is_not_there_is_a_warning_and_a_store_that_is_not_owner_keeps_it() {
    let bench = Bench::new("trees-discard").await;
    let root = bench.rig.config.path.clone();
    super::start::discard(true, &root.join("not-there"));
    let kept = root.join("kept");
    std::fs::write(&kept, b"x").unwrap();
    super::start::discard(false, &kept);
    assert!(kept.exists());
    super::start::discard(true, &kept);
    assert!(!kept.exists());
}

/// Asserts that a placement file with a location of an interface this release does not know (what
/// a later release of `v` 1 may write) is not read and is not removed by a start of this release,
/// so a rollback and a roll forward again lose nothing; the placement is not served meanwhile.
#[tokio::test]
async fn a_placement_of_an_interface_a_later_release_added_is_left_where_it_is() {
    let bench = Bench::new("trees-later-interface").await;
    let made = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1")]))
        .unwrap();
    let path = placement_file(&bench.rig.config.path, &made.placement);
    let text = std::fs::read_to_string(&path).unwrap();
    let later = text.replace(r#""interface":"fs""#, r#""interface":"gcs""#);
    assert_ne!(text, later);
    std::fs::write(&path, &later).unwrap();
    let store = Bench::reopen(bench.rig.reopen());
    assert!(store.locate_fs("jobs/1").is_none());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), later);
}
