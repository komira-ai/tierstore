//! The requests of the core about the placements, answered in process from the node's tree store:
//! where a path is among the placements and the hold a lookup takes, the tree of a placement, the
//! span of a member of a pack, and the holds a start ends.

use std::path::PathBuf;
use std::sync::Arc;

use super::super::unread;
use super::{MOUNT, after, ask, shared};
use crate::server::local_cache::trees::testing::{Bench, flat_of, fs_at, s3_at};
use crate::server::local_cache::trees::{
    FlatError, ImageRequest, Location, PlacementId, ReadError, placement_id,
};
use crate::server::treefs::fills::{AskError, Place, PlacedTree};
use crate::server::treefs::in_process::InProcess;
use crate::{
    BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeImage, TreeLimits, TreeList,
};

/// A bench with one placement, at the fs location `jobs/1/root` and the s3 location
/// `jobs/1/root/`, and the requests of a core to its cache and tree store.
async fn placed(name: &str) -> (Bench, PlacementId, Arc<InProcess>) {
    let bench = Bench::new(name).await;
    let at = vec![fs_at("jobs/1/root"), s3_at("jobs/1/root/")];
    let id = bench.store.create(bench.request(at)).unwrap().placement;
    let over = shared(&bench.rig.cache, Some(&bench.store));
    (bench, id, over)
}

/// A placement id no placement has.
fn unknown() -> PlacementId {
    placement_id(&"ab".repeat(16)).unwrap()
}

/// Where `path` is among the placements, for the mount.
async fn place(over: &Arc<InProcess>, path: &str) -> Result<Place, String> {
    let path = path.to_owned();
    ask(over, move |over| over.place(MOUNT, &path)).await
}

/// The fs locations of the placements.
async fn mounted(over: &Arc<InProcess>) -> Result<Vec<String>, String> {
    ask(over, |over| over.mounted()).await
}

/// The tree of the placement `id`.
async fn image(over: &Arc<InProcess>, id: &PlacementId) -> Result<PlacedTree, String> {
    let id = id.to_string();
    ask(over, move |over| over.image(&id)).await
}

/// Ends the hold of the mount on the placement `id`.
async fn unplace(over: &Arc<InProcess>, id: &PlacementId) {
    let id = id.to_string();
    ask(over, move |over| over.unplace(MOUNT, &id)).await;
}

/// The holds of the mount that end when it keeps the placements `keep`.
async fn places_live(over: &Arc<InProcess>, keep: Vec<&PlacementId>) -> Result<String, String> {
    let keep: Vec<String> = keep.iter().map(|id| id.to_string()).collect();
    let answered = ask(over, move |over| over.places_live(MOUNT, &keep)).await;
    answered.map_err(|why| why.to_string())
}

/// Asserts that a path that is the fs location of a placement is the tree, with its id, and holds
/// the placement for the mount (once, however often it is asked), that a path above a location is
/// a directory and holds nothing, and that any other path, one inside the location included, is
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_place_answers_what_is_at_a_path_and_holds_the_placement_once_for_the_mount() {
    let (bench, id, over) = placed("tfs-in-place").await;
    let held = || bench.store.placement(&id).unwrap().attachments;
    assert_eq!(held(), 0);
    let tree = Ok(Place::Tree(id.to_string()));
    assert_eq!(place(&over, "jobs/1/root").await, tree);
    assert_eq!(held(), 1);
    assert_eq!(place(&over, "jobs/1/root").await, tree);
    assert_eq!(held(), 1);
    assert_eq!(place(&over, "jobs").await, Ok(Place::Dir));
    assert_eq!(place(&over, "jobs/1").await, Ok(Place::Dir));
    for path in ["job", "jobs/2", "jobs/1/root/bin", "elsewhere"] {
        assert_eq!(place(&over, path).await, Ok(Place::Nothing), "{path}");
    }
    assert_eq!(held(), 1);
}

/// Asserts that an unplace ends the hold of the mount, so that a placement released meanwhile
/// goes with it, and that an unplace of a placement the mount never held, or one that is not
/// there, ends nothing and fails nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_unplace_ends_the_hold_of_the_mount() {
    let (bench, id, over) = placed("tfs-in-unplace").await;
    let _ = place(&over, "jobs/1/root").await;
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 1);
    bench.store.release(&id).unwrap();
    unplace(&over, &id).await;
    assert!(bench.store.placement(&id).is_none());
    unplace(&over, &id).await;
    unplace(&over, &unknown()).await;
}

/// Asserts that a place of a path whose placement is released between being listed and being
/// asked for is nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_place_of_a_released_placement_is_nothing() {
    let (bench, id, over) = placed("tfs-in-place-released").await;
    bench.store.release(&id).unwrap();
    assert_eq!(place(&over, "jobs/1/root").await, Ok(Place::Nothing));
    assert_eq!(place(&over, "jobs").await, Ok(Place::Nothing));
}

/// Asserts that the mounted paths are the fs locations of the placements, sorted, and none while
/// there are none.
#[tokio::test(flavor = "multi_thread")]
async fn mounted_gives_the_fs_locations_sorted_and_none_while_there_are_none() {
    let bench = Bench::new("tfs-in-mounted").await;
    let over = shared(&bench.rig.cache, Some(&bench.store));
    assert_eq!(mounted(&over).await, Ok(Vec::new()));
    for at in [vec![fs_at("zed")], vec![fs_at("a b/c"), s3_at("x/")]] {
        let _ = bench.store.create(bench.request(at)).unwrap();
    }
    assert_eq!(
        mounted(&over).await,
        Ok(vec!["a b/c".to_owned(), "zed".to_owned()])
    );
}

/// Asserts that the tree of a placement made of records is the tree the store flattens, that a
/// placement that is not there or has no fs location is `none`, and that a placement whose records
/// are damaged is `damaged`.
#[tokio::test(flavor = "multi_thread")]
async fn the_image_of_a_records_placement_is_its_flat_tree_none_or_damaged() {
    let (bench, id, over) = placed("tfs-in-flat").await;
    let flat = flat_of(&bench.store, &id).unwrap();
    assert_eq!(flat.entries.len(), 10);
    assert_eq!(image(&over, &id).await, Ok(PlacedTree::Flat(flat)));
    assert_eq!(image(&over, &unknown()).await, Err("none".to_owned()));
    let s3_only = bench
        .store
        .create(bench.request(vec![s3_at("only/")]))
        .unwrap()
        .placement;
    assert_eq!(image(&over, &s3_only).await, Err("none".to_owned()));
    let records =
        crate::server::local_cache::trees::testing::tree_path(&bench.rig, &bench.built().root)
            .join("records");
    std::fs::write(&records, b"damaged").unwrap();
    assert_eq!(image(&over, &id).await, Err("damaged".to_owned()));
}

/// Asserts the words of a tree that cannot be read: `too large` for a tree too large for the mount,
/// `none` for a placement that is gone, `damaged` for any other.
#[test]
fn a_tree_that_cannot_be_read_is_none_damaged_or_too_large() {
    assert_eq!(unread(&FlatError::TooLarge), "too large");
    assert_eq!(unread(&FlatError::Read(ReadError::NoPlacement)), "none");
    assert_eq!(
        unread(&FlatError::Read(ReadError::Corrupt("x".to_owned()))),
        "damaged"
    );
}

/// A version 2 list of a file of the bench's first blob, an empty directory and a link, made the
/// tree `bench.store` holds at `at`, and the placement and the digest of the tree.
fn image_placed(bench: &Bench, at: Vec<Location>) -> (PlacementId, TreeDigest) {
    let (sha256, offset, size) = &bench.pack.blobs[0];
    let list = TreeList {
        entries: vec![
            ListEntry {
                path: "bin".to_owned(),
                kind: ListKind::File {
                    mode: Mode::new(0o755).unwrap(),
                    size: *size,
                    source: Some(BlobSource {
                        key: Arc::from(bench.pack.key.as_str()),
                        offset: *offset,
                    }),
                    sha256: Some(TreeDigest::from_hex(sha256).unwrap()),
                },
            },
            ListEntry {
                path: "empty".to_owned(),
                kind: ListKind::Dir {
                    mode: Mode::new(0o755).unwrap(),
                },
            },
            ListEntry {
                path: "link".to_owned(),
                kind: ListKind::Symlink {
                    target: "bin".to_owned(),
                },
            },
        ],
    };
    let bytes = bytes::Bytes::from(list.encode_v2().unwrap());
    let digest = TreeDigest::of(&bytes);
    let image = TreeImage::open(bytes, &TreeLimits::default(), |_| {}).unwrap();
    let created = bench
        .store
        .create_image(ImageRequest {
            image: Arc::new(image),
            digest,
            at,
            ttl: None,
            ready: false,
            staged: None,
        })
        .unwrap();
    (created.placement, digest)
}

/// Asserts that the tree of a placement whose tree is a version 2 list is the digest of the list
/// and the fs paths of the placement, in the order of its locations, with the paths of the fs
/// locations alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_image_of_a_version_2_placement_is_its_digest_and_its_fs_paths() {
    let bench = Bench::new("tfs-in-image").await;
    let at = vec![fs_at("jobs/1/root"), s3_at("jobs/1/root/"), fs_at("a b")];
    let (id, digest) = image_placed(&bench, at);
    let over = shared(&bench.rig.cache, Some(&bench.store));
    assert_eq!(
        image(&over, &id).await,
        Ok(PlacedTree::Image {
            tree: digest,
            paths: vec!["jobs/1/root".to_owned(), "a b".to_owned()],
        })
    );
}

/// The file the span of the blob `blob` of the bench's pack is in, and where in it.
async fn span(over: &Arc<InProcess>, bench: &Bench, blob: usize) -> Result<(u64, PathBuf), String> {
    let (sha256, offset, size) = bench.pack.blobs[blob].clone();
    let pack = bench.pack.key.clone();
    ask(over, move |over| {
        over.span((sha256.as_str(), size), (pack.as_str(), offset), None)
    })
    .await
}

/// Asserts that a span of a member the cache does not hold is filled, and says the file and the
/// offset of its bytes, that reading the file at the offset gives exactly the member's bytes, and
/// that asking again makes no second request of the upstream.
#[tokio::test(flavor = "multi_thread")]
async fn a_span_fills_a_member_once_and_says_where_its_bytes_are() {
    let bench = Bench::new("tfs-in-span").await;
    let over = shared(&bench.rig.cache, Some(&bench.store));
    for blob in [1, 0] {
        let (offset, file) = span(&over, &bench, blob).await.unwrap();
        let offset = usize::try_from(offset).unwrap();
        let bytes = std::fs::read(file).unwrap();
        let want = bench.pack.content(blob);
        assert_eq!(&bytes[offset..offset + want.len()], want);
    }
    let gets = bench.rig.upstream.calls_on(Op::Get, &bench.pack.key);
    assert!(gets >= 1);
    for blob in [1, 0] {
        assert!(span(&over, &bench, blob).await.is_ok());
    }
    assert_eq!(bench.rig.upstream.calls_on(Op::Get, &bench.pack.key), gets);
}

/// Asserts that a span whose digest is not that of the bytes at the range is answered with the
/// words of the failed fill, and so is one of a pack the upstream does not have.
#[tokio::test(flavor = "multi_thread")]
async fn a_span_that_cannot_be_filled_says_why() {
    let bench = Bench::new("tfs-in-span-refused").await;
    let over = shared(&bench.rig.cache, Some(&bench.store));
    let (other, _, _) = bench.pack.blobs[1].clone();
    let (sha256, offset, size) = bench.pack.blobs[0].clone();
    let key = bench.pack.key.clone();
    let wrong = ask(&over, move |over| {
        over.span((other.as_str(), size), (key.as_str(), offset), None)
    })
    .await;
    assert!(wrong.unwrap_err().starts_with("fill: mismatch"));
    let lacking = ask(&over, move |over| {
        over.span((sha256.as_str(), size), ("cas/pack/lacking", offset), None)
    })
    .await;
    assert_eq!(lacking.unwrap_err(), "fill: upstream 404");
}

/// Asserts that the ending of the holds of the mount keeps those of the placements it lists and of
/// other mounts, and says how many ended.
#[tokio::test(flavor = "multi_thread")]
async fn places_live_ends_the_holds_of_the_mount_it_does_not_list() {
    let (bench, kept, over) = placed("tfs-in-places-live").await;
    let other = bench
        .store
        .create(bench.request(vec![fs_at("jobs/2/root")]))
        .unwrap()
        .placement;
    assert_eq!(
        place(&over, "jobs/1/root").await,
        Ok(Place::Tree(kept.to_string()))
    );
    assert_eq!(
        place(&over, "jobs/2/root").await,
        Ok(Place::Tree(other.to_string()))
    );
    bench.store.attach(&other, "mount-2").unwrap();
    let held = |id: &PlacementId| bench.store.placement(id).unwrap().attachments;
    assert_eq!(places_live(&over, vec![&kept]).await, Ok("ok 1".to_owned()));
    assert_eq!(held(&kept), 1);
    assert_eq!(held(&other), 1);
    assert_eq!(places_live(&over, vec![]).await, Ok("ok 1".to_owned()));
    assert_eq!(held(&kept), 0);
    assert_eq!(places_live(&over, vec![]).await, Ok("ok 0".to_owned()));
}

/// Asserts that a span whose end has passed is not asked: it fails with the words of a request out
/// of time, and the upstream is not read.
#[tokio::test(flavor = "multi_thread")]
async fn a_span_whose_end_has_passed_is_not_asked() {
    let bench = Bench::new("tfs-in-span-spent").await;
    let over = shared(&bench.rig.cache, Some(&bench.store));
    let (sha256, offset, size) = bench.pack.blobs[0].clone();
    let pack = bench.pack.key.clone();
    let spent = ask(&over, move |over| {
        over.span(
            (sha256.as_str(), size),
            (pack.as_str(), offset),
            Some(after(0)),
        )
    })
    .await;
    assert_eq!(spent.unwrap_err(), AskError::Spent.to_string());
    assert_eq!(bench.rig.upstream.calls_on(Op::Get, &bench.pack.key), 0);
}
