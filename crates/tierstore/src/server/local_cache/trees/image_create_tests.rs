//! Making a tree of an image: an image staged ahead or written by the create, and the refusals
//! and failures of a create, each leaving nothing behind.

use bytes::Bytes;

use super::files::{IMAGE, READY, placement_file, trees_dir};
use super::image_tests::{explicit, placed, trees_names};
use super::testing::{Bench, built, fs_at, open_image, tree_path};
use super::{CreateError, CreateRequest, ImageRequest, PlacementId, TreesConfig};
use crate::{NewEntry, NewKind, TreeDigest};

/// Asserts that a ready create of a new image writes the ready marker beside the image, whether the
/// image was written by the create or staged before it.
#[tokio::test]
async fn a_ready_image_is_marked_ready_when_written_or_staged() {
    let bench = Bench::new("image-ready").await;
    bench.fill_all().await;
    let written = bench
        .store
        .create_image(ImageRequest {
            ready: true,
            ..bench.image_request(vec![fs_at("w")])
        })
        .unwrap();
    assert!(tree_path(&bench.rig, &written.tree).join(READY).is_file());
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);

    let other = Bench::new("image-ready-staged").await;
    let bytes = other.image_bytes();
    let staged = other.store.stage_image(bytes.clone()).unwrap();
    let made = other
        .store
        .create_image(explicit(
            TreeDigest::of(&bytes),
            bytes,
            vec![fs_at("s")],
            true,
            Some(staged),
        ))
        .unwrap();
    let dir = tree_path(&other.rig, &made.tree);
    assert!(dir.join(READY).is_file() && dir.join(IMAGE).is_file());
}

/// Asserts that an image staged ahead is renamed into place by the create of its tree when the
/// image was opened from the same buffer: the file is the image, the staging name is gone, the
/// directory of the tree is the one staged (the file written is the one the stage wrote), and the
/// tree is read back at a restart.
#[tokio::test]
async fn a_staged_image_of_the_same_buffer_is_renamed_into_place() {
    let bench = Bench::new("image-staged").await;
    let bytes = bench.image_bytes();
    let staged = bench.store.stage_image(bytes.clone()).unwrap();
    let staging = trees_dir(&bench.rig.config.path).join(".new-0");
    assert_eq!(std::fs::read(staging.join(IMAGE)).unwrap(), bytes);
    std::fs::write(staging.join("probe"), b"staged").unwrap();
    let made = bench
        .store
        .create_image(explicit(
            TreeDigest::of(&bytes),
            bytes.clone(),
            vec![fs_at("s")],
            false,
            Some(staged),
        ))
        .unwrap();
    assert!(made.new_tree);
    assert!(!staging.exists());
    let dir = tree_path(&bench.rig, &made.tree);
    assert_eq!(std::fs::read(dir.join(IMAGE)).unwrap(), bytes);
    assert_eq!(std::fs::read(dir.join("probe")).unwrap(), b"staged");
    assert_eq!(trees_names(&bench), vec![made.tree.to_string()]);
    let store = Bench::reopen(bench.rig.reopen());
    assert!(store.resolve(&made.placement, "bin/tool").is_ok());
}

/// Asserts that an image staged ahead whose staging directory is gone by the time of its create
/// (a read of the directory when the cache takes its lock again removes what is staged) does not
/// fail the create: the create writes the image itself, the tree is placed and read back at a
/// restart, and nothing is left staged.
#[tokio::test]
async fn a_staged_image_removed_before_its_create_is_written_by_the_create() {
    let bench = Bench::new("image-staged-gone").await;
    let bytes = bench.image_bytes();
    let staged = bench.store.stage_image(bytes.clone()).unwrap();
    let staging = trees_dir(&bench.rig.config.path).join(".new-0");
    std::fs::remove_dir_all(&staging).unwrap();
    let made = bench
        .store
        .create_image(explicit(
            TreeDigest::of(&bytes),
            bytes.clone(),
            vec![fs_at("s")],
            false,
            Some(staged),
        ))
        .unwrap();
    assert!(made.new_tree);
    let dir = tree_path(&bench.rig, &made.tree);
    assert_eq!(std::fs::read(dir.join(IMAGE)).unwrap(), bytes);
    assert_eq!(trees_names(&bench), vec![made.tree.to_string()]);
    let store = Bench::reopen(bench.rig.reopen());
    assert!(store.resolve(&made.placement, "bin/tool").is_ok());
}

/// Asserts that an image staged from another buffer than the image was opened from (the same
/// bytes, copied) or as a prefix of the buffer (the same pointer, a shorter length) is not used:
/// the create writes the image itself and the staging directory is removed.
#[tokio::test]
async fn a_staged_image_of_another_buffer_is_removed_and_the_image_written() {
    let bench = Bench::new("image-staged-other").await;
    let bytes = bench.image_bytes();
    let copy = Bytes::copy_from_slice(&bytes);
    let staged = bench.store.stage_image(copy).unwrap();
    let made = bench
        .store
        .create_image(explicit(
            TreeDigest::of(&bytes),
            bytes.clone(),
            vec![fs_at("s")],
            false,
            Some(staged),
        ))
        .unwrap();
    assert_eq!(trees_names(&bench), vec![made.tree.to_string()]);
    let dir = tree_path(&bench.rig, &made.tree);
    assert_eq!(std::fs::read(dir.join(IMAGE)).unwrap(), bytes);

    let bench = Bench::new("image-staged-short").await;
    let staged = bench
        .store
        .stage_image(bytes.slice(..bytes.len() - 1))
        .unwrap();
    let made = bench
        .store
        .create_image(explicit(
            TreeDigest::of(&bytes),
            bytes.clone(),
            vec![fs_at("s")],
            false,
            Some(staged),
        ))
        .unwrap();
    assert_eq!(trees_names(&bench), vec![made.tree.to_string()]);
    let image = std::fs::read(tree_path(&bench.rig, &made.tree).join(IMAGE)).unwrap();
    assert_eq!(image, bytes);
}

/// Asserts that a staged image no create takes removes its directory when it is dropped, that one
/// whose create is of a tree the node has (so writes nothing) is removed too, and that one whose
/// create is refused is.
#[tokio::test]
async fn a_staged_image_that_is_not_used_removes_its_directory_when_dropped() {
    let bench = Bench::new("image-staged-drop").await;
    let staged = bench.store.stage_image(bench.image_bytes()).unwrap();
    assert_eq!(trees_names(&bench), vec![".new-0".to_owned()]);
    drop(staged);
    assert!(trees_names(&bench).is_empty());

    let first = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    let bytes = bench.image_bytes();
    let staged = bench.store.stage_image(bytes.clone()).unwrap();
    assert_eq!(trees_names(&bench).len(), 2);
    bench
        .store
        .create_image(explicit(
            first.tree,
            bytes.clone(),
            vec![fs_at("b")],
            false,
            Some(staged),
        ))
        .unwrap();
    assert_eq!(trees_names(&bench), vec![first.tree.to_string()]);

    let staged = bench.store.stage_image(bytes.clone()).unwrap();
    let refused = bench
        .store
        .create_image(explicit(first.tree, bytes, Vec::new(), false, Some(staged)))
        .unwrap_err();
    assert_eq!(refused, CreateError::NoLocation);
    assert_eq!(trees_names(&bench), vec![first.tree.to_string()]);
}

/// Asserts that staging is refused with trees off (`Disabled`) and in a store that does not hold
/// the cache's lock (`ReadOnly`), each leaving nothing in the directory, and that a stage whose
/// directory cannot be made (the `trees` path is a file) says so and leaves nothing.
#[tokio::test]
async fn stage_image_is_refused_when_off_read_only_or_unwritable() {
    let off = TreesConfig {
        enabled: false,
        ..TreesConfig::default()
    };
    let bench = Bench::with("image-stage-off", off).await;
    let refused = bench.store.stage_image(bench.image_bytes()).unwrap_err();
    assert_eq!(refused, CreateError::Disabled);
    assert!(!bench.rig.config.path.join("trees").exists());
    let bench = Bench::new("image-stage-ro").await;
    let holder = bench.rig.locked();
    let waiting = bench.rig.locked();
    let store = Bench::reopen(waiting);
    assert_eq!(
        store.stage_image(bench.image_bytes()).unwrap_err(),
        CreateError::ReadOnly
    );
    assert!(trees_names(&bench).is_empty());
    drop(holder);
    let bench = Bench::new("image-stage-io").await;
    let trees = trees_dir(&bench.rig.config.path);
    std::fs::remove_dir_all(&trees).unwrap();
    std::fs::write(&trees, b"a file").unwrap();
    let refused = bench.store.stage_image(bench.image_bytes()).unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert_eq!(std::fs::read(&trees).unwrap(), b"a file");
}

/// Asserts the refusals of a create of an image: trees off, no location, a tree of more file bytes
/// than `trees.max_bytes` (by the bytes the image counts),
/// and a cache with no lock; none leaves a tree or a placement.
#[tokio::test]
async fn create_image_refusals_are_by_kind_and_leave_nothing() {
    let off = TreesConfig {
        enabled: false,
        ..TreesConfig::default()
    };
    let bench = Bench::with("image-refuse-off", off).await;
    let refused = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap_err();
    assert_eq!(refused, CreateError::Disabled);

    let bench = Bench::new("image-refuse-none").await;
    let refused = bench
        .store
        .create_image(bench.image_request(Vec::new()))
        .unwrap_err();
    assert_eq!(refused, CreateError::NoLocation);

    let small = TreesConfig {
        max_bytes: 5999,
        ..TreesConfig::default()
    };
    let bench = Bench::with("image-refuse-large", small).await;
    let large = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap_err();
    assert_eq!(
        large,
        CreateError::TooLarge {
            bytes: 6000,
            max: 5999
        }
    );

    let bench = Bench::new("image-refuse-ro").await;
    let holder = bench.rig.locked();
    let store = Bench::reopen(bench.rig.locked());
    let refused = store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap_err();
    assert_eq!(refused, CreateError::ReadOnly);
    assert_eq!(store.sizes(), (0, 0));
    drop(holder);
}

/// Asserts that a tree of records may not hold an image tree by its digest: the create is refused
/// with `UnknownChild` of that digest, and nothing is made.
#[tokio::test]
async fn a_records_tree_may_not_hold_an_image_tree_by_digest() {
    let (bench, _, image) = placed("image-child").await;
    let holder = built(vec![NewEntry {
        path: "below".to_owned(),
        kind: NewKind::Tree { digest: image },
    }]);
    let refused = bench
        .store
        .create(CreateRequest {
            built: holder,
            at: vec![fs_at("holder")],
            ttl: None,
            ready: false,
        })
        .unwrap_err();
    assert_eq!(refused, CreateError::UnknownChild(image));
    assert_eq!(bench.store.sizes(), (1, 1));
}

/// Asserts that a create that cannot write the tree's directory says so and leaves no staging
/// directory and no tree: the directory of the tree is taken by a full directory (the rename
/// fails), for an image written by the create and for one staged.
#[tokio::test]
async fn a_create_image_whose_directory_cannot_be_settled_is_an_io_error() {
    let bench = Bench::new("image-settle").await;
    let bytes = bench.image_bytes();
    let digest = TreeDigest::of(&bytes);
    let taken = tree_path(&bench.rig, &digest);
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("keep"), b"x").unwrap();

    let refused = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert_eq!(trees_names(&bench), vec![digest.to_string()]);

    let staged = bench.store.stage_image(bytes.clone()).unwrap();
    let refused = bench
        .store
        .create_image(ImageRequest {
            digest,
            image: open_image(bytes),
            at: vec![fs_at("b")],
            ttl: None,
            ready: true,
            staged: Some(staged),
        })
        .unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert_eq!(trees_names(&bench), vec![digest.to_string()]);
    assert_eq!(bench.store.sizes(), (0, 0));
}

/// Asserts that a create whose directory cannot be staged (the `trees` path is a file) is an io
/// error and makes no placement.
#[tokio::test]
async fn a_create_image_whose_staging_cannot_be_written_is_an_io_error() {
    let bench = Bench::new("image-stage-fail").await;
    let trees = trees_dir(&bench.rig.config.path);
    std::fs::remove_dir_all(&trees).unwrap();
    std::fs::write(&trees, b"a file").unwrap();
    let refused = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert_eq!(bench.store.sizes(), (0, 0));
}

/// Asserts that a create whose placement file cannot be written (a directory is where it goes)
/// is an io error and drops the image tree it made, directory included.
#[tokio::test]
async fn a_create_image_whose_placement_cannot_be_written_drops_its_tree() {
    let bench = Bench::new("image-placement-fail").await;
    let request = bench.image_request(vec![fs_at("a")]);
    let id = PlacementId::of(&request.digest, &request.at);
    let file = placement_file(&bench.rig.config.path, &id);
    std::fs::create_dir_all(&file).unwrap();
    let digest = request.digest;
    let refused = bench.store.create_image(request).unwrap_err();
    assert!(matches!(refused, CreateError::Io(_)), "{refused:?}");
    assert_eq!(bench.store.sizes(), (0, 0));
    assert!(!tree_path(&bench.rig, &digest).exists());
}

/// Asserts that a staged image is printed as its length and its staging directory, never its
/// bytes.
#[tokio::test]
async fn a_staged_image_is_debug_printed_without_its_bytes() {
    let bench = Bench::new("image-staged-debug").await;
    let bytes = bench.image_bytes();
    let staged = bench.store.stage_image(bytes.clone()).unwrap();
    let dir = trees_dir(&bench.rig.config.path).join(".new-0");
    assert_eq!(
        format!("{staged:?}"),
        format!(
            "StagedImage {{ bytes: {}, dir: Some({dir:?}) }}",
            bytes.len()
        )
    );
}
