//! The tree store's trees that are version 2 tree lists (images): making one and staging its
//! file, answering paths from it, flattening it for the mount, and reading it back at a start.

use bytes::Bytes;

use super::files::{IMAGE, READY, RECORDS, placement_file, trees_dir};
use super::flat::{FlatEntry, FlatError, FlatKind, MountTree};
use super::read::{FileSpan, Kind, Listing, Resolved, Stat};
use super::testing::{Bench, built, fs_at, open_image, s3_at, tree_path};
use super::{
    CreateError, CreateRequest, ImageRequest, Location, PlacementId, ReadError, StagedImage,
    TreesConfig,
};
use crate::{Mode, NewEntry, NewKind, RecordChild, TreeDigest};

const HOUR: u64 = 3600 * 1000;

/// A request for the tree `digest` of the image `bytes` at `at`, with the staged file `staged`.
fn explicit(
    digest: TreeDigest,
    bytes: Bytes,
    at: Vec<Location>,
    ready: bool,
    staged: Option<StagedImage>,
) -> ImageRequest {
    ImageRequest {
        digest,
        image: open_image(bytes),
        at,
        ttl: None,
        ready,
        staged,
    }
}

fn zero() -> TreeDigest {
    TreeDigest::from_bytes([0; 32])
}

/// The names in `dir` of the trees directory, sorted.
fn trees_names(bench: &Bench) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(trees_dir(&bench.rig.config.path))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A bench with the small tree placed as an image at `fs_at("img")`, and its placement.
async fn placed(name: &str) -> (Bench, PlacementId, TreeDigest) {
    let bench = Bench::new(name).await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("img")]))
        .unwrap();
    (bench, made.placement, made.tree)
}

fn names_of(listing: &Listing) -> Vec<&str> {
    listing.entries.iter().map(RecordChild::name).collect()
}

/// Asserts that a create of an image names the tree by the digest of the request, reports the
/// files, bytes and packs of the list, writes the list as `image` in the tree's directory with no
/// records file and no ready marker, writes the placement file, pins the pack once, leaves nothing
/// in staging, and counts the tree and the placement in the store.
#[tokio::test]
async fn create_image_writes_the_list_as_the_tree_and_pins_its_pack() {
    let bench = Bench::new("image-create").await;
    bench.fill_all().await;
    let request = bench.image_request(vec![fs_at("jobs/1"), s3_at("jobs/1/")]);
    let digest = request.digest;
    let at = request.at.clone();
    let made = bench.store.create_image(request).unwrap();
    assert_eq!(made.tree, digest);
    assert_eq!(made.placement, PlacementId::of(&digest, &at));
    assert!(made.created && made.new_tree);
    assert_eq!((made.files, made.bytes, made.packs), (4, 6000, 1));
    assert_eq!(made.expires_ms, 4 * HOUR);
    assert_eq!(
        (made.pins.pinned, made.pins.refused, made.pins.absent),
        (1, 0, 0)
    );
    let dir = tree_path(&bench.rig, &digest);
    assert_eq!(std::fs::read(dir.join(IMAGE)).unwrap(), bench.image_bytes());
    assert!(!dir.join(RECORDS).exists() && !dir.join(READY).exists());
    assert!(placement_file(&bench.rig.config.path, &made.placement).is_file());
    assert_eq!(trees_names(&bench), vec![digest.to_string()]);
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);
    assert_eq!(bench.store.sizes(), (1, 1));
}

/// Asserts that an image tree and a records tree of the same entries answer every path alike:
/// resolve and stat equal for every path and for paths that lead nowhere or below a file, and
/// every page of every directory equal (but for the digest of a directory child, which an image
/// does not have and says as all zeros).
#[tokio::test]
async fn an_image_tree_and_a_records_tree_of_the_same_entries_resolve_alike() {
    let bench = Bench::new("image-alike").await;
    let records = bench
        .store
        .create(bench.request(vec![fs_at("records")]))
        .unwrap()
        .placement;
    let image = bench
        .store
        .create_image(bench.image_request(vec![fs_at("image")]))
        .unwrap()
        .placement;
    let store = &bench.store;
    let paths = "|bin|bin/tool|bin/other|lib|lib/deep|lib/deep/data|empty|link|zero|missing|bin/missing|bin/tool/below|zero/below|link/below".split('|');
    for path in paths {
        assert_eq!(
            store.resolve(&records, path),
            store.resolve(&image, path),
            "resolve {path:?}"
        );
        assert_eq!(
            store.stat(&records, path),
            store.stat(&image, path),
            "stat {path:?}"
        );
        for after in [None, Some(""), Some("bin"), Some("empty"), Some("zzz")] {
            for limit in [0, 1, 2, 100] {
                let flatten = |listing: Result<Listing, ReadError>| {
                    listing.map(|mut listing| {
                        for entry in &mut listing.entries {
                            if let RecordChild::Dir { digest, .. } = entry {
                                *digest = zero();
                            }
                        }
                        listing
                    })
                };
                assert_eq!(
                    flatten(store.list(&records, path, after, limit)),
                    flatten(store.list(&image, path, after, limit)),
                    "list {path:?} after {after:?} limit {limit}"
                );
            }
        }
    }
}

/// Asserts the exact answers of resolve and stat for an image: a directory says its mode and how
/// many children it has (the root, an empty one) and is of size 0, a file its mode (setuid kept),
/// size, digest and the pack and offset of its bytes, an empty file no span, and a link its target
/// and, for stat, the length of the target and the mode 0777; and that paths that lead nowhere are
/// refused by kind: a name the directory does not have is not found, a name below a file or a link
/// is not a directory, and a placement that is not there has none.
#[tokio::test]
async fn resolve_and_stat_of_an_image_say_every_kind_and_refuse_paths_by_kind() {
    let (bench, id, _) = placed("image-resolve").await;
    let store = &bench.store;
    let at = |n: usize| {
        let (sha256, offset, size) = &bench.pack.blobs[n];
        let span = FileSpan {
            pack: bench.pack.key.clone(),
            offset: *offset,
        };
        (TreeDigest::from_hex(sha256).unwrap(), Some(span), *size)
    };
    let dir = |entries| Resolved::Dir {
        mode: Mode::DIR,
        entries,
    };
    let file = |mode: u16, (sha256, span, size)| Resolved::File {
        mode: Mode::new(mode).unwrap(),
        size,
        sha256,
        span,
    };
    let link = Resolved::Symlink {
        target: "bin/tool".to_owned(),
    };
    let zero_file = file(0o644, (TreeDigest::of(b""), None, 0));
    let wanted = [
        ("", dir(5)),
        ("empty", dir(0)),
        ("lib/deep", dir(1)),
        ("bin/tool", file(0o4755, at(0))),
        ("lib/deep/data", file(0o644, at(2))),
        ("zero", zero_file),
        ("link", link),
    ];
    for (path, resolved) in wanted {
        assert_eq!(store.resolve(&id, path).unwrap(), resolved, "{path:?}");
    }
    let stat = |kind, size, mode| Stat { kind, size, mode };
    assert_eq!(
        store.stat(&id, "bin").unwrap(),
        stat(Kind::Dir, 0, Mode::DIR)
    );
    let tool = Mode::new(0o4755).unwrap();
    assert_eq!(
        store.stat(&id, "bin/tool").unwrap(),
        stat(Kind::File, 1000, tool)
    );
    assert_eq!(
        store.stat(&id, "link").unwrap(),
        stat(Kind::Symlink, 8, Mode::SYMLINK)
    );

    let (missing, below) = (ReadError::NotFound, ReadError::NotADirectory);
    assert_eq!(store.resolve(&id, "nope").unwrap_err(), missing);
    assert_eq!(store.stat(&id, "bin/nope").unwrap_err(), missing);
    assert_eq!(store.list(&id, "nope", None, 10).unwrap_err(), missing);
    assert_eq!(store.resolve(&id, "bin/tool/x").unwrap_err(), below);
    assert_eq!(store.stat(&id, "link/x").unwrap_err(), below);
    assert_eq!(store.list(&id, "bin/tool", None, 10).unwrap_err(), below);
    assert_eq!(store.list(&id, "link", None, 10).unwrap_err(), below);
    let other = PlacementId::of(&TreeDigest::of(b"x"), &[fs_at("x")]);
    let none = ReadError::NoPlacement;
    assert_eq!(store.resolve(&other, "").unwrap_err(), none);
    assert_eq!(store.list(&other, "", None, 1).unwrap_err(), none);
}

/// Asserts the listing of an image: every child in name order with its kind, a directory child
/// carrying the all-zero digest, a file its mode, size and digest, a link its target; pages of
/// one and of two with the name to go on from (none on the last page), a limit of 0 taken as 1,
/// `after` a name that is there or one that is not, `after` past the last name giving an empty
/// page with no next, and a directory with no children giving an empty page.
#[tokio::test]
async fn list_of_an_image_pages_in_name_order_with_zero_digest_directories() {
    let (bench, id, _) = placed("image-list").await;
    let store = &bench.store;
    let all = store.list(&id, "", None, 100).unwrap();
    assert_eq!(names_of(&all), vec!["bin", "empty", "lib", "link", "zero"]);
    assert_eq!(all.next, None);
    assert_eq!(
        all.entries,
        vec![
            RecordChild::Dir {
                name: "bin".to_owned(),
                digest: zero()
            },
            RecordChild::Dir {
                name: "empty".to_owned(),
                digest: zero()
            },
            RecordChild::Dir {
                name: "lib".to_owned(),
                digest: zero()
            },
            RecordChild::Symlink {
                name: "link".to_owned(),
                target: "bin/tool".to_owned()
            },
            RecordChild::File {
                name: "zero".to_owned(),
                mode: Mode::new(0o644).unwrap(),
                size: 0,
                sha256: TreeDigest::of(b"")
            },
        ]
    );
    let bin = store.list(&id, "bin", None, 100).unwrap();
    let (sha256, _, size) = &bench.pack.blobs[0];
    assert_eq!(
        bin.entries[1],
        RecordChild::File {
            name: "tool".to_owned(),
            mode: Mode::new(0o4755).unwrap(),
            size: *size,
            sha256: TreeDigest::from_hex(sha256).unwrap()
        }
    );

    let first = store.list(&id, "", None, 2).unwrap();
    assert_eq!(names_of(&first), vec!["bin", "empty"]);
    assert_eq!(first.next.as_deref(), Some("empty"));
    let second = store.list(&id, "", first.next.as_deref(), 2).unwrap();
    assert_eq!(names_of(&second), vec!["lib", "link"]);
    assert_eq!(second.next.as_deref(), Some("link"));
    let third = store.list(&id, "", second.next.as_deref(), 2).unwrap();
    assert_eq!(names_of(&third), vec!["zero"]);
    assert_eq!(third.next, None);
    let one = store.list(&id, "", None, 0).unwrap();
    assert_eq!(names_of(&one), vec!["bin"]);
    assert_eq!(one.next.as_deref(), Some("bin"));
    let between = store.list(&id, "", Some("c"), 100).unwrap();
    assert_eq!(names_of(&between), vec!["empty", "lib", "link", "zero"]);
    let past = store.list(&id, "", Some("zzz"), 100).unwrap();
    assert_eq!((past.entries.len(), past.next), (0, None));
    let empty = store.list(&id, "empty", None, 100).unwrap();
    assert_eq!((empty.entries.len(), empty.next), (0, None));
}

/// Asserts that a create of the same image at the same locations again is a refresh: it is not
/// created and the node had the tree, the lease moves to the later time, the files and packs are
/// reported as before, a ready create marks the tree ready, and nothing new is in the directory.
#[tokio::test]
async fn create_image_again_at_the_same_places_refreshes_the_lease() {
    let bench = Bench::new("image-refresh").await;
    bench.fill_all().await;
    let first = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    bench.rig.clock.advance(HOUR);
    let again = bench
        .store
        .create_image(ImageRequest {
            ready: true,
            ..bench.image_request(vec![fs_at("a")])
        })
        .unwrap();
    assert_eq!(again.placement, first.placement);
    assert!(!again.created && !again.new_tree);
    assert_eq!(again.expires_ms, 5 * HOUR);
    assert_eq!((again.files, again.bytes, again.packs), (4, 6000, 1));
    assert!(tree_path(&bench.rig, &first.tree).join(READY).is_file());
    assert_eq!(bench.store.sizes(), (1, 1));
    assert_eq!(trees_names(&bench).len(), 1);
}

/// Asserts that a second placement of an image the node has is a new placement of the same tree:
/// it is created and the tree is not new, the file of the image is not written again, and a ready
/// create marks the tree ready.
#[tokio::test]
async fn a_second_placement_of_an_image_shares_the_tree() {
    let bench = Bench::new("image-second").await;
    let first = bench
        .store
        .create_image(bench.image_request(vec![fs_at("a")]))
        .unwrap();
    let image = tree_path(&bench.rig, &first.tree).join(IMAGE);
    std::fs::write(&image, b"marker").unwrap();
    let second = bench
        .store
        .create_image(ImageRequest {
            ready: true,
            ..bench.image_request(vec![fs_at("b")])
        })
        .unwrap();
    assert!(second.created && !second.new_tree);
    assert_ne!(second.placement, first.placement);
    assert_eq!(std::fs::read(&image).unwrap(), b"marker");
    assert!(tree_path(&bench.rig, &first.tree).join(READY).is_file());
    assert_eq!(bench.store.sizes(), (2, 1));
}

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

/// Asserts that the packs of an image tree are the keys of its list and those of a records tree
/// the keys of its sources, by what pinning each does: one pack pinned for either, however many
/// files, and that pinning an image tree again by its digest gives that answer, and an image of no
/// file pins nothing.
#[tokio::test]
async fn pinning_an_image_tree_pins_the_packs_of_its_list() {
    let bench = Bench::new("image-pin").await;
    bench.fill_all().await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("img")]))
        .unwrap();
    assert_eq!(bench.rig.cache.pinned_bytes(), 6000);
    let again = bench.store.pin(&made.tree).unwrap();
    assert_eq!((again.pinned, again.refused, again.absent), (1, 0, 0));
    {
        let state = bench.store.shared.lock();
        assert_eq!(
            state.trees[&made.tree].packs(),
            vec![bench.pack.key.as_str()]
        );
    }
    let records = bench
        .store
        .create(bench.request(vec![fs_at("rec")]))
        .unwrap();
    {
        let state = bench.store.shared.lock();
        assert_eq!(
            state.trees[&records.tree].packs(),
            vec![bench.pack.key.as_str()]
        );
    }
    assert_eq!((records.packs, records.pins.pinned), (1, 1));

    let none = crate::TreeList {
        entries: vec![crate::ListEntry {
            path: "d".to_owned(),
            kind: crate::ListKind::Dir { mode: Mode::DIR },
        }],
    };
    let bytes = Bytes::from(none.encode_v2().unwrap());
    let empty = bench
        .store
        .create_image(explicit(
            TreeDigest::of(&bytes),
            bytes,
            vec![fs_at("none")],
            false,
            None,
        ))
        .unwrap();
    assert_eq!((empty.files, empty.bytes, empty.packs), (0, 0, 0));
    assert_eq!(empty.pins.pinned, 0);
    {
        let state = bench.store.shared.lock();
        assert!(state.trees[&empty.tree].packs().is_empty());
    }
}

/// Asserts that a placement of an image is given to the mount as the image by its digest and the
/// fs locations (the s3 one left out), a placement of records as the tree flattened, and that a
/// placement that has no fs location is refused as no placement.
#[tokio::test]
async fn mount_tree_is_the_image_for_an_image_and_the_flat_tree_for_records() {
    let bench = Bench::new("image-mount").await;
    let image = bench
        .store
        .create_image(bench.image_request(vec![s3_at("i/"), fs_at("i")]))
        .unwrap();
    assert_eq!(
        bench.store.mount_tree(&image.placement),
        Ok(MountTree::Image {
            tree: image.tree,
            paths: vec!["i".to_owned()]
        })
    );
    let records = bench.store.create(bench.request(vec![fs_at("r")])).unwrap();
    match bench.store.mount_tree(&records.placement).unwrap() {
        MountTree::Flat(flat) => {
            assert_eq!(flat.paths, vec!["r".to_owned()]);
            assert_eq!(flat.entries.len(), 10);
        }
        other @ MountTree::Image { .. } => panic!("{other:?}"),
    }
    let s3_only = bench
        .store
        .create_image(ImageRequest {
            at: vec![s3_at("only/")],
            ..bench.image_request(Vec::new())
        })
        .unwrap();
    assert_eq!(
        bench.store.mount_tree(&s3_only.placement),
        Err(FlatError::Read(ReadError::NoPlacement))
    );
}

/// Asserts that an image flattened for a mount of a release before the images has its entries in
/// the order of their indices (breadth first, the children of a directory in name order), each
/// naming its parent's index, with modes as stored, the pack listed once and every file's offset,
/// the empty file with no pack, and the fs paths only.
#[tokio::test]
async fn flat_of_an_image_is_in_index_order_with_packs_and_offsets() {
    let bench = Bench::new("image-flat").await;
    let made = bench
        .store
        .create_image(bench.image_request(vec![fs_at("i"), s3_at("i/")]))
        .unwrap();
    let flat = bench.store.flat(&made.placement).unwrap();
    assert_eq!(flat.paths, vec!["i".to_owned()]);
    assert_eq!(flat.packs, vec![bench.pack.key.clone()]);
    let dir = |parent: Option<u32>, name: &str, mode: u16| FlatEntry {
        parent,
        name: name.to_owned(),
        mode,
        kind: FlatKind::Dir,
    };
    let file = |parent: u32, name: &str, mode: u16, blob: usize| {
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
    };
    let expected = vec![
        dir(None, "", 0o755),
        dir(Some(0), "bin", 0o755),
        dir(Some(0), "empty", 0o755),
        dir(Some(0), "lib", 0o755),
        FlatEntry {
            parent: Some(0),
            name: "link".to_owned(),
            mode: 0o777,
            kind: FlatKind::Symlink {
                target: "bin/tool".to_owned(),
            },
        },
        FlatEntry {
            parent: Some(0),
            name: "zero".to_owned(),
            mode: 0o644,
            kind: FlatKind::File {
                size: 0,
                sha256: TreeDigest::of(b"").to_string(),
                pack: None,
                offset: 0,
            },
        },
        file(1, "other", 0o644, 1),
        file(1, "tool", 0o4755, 0),
        dir(Some(3), "deep", 0o755),
        file(8, "data", 0o644, 2),
    ];
    assert_eq!(flat.entries, expected);
}

/// Asserts that a restart keeps an image tree and its placement: the placement is found by
/// location, every path resolves as before, the image is the one written, the tree is not marked
/// ready when it was not, and the pack is pinned again for a ready one.
#[tokio::test]
async fn a_restart_keeps_an_image_tree_and_its_placement() {
    let bench = Bench::new("image-restart").await;
    bench.fill_all().await;
    let ready = bench
        .store
        .create_image(ImageRequest {
            ready: true,
            ..bench.image_request(vec![fs_at("jobs/1"), s3_at("jobs/1/")])
        })
        .unwrap();
    let before = bench.store.resolve(&ready.placement, "lib/deep/data");
    let cache = bench.rig.reopen();
    let store = Bench::reopen(cache.clone());
    assert_eq!(store.sizes(), (1, 1));
    assert_eq!(cache.pinned_bytes(), 6000);
    let found = store.locate_s3("build-cas", "jobs/1/bin/tool").unwrap();
    assert_eq!(found.placement, ready.placement);
    assert_eq!(found.tree, ready.tree);
    assert_eq!(store.resolve(&ready.placement, "lib/deep/data"), before);
    let info = store.placement(&ready.placement).unwrap();
    assert!(info.ready);
    assert_eq!(
        store.mount_tree(&ready.placement),
        Ok(MountTree::Image {
            tree: ready.tree,
            paths: vec!["jobs/1".to_owned()]
        })
    );
}

/// Asserts that each way an image's directory can be wrong is a reason to drop the tree, its
/// placement and its directory at a restart: an image that does not hash to the name of its
/// directory, one that hashes and is not a tree list, one cut short, and an
/// image path that cannot be read (a directory); the placement of each is gone.
#[tokio::test]
async fn an_image_directory_that_does_not_hold_up_is_dropped_and_removed() {
    type Damage = Box<dyn Fn(&std::path::Path, &Bytes)>;
    let damages: Vec<(&str, Damage)> = vec![
        (
            "mishashed",
            Box::new(|dir, bytes| {
                let mut other = bytes.to_vec();
                let last = other.len() - 1;
                other[last] ^= 1;
                std::fs::write(dir.join(IMAGE), other).unwrap();
            }),
        ),
        (
            "malformed",
            Box::new(|dir, _| std::fs::write(dir.join(IMAGE), b"not a list").unwrap()),
        ),
        (
            "cut",
            Box::new(|dir, bytes| {
                // Cut short, and put where the cut bytes' own digest names it.
                let cut = bytes.slice(..bytes.len() - 7);
                let to = dir.with_file_name(TreeDigest::of(&cut).to_string());
                std::fs::rename(dir, &to).unwrap();
                std::fs::write(to.join(IMAGE), &cut).unwrap();
            }),
        ),
        (
            "unreadable",
            Box::new(|dir, _| {
                std::fs::remove_file(dir.join(IMAGE)).unwrap();
                std::fs::create_dir(dir.join(IMAGE)).unwrap();
            }),
        ),
    ];
    for (name, damage) in damages {
        let bench = Bench::new(&format!("image-damaged-{name}")).await;
        let made = bench
            .store
            .create_image(bench.image_request(vec![fs_at("img")]))
            .unwrap();
        let other = bench
            .store
            .create(CreateRequest {
                built: built(vec![NewEntry {
                    path: "d".to_owned(),
                    kind: NewKind::Dir { mode: None },
                }]),
                at: vec![fs_at("other")],
                ttl: None,
                ready: false,
            })
            .unwrap();
        damage(&tree_path(&bench.rig, &made.tree), &bench.image_bytes());
        let store = Bench::reopen(bench.rig.reopen());
        assert_eq!(store.sizes(), (1, 1), "{name}");
        assert!(store.placement(&made.placement).is_none(), "{name}");
        assert!(store.placement(&other.placement).is_some(), "{name}");
        assert!(!tree_path(&bench.rig, &made.tree).exists(), "{name}");
        assert!(
            !placement_file(&bench.rig.config.path, &made.placement).exists(),
            "{name}"
        );
    }
}

/// Asserts that a ready image tree whose pack the cache lost is dropped at a restart with its
/// placement and directory, that the same restart keeps one that is not ready, and that a ready
/// image whose files are all in the cache is kept.
#[tokio::test]
async fn a_ready_image_tree_whose_spans_the_cache_lost_is_dropped() {
    let bench = Bench::new("image-lost").await;
    bench.fill_all().await;
    let ready = bench
        .store
        .create_image(ImageRequest {
            ready: true,
            ..bench.image_request(vec![fs_at("ready")])
        })
        .unwrap();
    let lazy_list = crate::TreeList {
        entries: vec![crate::ListEntry {
            path: "d".to_owned(),
            kind: crate::ListKind::Dir { mode: Mode::DIR },
        }],
    };
    let lazy_bytes = Bytes::from(lazy_list.encode_v2().unwrap());
    let lazy = bench
        .store
        .create_image(explicit(
            TreeDigest::of(&lazy_bytes),
            lazy_bytes,
            vec![fs_at("lazy")],
            false,
            None,
        ))
        .unwrap();
    assert_eq!(Bench::reopen(bench.rig.reopen()).sizes(), (2, 2));

    let hex = super::super::layout::name_of(&bench.pack.key).hex;
    let root = &bench.rig.config.path;
    for path in [
        bench.rig.file(&bench.pack.key),
        super::super::layout::meta_path(root, &hex),
    ] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir_all(root.join("extents-v1"));
    let store = Bench::reopen(bench.rig.reopen());
    assert_eq!(store.sizes(), (1, 1));
    assert!(store.placement(&ready.placement).is_none());
    assert!(!tree_path(&bench.rig, &ready.tree).exists());
    assert!(!placement_file(root, &ready.placement).exists());
    assert!(store.placement(&lazy.placement).is_some());
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
