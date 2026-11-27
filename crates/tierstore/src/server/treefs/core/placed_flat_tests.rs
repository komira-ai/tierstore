//! The flattened tree of a placement as the core takes it, and the keys and numbers of placements.

use std::time::Duration;

use super::{Errno, dir_ino, key_of, path_hash, placement_of, tree_of};
use crate::server::local_cache::trees::FlatTree;
use crate::server::local_cache::trees::testing::{Bench, fs_at, tree_path};
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::Live;
use crate::server::treefs::testing::{Built, PACK, PACK_START, Rig, sha};
use crate::tree_manifest::{EntryKind, TreeDigest};

/// The wait for the write role of a core that calls a read-only cache: what a test that waits it
/// out takes.
const SHORT: Duration = Duration::from_millis(100);

const ID: &str = "0123456789abcdef0123456789abcdef";
const HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A flattened tree read from its JSON, as the tree store gives it.
fn flat(json: &str) -> FlatTree {
    serde_json::from_str(json).unwrap()
}

/// A tree of a root, a directory `d` and the entry `last`, as a JSON object.
fn flat_with(last: &str) -> FlatTree {
    flat(&format!(
        r#"{{"paths":["a/b"],"packs":["p0","p1"],"entries":[
        {{"name":"","mode":493,"kind":"dir"}},
        {{"parent":0,"name":"d","mode":448,"kind":"dir"}},
        {last}]}}"#
    ))
}

/// The refusal of `flat` at the path `a/b`.
fn refused(flat: &FlatTree) -> String {
    match tree_of(flat, "a/b".to_owned()) {
        Ok(_) => panic!("the tree was accepted"),
        Err(why) => why,
    }
}

/// Asserts that a key is made of the id, the mark and the hash of the path, that it reads back to
/// the id and the hash, and that two locations of one placement have two keys.
#[test]
fn a_placement_key_reads_back_to_its_id_and_the_hash_of_its_path() {
    let key = key_of(ID, "jobs/1f3a/root").unwrap();
    assert_eq!(
        placement_of(&key),
        Some((ID.to_owned(), path_hash("jobs/1f3a/root")))
    );
    assert_eq!(&key.as_bytes()[16..24], b"tsplaced");
    let other = key_of(ID, "jobs/1f3a/other").unwrap();
    assert_ne!(key, other);
    assert_eq!(placement_of(&other).unwrap().0, ID);
    assert_eq!(key_of(ID, "jobs/1f3a/root"), Some(key));
}

/// Asserts that an id that is not thirty-two lower-case hex digits has no key.
#[test]
fn an_id_that_is_not_thirty_two_lower_case_hex_digits_has_no_key() {
    for id in [
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
        "",
    ] {
        assert_eq!(key_of(id, "a"), None, "{id:?}");
    }
}

/// Asserts that the digest of ordinary bytes is not the key of a placement.
#[test]
fn an_ordinary_digest_is_not_a_placement_key() {
    assert_eq!(placement_of(&TreeDigest::of(b"x")), None);
    assert_eq!(placement_of(&TreeDigest::from_bytes([0; 32])), None);
}

/// Asserts that the number of a directory above the locations is in the range of such directories,
/// is stable, differs by path, and is no put number.
#[test]
fn the_number_of_a_directory_above_locations_is_its_own() {
    let jobs = dir_ino("jobs");
    for number in [jobs, dir_ino("jobs/1f3a"), dir_ino("")] {
        assert!(ino::is_placed_dir(number));
        assert_eq!(ino::put_of(number), None);
    }
    assert_eq!(jobs, dir_ino("jobs"));
    assert_ne!(jobs, dir_ino("jobs/1f3a"));
    assert_ne!(jobs, dir_ino("job"));
}

/// Asserts that a well-formed flattened tree becomes a tree with its entries, kinds, sources,
/// executable bits from the modes, stored modes and the location it was given.
#[test]
fn a_well_formed_flat_tree_becomes_a_tree_with_modes_and_its_location() {
    let last = format!(
        r#"{{"parent":1,"name":"f","mode":493,"kind":"file","size":5,"sha256":"{HEX}","pack":1,"offset":7}}"#
    );
    let tree = tree_of(&flat_with(&last), "a/b".to_owned()).unwrap();
    assert_eq!(tree.len(), 3);
    assert_eq!(tree.location(), Some("a/b"));
    assert_eq!(
        [tree.mode(0), tree.mode(1), tree.mode(2), tree.mode(3)],
        [Some(0o755), Some(0o700), Some(0o755), None]
    );
    assert_eq!(tree.child(0, "d"), Some(1));
    assert_eq!(tree.child(1, "f"), Some(2));
    assert!(matches!(tree.entry(0).unwrap().kind, EntryKind::Dir { .. }));
    match &tree.entry(2).unwrap().kind {
        EntryKind::File {
            digest,
            size,
            exec,
            source: Some(source),
        } => {
            assert_eq!(*digest, TreeDigest::from_hex(HEX).unwrap());
            assert_eq!((*size, *exec), (5, true));
            assert_eq!((&*source.key, source.offset), ("p1", 7));
        }
        other => panic!("{other:?}"),
    }
}

/// Asserts that a plain file has no exec bit, an empty file has no source and a link keeps its
/// target and mode.
#[test]
fn files_without_exec_bits_empty_files_and_links_are_read_as_they_are() {
    let flat = flat(&format!(
        r#"{{"paths":["a/b"],"packs":["p0"],"entries":[
        {{"name":"","mode":493,"kind":"dir"}},
        {{"parent":0,"name":"e","mode":420,"kind":"file","size":0,"sha256":"{HEX}"}},
        {{"parent":0,"name":"l","mode":511,"kind":"symlink","target":"e"}},
        {{"parent":0,"name":"p","mode":420,"kind":"file","size":3,"sha256":"{HEX}","pack":0,"offset":0}}]}}"#
    ));
    let tree = tree_of(&flat, "a/b".to_owned()).unwrap();
    assert!(matches!(
        tree.entry(1).unwrap().kind,
        EntryKind::File {
            size: 0,
            exec: false,
            source: None,
            ..
        }
    ));
    assert!(
        matches!(&tree.entry(2).unwrap().kind, EntryKind::Symlink { target } if &**target == "e")
    );
    assert!(matches!(
        tree.entry(3).unwrap().kind,
        EntryKind::File {
            exec: false,
            source: Some(_),
            ..
        }
    ));
    assert_eq!(tree.mode(2), Some(0o777));
}

/// Asserts that a flattened tree with no entries, which has no root, is refused naming the path.
#[test]
fn a_flat_tree_with_no_entries_is_refused() {
    let why = refused(&flat(r#"{"paths":["a/b"],"packs":[],"entries":[]}"#));
    assert!(why.contains("a/b"), "{why}");
}

/// Asserts that a flattened tree whose first entry is not a root directory with no parent is
/// refused naming the path.
#[test]
fn a_flat_tree_without_a_root_directory_first_is_refused() {
    let file_root = flat(&format!(
        r#"{{"paths":["a/b"],"packs":[],"entries":[{{"name":"","mode":420,"kind":"file","size":0,"sha256":"{HEX}"}}]}}"#
    ));
    let held_root = flat(
        r#"{"paths":["a/b"],"packs":[],"entries":[{"parent":0,"name":"","mode":493,"kind":"dir"}]}"#,
    );
    for tree in [file_root, held_root] {
        let why = refused(&tree);
        assert!(why.contains("a/b"), "{why}");
    }
}

/// Asserts that an entry whose parent comes after it, is itself, is a file, or that has no parent
/// is refused naming the path.
#[test]
fn an_entry_that_is_not_below_a_directory_before_it_is_refused() {
    let file =
        format!(r#"{{"parent":0,"name":"f","mode":420,"kind":"file","size":0,"sha256":"{HEX}"}}"#);
    let root = r#"{"name":"","mode":493,"kind":"dir"}"#;
    for entries in [
        format!(r#"{root},{{"parent":2,"name":"d","mode":493,"kind":"dir"}},{file}"#),
        format!(r#"{root},{{"parent":1,"name":"d","mode":493,"kind":"dir"}}"#),
        format!(r#"{root},{file},{{"parent":1,"name":"x","mode":420,"kind":"dir"}}"#),
        format!(r#"{root},{{"name":"d","mode":493,"kind":"dir"}}"#),
    ] {
        let tree = flat(&format!(
            r#"{{"paths":["a/b"],"packs":[],"entries":[{entries}]}}"#
        ));
        let why = refused(&tree);
        assert!(why.contains("a/b"), "{entries}: {why}");
    }
}

/// Asserts that a file naming a pack past the list, and a file whose digest is not sixty-four
/// lower-case hex digits, are refused naming the path.
#[test]
fn a_file_with_an_unknown_pack_or_a_bad_digest_is_refused() {
    let past = format!(
        r#"{{"parent":1,"name":"f","mode":420,"kind":"file","size":1,"sha256":"{HEX}","pack":2}}"#
    );
    let bad =
        r#"{"parent":1,"name":"f","mode":420,"kind":"file","size":1,"sha256":"xyz","pack":0}"#
            .to_owned();
    let upper = format!(
        r#"{{"parent":1,"name":"f","mode":420,"kind":"file","size":1,"sha256":"{}","pack":0}}"#,
        HEX.to_uppercase()
    );
    for file in [past, bad, upper] {
        let why = refused(&flat_with(&file));
        assert!(why.contains("a/b"), "{why}");
    }
}

/// Asserts that over a tree store with no placements, a lookup in the root of a name that is no
/// location, and one in a directory number nobody knows, are not found.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_that_is_no_location_is_not_found_in_the_root() {
    let live = Live::new("tfs-placed-none").await;
    live.on_core(|core| {
        assert_eq!(core.lookup(ino::ROOT, "jobs").err(), Some(Errno::NoEnt));
        assert_eq!(
            core.lookup(dir_ino("jobs"), "1f3a").err(),
            Some(Errno::NoEnt)
        );
    })
    .await;
}

/// Asserts that a placement whose records the tree store cannot read (the records file of its tree
/// is gone) makes the lookup an I/O error and gives the hold of the placement back, and that a
/// placement the tree store no longer has is not there.
#[tokio::test(flavor = "multi_thread")]
async fn a_placement_with_damaged_records_makes_the_lookup_an_io_error() {
    let live = Live::new("tfs-placed-damaged").await;
    let created = live.place(vec![fs_at("jobs")]);
    let gone = live.place(vec![fs_at("gone")]);
    live.bench.store.release(&gone.placement).unwrap();
    std::fs::remove_file(tree_path(&live.bench.rig, &created.tree).join("records")).unwrap();
    live.on_core(|core| {
        assert_eq!(core.lookup(ino::ROOT, "jobs").err(), Some(Errno::Io));
        assert_eq!(core.lookup(ino::ROOT, "gone").err(), Some(Errno::NoEnt));
    })
    .await;
    let held = live.bench.store.placement(&created.placement).unwrap();
    assert_eq!(held.attachments, 0);
}

/// Asserts that a core with nothing in memory finds the directories above the locations through
/// the tree store's paths of the locations, and that a directory above no location is not there;
/// and that where the tree store's paths cannot be had (the cache stays read-only for the whole
/// wait), a lookup in an unknown directory and its parent are I/O errors.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_directory_above_the_locations_is_found_through_the_node_or_not_at_all() {
    let read_only = |bench: &Bench| {
        let _holder = bench.rig.locked();
        bench.rig.locked()
    };
    let blocked = Live::in_process_with("tfs-placed-mounted-ro", read_only, SHORT).await;
    blocked
        .on_core(|core| {
            assert_eq!(core.lookup(dir_ino("jobs"), "1f3a").err(), Some(Errno::Io));
            assert_eq!(core.parent(dir_ino("jobs")).err(), Some(Errno::Io));
        })
        .await;
    let live = Live::new("tfs-placed-mounted").await;
    let _ = live.place(vec![fs_at("jobs/1f3a/root")]);
    let next = live.successor();
    tokio::task::spawn_blocking(move || {
        assert_eq!(next.parent(dir_ino("jobs/1f3a")), Ok(dir_ino("jobs")));
        assert_eq!(next.parent(dir_ino("jobs")), Ok(ino::ROOT));
        assert_eq!(next.parent(dir_ino("other")).err(), Some(Errno::NoEnt));
    })
    .await
    .unwrap();
}

/// Asserts that a tree of manifests still mounts and reads as before the placements: a lookup of
/// `trees/<digest>` reads its manifests through the cache's fill and a file of it reads its bytes,
/// each filled once from the upstream.
#[test]
fn old_manifest_tree_still_mounts() {
    use crate::server::treefs::testing::file;
    let built = Built::of(vec![("hello", file(b"hello, tree"))]);
    let root = built.root.clone();
    let rig = Rig::new("tfs-placed-old", built);
    let Ok(crate::server::treefs::core::Looked::Found(entry)) = rig.core.lookup(ino::TREES, &root)
    else {
        panic!("the tree is there");
    };
    let Ok(crate::server::treefs::core::Looked::Found(file)) =
        rig.core.lookup(entry.attr.ino, "hello")
    else {
        panic!("the file is there");
    };
    assert_eq!(file.attr.perm, 0o644);
    assert_eq!(
        rig.core.read_ino(file.attr.ino, 0, 100).unwrap(),
        b"hello, tree"
    );
    let asked = rig.upstream.asked("fill");
    assert!(asked.contains(&format!("fill {root}")), "{asked:?}");
    let blob = format!("fill {} 11 from {PACK} @ {PACK_START}", sha(b"hello, tree"));
    assert!(asked.contains(&blob), "{asked:?}");
}
