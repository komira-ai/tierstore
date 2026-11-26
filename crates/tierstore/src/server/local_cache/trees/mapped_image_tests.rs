//! The images of the tree store as mapped lists: the store maps the list of a tree it makes, or
//! reads at a start, from the tree's image file, once however many placements show it, and the
//! file it maps is never changed in place: a second create of the tree leaves it as it is, and a
//! tree the store drops releases its mapping with its last holder, which reads it to the end.

use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;

use super::files::IMAGE;
use super::testing::{Bench, fs_at, tree_path};
use super::{ImageRequest, Mapped};
use crate::tree_image_list::in_pages;

/// The pages the bench's image takes mapped.
fn pages(bench: &Bench) -> u64 {
    in_pages(bench.image_bytes().len() as u64)
}

/// Asserts that a create of an image maps the list from the tree's image file, and that a second
/// placement of the same tree shares that mapping: one list, its pages counted once.
#[tokio::test]
async fn create_image_maps_the_list_from_its_file_once_for_every_placement() {
    let bench = Bench::new("image-mapped-once").await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    let lists = bench.store.lists();
    let first = lists.get(&made.tree).unwrap();
    assert!(first.is_mapped());
    let one = Mapped {
        lists: 1,
        bytes: pages(&bench),
    };
    assert_eq!(lists.mapped(), one);
    let again = bench
        .store
        .create_image(bench.image_request(vec![fs_at("b")]))
        .unwrap();
    assert_eq!(again.tree, made.tree);
    assert!(!again.new_tree);
    assert!(Arc::ptr_eq(&lists.get(&made.tree).unwrap(), &first));
    assert_eq!(lists.mapped(), one);
}

/// Asserts that a second create of a tree whose image is mapped, its image staged anew, leaves the
/// image file as it is (the same file, the same bytes) and removes what it staged: the staged
/// image is never renamed over, nor written into, the file a mapping reads, which reads as before.
#[tokio::test]
async fn a_second_create_of_a_mapped_image_tree_leaves_its_file_as_it_is() {
    let bench = Bench::new("image-mapped-again").await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    let file = tree_path(&bench.rig, &made.tree).join(IMAGE);
    let before = std::fs::metadata(&file).unwrap();
    let mapped = bench.store.lists().get(&made.tree).unwrap();
    let entries: Vec<_> = (0..mapped.len() as u32)
        .map(|at| mapped.entry(at))
        .collect();
    let staged = bench.store.stage_image(bench.image_bytes()).unwrap();
    let again = bench
        .store
        .create_image(ImageRequest {
            staged: Some(staged),
            ..bench.image_request(vec![fs_at("b")])
        })
        .unwrap();
    assert_eq!(again.tree, made.tree);
    let after = std::fs::metadata(&file).unwrap();
    assert_eq!(
        (after.ino(), after.len(), after.mtime_nsec()),
        (before.ino(), before.len(), before.mtime_nsec())
    );
    assert_eq!(std::fs::read(&file).unwrap(), bench.image_bytes());
    assert_eq!(
        super::image_tests::trees_names(&bench),
        vec![made.tree.to_string()]
    );
    let read: Vec<_> = (0..mapped.len() as u32)
        .map(|at| mapped.entry(at))
        .collect();
    assert_eq!(read, entries);
}

/// Asserts that a restart reads an image tree mapped from its file, its pages counted once.
#[tokio::test]
async fn a_restart_maps_each_image_from_its_file() {
    let bench = Bench::new("image-mapped-restart").await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    let store = Bench::reopen(bench.rig.reopen());
    let mapped = store.lists().get(&made.tree).unwrap();
    assert!(mapped.is_mapped());
    assert_eq!(
        store.lists().mapped(),
        Mapped {
            lists: 1,
            bytes: pages(&bench),
        }
    );
    assert_eq!(
        mapped.find("bin/tool"),
        bench
            .store
            .lists()
            .get(&made.tree)
            .unwrap()
            .find("bin/tool")
    );
}

/// Asserts that a tree the store drops (its placement released and swept) takes its directory off
/// the disk while a holder of its list still reads it through the mapping, and that the mapping
/// is released, and no longer counted, with that last holder.
#[tokio::test]
async fn a_dropped_image_tree_is_read_by_its_last_holder_and_released_with_it() {
    let bench = Bench::new("image-mapped-dropped").await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    let holder = bench.store.lists().get(&made.tree).unwrap();
    let tool = holder.find("bin/tool");
    bench.store.release(&made.placement).unwrap();
    let _ = bench.store.sweep();
    assert!(!tree_path(&bench.rig, &made.tree).exists());
    assert_eq!(bench.store.lists().mapped().lists, 1);
    assert_eq!(holder.find("bin/tool"), tool);
    assert!(tool.is_ok());
    drop(holder);
    assert_eq!(bench.store.lists().mapped(), Mapped::default());
    assert!(bench.store.lists().get(&made.tree).is_none());
}
