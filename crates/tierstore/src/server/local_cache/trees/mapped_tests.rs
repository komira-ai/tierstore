//! Tests of the registry of the lists in memory: one list a digest however many keep it, counted
//! once and in whole pages, forgotten with its last holder, and what a list not in memory needs.

use std::sync::Arc;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;

use super::*;
use crate::server::treefs::testing_image::image_named;
use crate::tree_image_list::in_pages;
use crate::{TreeImage, TreeListV2Error};

/// The list named `name` (`image_named`), its digest, and its image file in `dir`.
fn list(dir: &TestDir, name: &str) -> (TreeDigest, std::path::PathBuf, Vec<u8>) {
    let bytes = image_named(name);
    let path = dir.join(name);
    std::fs::write(&path, &bytes).unwrap();
    (TreeDigest::of(&bytes), path, bytes)
}

fn scratch() -> TestDir {
    TestDir::under(std::env::temp_dir(), "trees-mapped")
}

/// Asserts that a list opened twice is mapped once and the same list is given to both, counted
/// once in whole pages; that a second list adds its own; and that a list leaves the registry, and
/// its count, with its last holder.
#[test]
fn a_list_is_mapped_once_counted_once_and_forgotten_with_its_last_holder() {
    let dir = scratch();
    let lists = MappedLists::default();
    let (a, path_a, bytes_a) = list(&dir, "da");
    let (b, path_b, _) = list(&dir, "db");
    let page = in_pages(bytes_a.len() as u64);

    let first = lists.open(&a, &path_a).unwrap();
    let again = lists.open(&a, &path_a).unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert!(first.is_mapped());
    assert_eq!(
        lists.mapped(),
        Mapped {
            lists: 1,
            bytes: page
        }
    );
    let second = lists.open(&b, &path_b).unwrap();
    assert_eq!(
        lists.mapped(),
        Mapped {
            lists: 2,
            bytes: 2 * page
        }
    );

    drop(first);
    assert_eq!(lists.mapped().lists, 2);
    drop(again);
    assert_eq!(
        lists.mapped(),
        Mapped {
            lists: 1,
            bytes: page
        }
    );
    assert!(lists.get(&a).is_none());
    assert!(Arc::ptr_eq(&lists.get(&b).unwrap(), &second));
    drop(second);
    assert_eq!(lists.mapped(), Mapped::default());
}

/// Asserts that a list kept while another of its digest is held gives the one held, and the new
/// one is dropped; and that a list on the heap is shared but counted as no mapping.
#[test]
fn keeping_a_list_held_already_gives_the_one_held_and_a_heap_list_counts_nothing() {
    let dir = scratch();
    let lists = MappedLists::default();
    let (a, path_a, bytes_a) = list(&dir, "da");
    let image = TreeImage::checked(Bytes::from(bytes_a)).unwrap();
    let mapped = lists.keep(a, ListImage::stored(&image, &path_a));
    let other = lists.keep(a, ListImage::heap(&image));
    assert!(Arc::ptr_eq(&mapped, &other));
    drop((mapped, other));

    let heap = lists.keep(a, ListImage::heap(&image));
    assert!(!heap.is_mapped());
    assert!(Arc::ptr_eq(&lists.get(&a).unwrap(), &heap));
    assert_eq!(lists.mapped(), Mapped::default());
}

/// Asserts what a list needs before it is mapped: nothing while it is in memory, its file's
/// length in whole pages while it is not, and nothing for a file that is not there (which is not
/// mapped either: its open fails).
#[test]
fn a_list_needs_its_pages_unless_it_is_in_memory() {
    let dir = scratch();
    let lists = MappedLists::default();
    let (a, path_a, bytes_a) = list(&dir, "da");
    let page = in_pages(bytes_a.len() as u64);
    assert_eq!(lists.need(&a, &path_a), page);
    let held = lists.open(&a, &path_a).unwrap();
    assert_eq!(lists.need(&a, &path_a), 0);
    drop(held);
    assert_eq!(lists.need(&a, &path_a), page);

    let gone = dir.join("gone");
    assert_eq!(lists.need(&a, &gone), 0);
    assert!(lists.open(&TreeDigest::of(b"gone"), &gone).is_err());
}

/// Asserts that a file that is no list is not kept: the walk's refusal is given, and nothing is
/// counted.
#[test]
fn a_file_that_is_no_list_is_refused_and_not_kept() {
    let dir = scratch();
    let lists = MappedLists::default();
    let path = dir.join("junk");
    std::fs::write(
        &path,
        b"not a tree list at all, but long enough to hold a header....",
    )
    .unwrap();
    let tree = TreeDigest::of(b"junk");
    let refused = lists.open(&tree, &path).unwrap_err();
    assert_eq!(refused.to_string(), TreeListV2Error::BadMagic.to_string());
    assert!(lists.get(&tree).is_none());
    assert_eq!(lists.mapped(), Mapped::default());
}
