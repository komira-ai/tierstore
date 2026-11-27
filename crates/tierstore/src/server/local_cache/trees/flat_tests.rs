//! The tree of a placement flattened for the mount, where a path of the mount is among the fs
//! locations, and the paths of the locations.

use super::super::files::RECORDS;
use super::super::testing::{Bench, built, flat_of, fs_at, s3_at, tree_path};
use super::super::{CreateRequest, PlacementId, ReadError, TreeStore};
use super::{FlatEntry, FlatError, FlatKind, MountPlace, placement_id};
use crate::{NewEntry, NewKind, TreeDigest};

async fn placed(name: &str) -> (Bench, PlacementId) {
    let bench = Bench::new(name).await;
    let at = vec![fs_at("jobs/1/root"), s3_at("jobs/1/root/")];
    let made = bench.store.create(bench.request(at)).unwrap();
    (bench, made.placement)
}

fn dir(parent: u32, name: &str, mode: u16) -> FlatEntry {
    FlatEntry {
        parent: Some(parent),
        name: name.to_owned(),
        mode,
        kind: FlatKind::Dir,
    }
}

fn file(bench: &Bench, parent: u32, name: &str, mode: u16, blob: usize) -> FlatEntry {
    let (sha256, offset, size) = &bench.pack.blobs[blob];
    FlatEntry {
        parent: Some(parent),
        name: name.to_owned(),
        mode,
        kind: FlatKind::File {
            size: *size,
            sha256: sha256.clone(),
            pack: Some(0),
            offset: *offset,
        },
    }
}

/// Asserts that the small tree is flattened root first, depth first, the children of a directory in
/// name order, each entry naming the index of its parent, with the modes as stored (the setuid bit
/// of a file kept, a link at 0777), the size, digest, pack and offset of every file, no pack for
/// the empty file, the one pack listed once, and only the fs location among the paths.
#[tokio::test]
async fn a_placement_is_flattened_root_first_and_depth_first_in_name_order() {
    let (bench, id) = placed("flat-order").await;
    let flat = flat_of(&bench.store, &id).unwrap();
    assert_eq!(flat.paths, vec!["jobs/1/root".to_owned()]);
    assert_eq!(flat.packs, vec![bench.pack.key.clone()]);
    let empty_file = FlatEntry {
        parent: Some(0),
        name: "zero".to_owned(),
        mode: 0o644,
        kind: FlatKind::File {
            size: 0,
            sha256: TreeDigest::of(b"").to_string(),
            pack: None,
            offset: 0,
        },
    };
    let link = FlatEntry {
        parent: Some(0),
        name: "link".to_owned(),
        mode: 0o777,
        kind: FlatKind::Symlink {
            target: "bin/tool".to_owned(),
        },
    };
    let want = vec![
        FlatEntry {
            parent: None,
            name: String::new(),
            mode: 0o755,
            kind: FlatKind::Dir,
        },
        dir(0, "bin", 0o755),
        file(&bench, 1, "other", 0o644, 1),
        file(&bench, 1, "tool", 0o4755, 0),
        dir(0, "empty", 0o755),
        dir(0, "lib", 0o755),
        dir(5, "deep", 0o755),
        file(&bench, 6, "data", 0o644, 2),
        link,
        empty_file,
    ];
    assert_eq!(flat.entries, want);
}

/// Asserts that a placement at two fs locations lists both paths, and that a location in the
/// bucket interface is not listed.
#[tokio::test]
async fn a_placement_at_two_fs_locations_lists_both_paths() {
    let bench = Bench::new("flat-two").await;
    let at = vec![fs_at("jobs/1/a"), s3_at("jobs/"), fs_at("other/b")];
    let id = bench.store.create(bench.request(at)).unwrap().placement;
    let mut paths = flat_of(&bench.store, &id).unwrap().paths;
    paths.sort();
    assert_eq!(paths, vec!["jobs/1/a".to_owned(), "other/b".to_owned()]);
}

/// Asserts that a tree that holds a child tree by digest at two names lists the child's entries
/// below each of them, with the parents of each copy, and lists the pack once.
#[tokio::test]
async fn a_child_tree_held_twice_is_listed_at_both_places() {
    let bench = Bench::new("flat-twice").await;
    let base = bench
        .store
        .create(bench.request(vec![fs_at("base")]))
        .unwrap();
    let child = |path: &str| NewEntry {
        path: path.to_owned(),
        kind: NewKind::Tree { digest: base.tree },
    };
    let made = bench
        .store
        .create(CreateRequest {
            built: built(vec![child("a"), child("b")]),
            at: vec![fs_at("twice")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    let flat = flat_of(&bench.store, &made.placement).unwrap();
    let alone = flat_of(&bench.store, &base.placement).unwrap();
    let width = alone.entries.len() as u32;
    assert_eq!(flat.packs, alone.packs);
    let mut want = vec![alone.entries[0].clone()];
    for (name, top) in [("a", 1), ("b", 1 + width)] {
        let mut top_entry = alone.entries[0].clone();
        top_entry.parent = Some(0);
        top_entry.name = name.to_owned();
        want.push(top_entry);
        for entry in &alone.entries[1..] {
            let mut moved = entry.clone();
            moved.parent = entry.parent.map(|parent| top + parent);
            want.push(moved);
        }
    }
    assert_eq!(flat.entries, want);
}

/// Asserts that a placement that is not there, one with only a bucket location, and one released
/// with nothing holding it answer that there is no placement.
#[tokio::test]
async fn a_placement_that_is_unknown_bucket_only_or_released_is_not_flattened() {
    let bench = Bench::new("flat-none").await;
    let none = Err(FlatError::Read(ReadError::NoPlacement));
    let unknown = PlacementId::parse(&"ab".repeat(16)).unwrap();
    assert_eq!(flat_of(&bench.store, &unknown), none);
    let bucket = bench
        .store
        .create(bench.request(vec![s3_at("only/")]))
        .unwrap();
    assert_eq!(flat_of(&bench.store, &bucket.placement), none);
    let gone = bench
        .store
        .create(bench.request(vec![fs_at("gone")]))
        .unwrap();
    assert!(flat_of(&bench.store, &gone.placement).is_ok());
    bench.store.release(&gone.placement).unwrap();
    assert_eq!(flat_of(&bench.store, &gone.placement), none);
}

/// Asserts that a placement released while a run holds it is still flattened by its id, as it
/// still resolves, so that the mount goes on reading what it holds; the released placement is not
/// found at its location, and is flattened no more once the run lets go.
#[tokio::test]
async fn a_released_placement_that_a_run_holds_is_still_flattened_for_it() {
    let (bench, id) = placed("flat-held").await;
    let before = flat_of(&bench.store, &id).unwrap();
    bench.store.attach(&id, "run").unwrap();
    bench.store.release(&id).unwrap();
    assert_eq!(bench.store.mount_place("jobs/1/root"), MountPlace::Nothing);
    assert_eq!(flat_of(&bench.store, &id), Ok(before));
    assert!(bench.store.detach(&id, "run"));
    assert_eq!(
        flat_of(&bench.store, &id),
        Err(FlatError::Read(ReadError::NoPlacement))
    );
}

/// Asserts that a records file that is cut, overwritten with other bytes, or gone gives the answer
/// for damaged files and not a tree, and that the tree is flattened again once the file is whole.
#[tokio::test]
async fn a_tree_with_damaged_records_is_not_flattened() {
    let (bench, id) = placed("flat-damaged").await;
    let file = tree_path(&bench.rig, &bench.built().root).join(RECORDS);
    let good = std::fs::read(&file).unwrap();
    let damaged = |bench: &Bench| {
        matches!(
            flat_of(&bench.store, &id),
            Err(FlatError::Read(ReadError::Corrupt(_)))
        )
    };
    std::fs::write(&file, &good[..good.len() - 4]).unwrap();
    assert!(damaged(&bench));
    std::fs::write(&file, vec![b'X'; good.len()]).unwrap();
    assert!(damaged(&bench));
    std::fs::remove_file(&file).unwrap();
    assert!(damaged(&bench));
    std::fs::write(&file, &good).unwrap();
    assert!(flat_of(&bench.store, &id).is_ok());
}

/// Asserts where a path is: the exact location is the placement, each proper prefix at a `/`
/// boundary is above, and a path that only begins like one, is beside it, or is inside the
/// location is nothing.
#[tokio::test]
async fn a_path_is_the_placement_above_it_or_nothing() {
    let (bench, id) = placed("flat-place").await;
    let store = &bench.store;
    assert_eq!(store.mount_place("jobs/1/root"), MountPlace::Placement(id));
    assert_eq!(store.mount_place("jobs"), MountPlace::Above);
    assert_eq!(store.mount_place("jobs/1"), MountPlace::Above);
    for path in [
        "job",
        "jobs/2",
        "jobs/1/roo",
        "jobs/1/rootx",
        "jobs/1/root/bin",
        "jobs/1/root/bin/tool",
        "other",
        "jobs/1/root/x/y",
    ] {
        assert_eq!(store.mount_place(path), MountPlace::Nothing, "{path}");
    }
}

/// Asserts that a placement with only a bucket location under the prefix `jobs/` does not make the
/// mount path `jobs` a directory.
#[tokio::test]
async fn a_bucket_location_does_not_make_a_mount_path_a_directory() {
    let bench = Bench::new("flat-bucket-above").await;
    let _ = bench
        .store
        .create(bench.request(vec![s3_at("jobs/")]))
        .unwrap();
    assert_eq!(bench.store.mount_place("jobs"), MountPlace::Nothing);
}

/// Asserts that once a placement is released its location is nothing and the path above it is no
/// longer above anything.
#[tokio::test]
async fn a_released_placement_is_not_in_the_mount() {
    let (bench, id) = placed("flat-released").await;
    bench.store.release(&id).unwrap();
    assert_eq!(bench.store.mount_place("jobs/1/root"), MountPlace::Nothing);
    assert_eq!(bench.store.mount_place("jobs"), MountPlace::Nothing);
    assert_eq!(bench.store.mount_paths(), Vec::<String>::new());
}

/// Asserts that the paths of the fs locations come in sorted order, and leave out a released
/// placement, and a location in the bucket interface.
#[tokio::test]
async fn the_mounted_paths_are_the_sorted_fs_locations_of_live_placements() {
    let bench = Bench::new("flat-paths").await;
    let store: &TreeStore = &bench.store;
    assert_eq!(store.mount_paths(), Vec::<String>::new());
    for at in [
        vec![fs_at("zed")],
        vec![fs_at("jobs/2"), s3_at("s3only/")],
        vec![fs_at("alpha/one")],
        vec![s3_at("bucket-only/")],
    ] {
        let _ = store.create(bench.request(at)).unwrap();
    }
    let released = store.create(bench.request(vec![fs_at("beta")])).unwrap();
    store.release(&released.placement).unwrap();
    assert_eq!(
        store.mount_paths(),
        vec![
            "alpha/one".to_owned(),
            "jobs/2".to_owned(),
            "zed".to_owned()
        ]
    );
}

/// Asserts that a placement id is thirty-two lower-case hex digits, and that upper case, a short or
/// long text, a non-hex digit, a leading space and the empty text are none.
#[test]
fn a_placement_id_is_thirty_two_lower_case_hex_digits() {
    let good = "0123456789abcdef0123456789abcdef";
    assert_eq!(placement_id(good).unwrap().to_string(), good);
    for bad in [
        good.to_uppercase(),
        good[1..].to_owned(),
        format!("{good}0"),
        format!("{}g", &good[1..]),
        String::new(),
        format!(" {}", &good[1..]),
    ] {
        assert_eq!(placement_id(&bad), None, "{bad:?}");
    }
}
