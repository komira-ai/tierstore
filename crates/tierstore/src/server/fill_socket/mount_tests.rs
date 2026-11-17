//! The answers of the fill socket to the mount's requests about placements.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use super::super::request::MountRequest;
use super::answer;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::testing::{Bench, fs_at, s3_at};
use crate::server::local_cache::trees::{
    FlatTree, ImageRequest, PlacementId, TreeStore, placement_id,
};
use crate::{
    BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeImage, TreeLimits, TreeList,
};

const MOUNT: &str = "mount-1";

async fn placed(name: &str) -> (Bench, PlacementId) {
    let bench = Bench::new(name).await;
    let at = vec![fs_at("jobs/1/root"), s3_at("jobs/1/root/")];
    let id = bench.store.create(bench.request(at)).unwrap().placement;
    (bench, id)
}

fn place(path: &str) -> MountRequest {
    MountRequest::Place {
        mount: MOUNT.to_owned(),
        path: path.to_owned(),
    }
}

fn span(bench: &Bench, blob: usize) -> MountRequest {
    let (sha256, offset, size) = &bench.pack.blobs[blob];
    MountRequest::Span {
        sha256: TreeDigest::from_hex(sha256).unwrap(),
        size: *size,
        pack: bench.pack.key.clone(),
        offset: *offset,
    }
}

fn unplace(id: &PlacementId) -> MountRequest {
    MountRequest::Unplace {
        mount: MOUNT.to_owned(),
        placement: id.clone(),
    }
}

fn unknown() -> PlacementId {
    placement_id(&"ab".repeat(16)).unwrap()
}

/// Asserts that a path that is the fs location of a placement is answered `tree` with the id and
/// holds the placement for the mount (once, however often it is asked), that a path above a
/// location is a `dir` and holds nothing, and that any other path, one inside the location
/// included, is `none`.
#[tokio::test]
async fn a_place_answers_the_tree_at_a_location_and_holds_it_for_the_mount() {
    let (bench, id) = placed("mount-place").await;
    let store = Some(&bench.store);
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 0);
    assert_eq!(
        answer(store, place("jobs/1/root")).await,
        format!("tree {id}")
    );
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 1);
    assert_eq!(
        answer(store, place("jobs/1/root")).await,
        format!("tree {id}")
    );
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 1);
    assert_eq!(answer(store, place("jobs")).await, "dir");
    assert_eq!(answer(store, place("jobs/1")).await, "dir");
    for path in ["job", "jobs/2", "jobs/1/root/bin", "elsewhere"] {
        assert_eq!(answer(store, place(path)).await, "none", "{path}");
    }
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 1);
}

/// Asserts that an unplace is answered `ok` and ends the hold of the mount, so that a placement
/// released meanwhile goes with it, and that an unplace of a placement the mount never held, or
/// one that is not there, is answered `ok` too.
#[tokio::test]
async fn an_unplace_answers_ok_and_ends_the_hold_of_the_mount() {
    let (bench, id) = placed("mount-unplace").await;
    let store = Some(&bench.store);
    let _ = answer(store, place("jobs/1/root")).await;
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 1);
    bench.store.release(&id).unwrap();
    assert_eq!(answer(store, unplace(&id)).await, "ok");
    assert!(bench.store.placement(&id).is_none());
    assert_eq!(answer(store, unplace(&id)).await, "ok");
    assert_eq!(answer(store, unplace(&unknown())).await, "ok");
}

/// Asserts that a place of a path whose placement is released between being listed and being asked
/// for is `none`.
#[tokio::test]
async fn a_place_of_a_released_placement_is_none() {
    let (bench, id) = placed("mount-place-released").await;
    bench.store.release(&id).unwrap();
    assert_eq!(
        answer(Some(&bench.store), place("jobs/1/root")).await,
        "none"
    );
    assert_eq!(answer(Some(&bench.store), place("jobs")).await, "none");
}

/// Asserts that mounted answers the paths of the fs locations as one line of JSON, sorted, and an
/// empty list while there are none.
#[tokio::test]
async fn mounted_answers_the_fs_locations_as_a_json_list() {
    let bench = Bench::new("mount-mounted").await;
    let store = Some(&bench.store);
    assert_eq!(answer(store, MountRequest::Mounted).await, "[]");
    for at in [vec![fs_at("zed")], vec![fs_at("a b/c"), s3_at("x/")]] {
        let _ = bench.store.create(bench.request(at)).unwrap();
    }
    let text = answer(store, MountRequest::Mounted).await;
    assert!(!text.contains('\n'));
    let paths: Vec<String> = serde_json::from_str(&text).unwrap();
    assert_eq!(paths, vec!["a b/c".to_owned(), "zed".to_owned()]);
}

/// Asserts that flat answers one line of JSON that reads back as the tree the store flattens, that
/// a placement that is not there is `none`, and that a placement whose records are damaged is
/// `damaged`.
#[tokio::test]
async fn a_flat_answers_the_tree_none_or_damaged() {
    let (bench, id) = placed("mount-flat").await;
    let store = Some(&bench.store);
    let flat = MountRequest::Flat {
        placement: id.clone(),
    };
    let text = answer(store, flat).await;
    assert!(!text.contains('\n'));
    let tree: FlatTree = serde_json::from_str(&text).unwrap();
    assert_eq!(tree, bench.store.flat(&id).unwrap());
    assert_eq!(tree.entries.len(), 10);
    let missing = MountRequest::Flat {
        placement: unknown(),
    };
    assert_eq!(answer(store, missing).await, "none");
    let records =
        crate::server::local_cache::trees::testing::tree_path(&bench.rig, &bench.built().root)
            .join("records");
    std::fs::write(&records, b"damaged").unwrap();
    let flat = MountRequest::Flat { placement: id };
    assert_eq!(answer(store, flat).await, "damaged");
}

/// Asserts that a span of a member the cache does not hold is filled and answered `ok <offset>
/// <file>`, that reading the file at the offset gives exactly the member's bytes, and that asking
/// again makes no second request of the upstream.
#[tokio::test]
async fn a_span_fills_a_member_once_and_says_where_its_bytes_are() {
    let bench = Bench::new("mount-span").await;
    let store = Some(&bench.store);
    for blob in [1, 0] {
        let text = answer(store, span(&bench, blob)).await;
        let rest = text.strip_prefix("ok ").unwrap();
        let (offset, file) = rest.split_once(' ').unwrap();
        let offset: usize = offset.parse().unwrap();
        let bytes = std::fs::read(PathBuf::from(file)).unwrap();
        let want = bench.pack.content(blob);
        assert_eq!(&bytes[offset..offset + want.len()], want);
    }
    let gets = bench.rig.upstream.calls_on(Op::Get, &bench.pack.key);
    assert!(gets >= 1);
    let again = answer(store, span(&bench, 1)).await;
    assert!(again.starts_with("ok "), "{again}");
    let again = answer(store, span(&bench, 0)).await;
    assert!(again.starts_with("ok "), "{again}");
    assert_eq!(bench.rig.upstream.calls_on(Op::Get, &bench.pack.key), gets);
}

/// Asserts that a span whose digest is not that of the bytes at the range is answered with the
/// words of the failed fill, and so is one of a pack the upstream does not have.
#[tokio::test]
async fn a_span_that_cannot_be_filled_says_why() {
    let bench = Bench::new("mount-span-refused").await;
    let store = Some(&bench.store);
    let (other, _, _) = &bench.pack.blobs[1];
    let (_, offset, size) = &bench.pack.blobs[0];
    let wrong = MountRequest::Span {
        sha256: TreeDigest::from_hex(other).unwrap(),
        size: *size,
        pack: bench.pack.key.clone(),
        offset: *offset,
    };
    assert!(answer(store, wrong).await.starts_with("fill: mismatch"));
    let MountRequest::Span {
        sha256,
        size,
        offset,
        ..
    } = span(&bench, 0)
    else {
        panic!("a span");
    };
    let lacking = MountRequest::Span {
        sha256,
        size,
        pack: "cas/pack/lacking".to_owned(),
        offset,
    };
    assert_eq!(answer(store, lacking).await, "fill: upstream 404");
}

/// Asserts that a node with no tree store has no placement: place, flat and span are `none`,
/// mounted is an empty list, and unplace is `ok`.
#[tokio::test]
async fn a_node_with_no_tree_store_has_no_placements() {
    let bench = Bench::new("mount-no-store").await;
    let flat = MountRequest::Flat {
        placement: unknown(),
    };
    assert_eq!(answer(None, place("jobs/1/root")).await, "none");
    assert_eq!(answer(None, flat).await, "none");
    assert_eq!(answer(None, span(&bench, 0)).await, "none");
    assert_eq!(answer(None, MountRequest::Mounted).await, "[]");
    assert_eq!(answer(None, unplace(&unknown())).await, "ok");
    let live = MountRequest::PlacesLive {
        mount: MOUNT.to_owned(),
        keep: Vec::new(),
    };
    assert_eq!(answer(None, live).await, "ok 0");
}

/// Asserts that `places-live` ends the holds of the mount on the placements it does not list and
/// keeps those it lists and the holds of other mounts, answering how many ended.
#[tokio::test]
async fn places_live_ends_the_holds_of_the_mount_it_does_not_list() {
    let (bench, kept) = placed("mount-places-live").await;
    let other = bench
        .store
        .create(bench.request(vec![fs_at("jobs/2/root")]))
        .unwrap()
        .placement;
    assert_eq!(
        answer(Some(&bench.store), place("jobs/1/root")).await,
        format!("tree {kept}")
    );
    assert_eq!(
        answer(Some(&bench.store), place("jobs/2/root")).await,
        format!("tree {other}")
    );
    bench.store.attach(&other, "mount-2").unwrap();
    let live = |keep: Vec<PlacementId>| MountRequest::PlacesLive {
        mount: MOUNT.to_owned(),
        keep,
    };
    assert_eq!(
        answer(Some(&bench.store), live(vec![kept.clone()])).await,
        "ok 1"
    );
    assert_eq!(bench.store.placement(&kept).unwrap().attachments, 1);
    assert_eq!(bench.store.placement(&other).unwrap().attachments, 1);
    assert_eq!(answer(Some(&bench.store), live(Vec::new())).await, "ok 1");
    assert_eq!(bench.store.placement(&kept).unwrap().attachments, 0);
    assert_eq!(answer(Some(&bench.store), live(Vec::new())).await, "ok 0");
}

/// Asserts that a tree too large for the mount is answered `too large`, and the other answers of
/// a flat by what went wrong.
#[test]
fn a_flat_too_large_for_the_mount_is_answered_so() {
    use super::flat_answer;
    use crate::server::local_cache::trees::{FlatError, ReadError};
    assert_eq!(flat_answer(&Err(FlatError::TooLarge)), "too large");
    assert_eq!(
        flat_answer(&Err(FlatError::Read(ReadError::NoPlacement))),
        "none"
    );
    assert_eq!(
        flat_answer(&Err(FlatError::Read(ReadError::Corrupt("x".to_owned())))),
        "damaged"
    );
}

/// Sends `lines` over one connection to the socket at `path` and reads one answer line for each.
fn converse(stream: UnixStream, lines: &[String]) -> Vec<String> {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut answers = Vec::new();
    for line in lines {
        writer.write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = reader.read_line(&mut answer).unwrap();
        answers.push(answer.trim_end().to_owned());
    }
    answers
}

/// Asserts that a session of the real socket dispatches the mount's requests to the tree store:
/// place holds a placement, mounted lists it, flat gives its tree, a span is filled, and unplace
/// ends the hold, each answered on its own line in order.
#[tokio::test]
async fn the_socket_answers_the_mount_requests_from_the_tree_store() {
    let (bench, id) = placed("mount-socket").await;
    let dir = TestDir::under(std::env::temp_dir(), "mount-socket");
    let path = dir.path().join("fill.sock");
    let halt = Arc::new(Notify::new());
    let stores: (_, Option<TreeStore>) = (bench.rig.cache.clone(), Some(bench.store.clone()));
    let task = {
        let (path, halt) = (path.clone(), Arc::clone(&halt));
        tokio::spawn(async move { super::super::serve(stores, &path, halt).await })
    };
    let mut found = None;
    until("the socket accepts", || {
        found = UnixStream::connect(&path).ok();
        found.is_some()
    })
    .await;
    let (sha256, offset, size) = bench.pack.blobs[2].clone();
    let lines = vec![
        format!("place {MOUNT} jobs/1/root"),
        format!("place {MOUNT} jobs"),
        "mounted".to_owned(),
        format!("flat {id}"),
        format!("span {sha256} {size} from {} @ {offset}", bench.pack.key),
        format!("unplace {MOUNT} {id}"),
        "flat nonsense".to_owned(),
    ];
    let stream = found.unwrap();
    let answers = tokio::task::spawn_blocking(move || converse(stream, &lines))
        .await
        .unwrap();
    assert_eq!(answers[0], format!("tree {id}"));
    assert_eq!(answers[1], "dir");
    assert_eq!(answers[2], r#"["jobs/1/root"]"#);
    let tree: FlatTree = serde_json::from_str(&answers[3]).unwrap();
    assert_eq!(tree, bench.store.flat(&id).unwrap());
    assert!(answers[4].starts_with("ok "), "{}", answers[4]);
    assert_eq!(answers[5], "ok");
    assert_eq!(answers[6], "refused not a placement id");
    assert_eq!(bench.store.placement(&id).unwrap().attachments, 0);
    halt.notify_one();
    task.await.unwrap();
}

/// A version 2 list of a file of the bench's first blob, an empty directory and a link, made the
/// tree `bench.store` holds at `at`, and the placement and the digest of the tree.
fn image_placed(
    bench: &Bench,
    at: Vec<crate::server::local_cache::trees::Location>,
) -> (PlacementId, TreeDigest) {
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

/// Asserts that an image of a placement whose tree is a version 2 list is answered `image`, the
/// tree's digest and the fs paths of the placement, in the order of its locations, as one line of
/// JSON, with the paths of the fs locations alone.
#[tokio::test]
async fn an_image_answers_the_digest_and_the_paths_of_an_image_tree() {
    let bench = Bench::new("mount-image").await;
    let at = vec![fs_at("jobs/1/root"), s3_at("jobs/1/root/"), fs_at("a b")];
    let (id, digest) = image_placed(&bench, at);
    let request = MountRequest::Image { placement: id };
    assert_eq!(
        answer(Some(&bench.store), request).await,
        format!("image {digest} [\"jobs/1/root\",\"a b\"]")
    );
}

/// Asserts that an image of a placement whose tree is made of records is answered with the same
/// line of JSON as a flat of it.
#[tokio::test]
async fn an_image_of_a_records_tree_answers_as_a_flat_does() {
    let (bench, id) = placed("mount-image-records").await;
    let store = Some(&bench.store);
    let flat = answer(
        store,
        MountRequest::Flat {
            placement: id.clone(),
        },
    )
    .await;
    let image = answer(store, MountRequest::Image { placement: id }).await;
    assert_eq!(image, flat);
    assert!(image.starts_with('{'), "{image}");
}

/// Asserts that an image of a placement that is not there, or one with no fs location, is `none`,
/// that one whose records are damaged is `damaged`, and that with no tree store every image is
/// `none`.
#[tokio::test]
async fn an_image_answers_none_or_damaged_and_none_with_no_store() {
    let (bench, id) = placed("mount-image-none").await;
    let store = Some(&bench.store);
    let unknown = MountRequest::Image {
        placement: unknown(),
    };
    assert_eq!(answer(store, unknown).await, "none");
    let s3_only = bench
        .store
        .create(bench.request(vec![s3_at("only/")]))
        .unwrap()
        .placement;
    let request = MountRequest::Image { placement: s3_only };
    assert_eq!(answer(store, request).await, "none");
    let image = || MountRequest::Image {
        placement: id.clone(),
    };
    assert_eq!(answer(None, image()).await, "none");
    let records =
        crate::server::local_cache::trees::testing::tree_path(&bench.rig, &bench.built().root)
            .join("records");
    std::fs::write(&records, b"damaged").unwrap();
    assert_eq!(answer(store, image()).await, "damaged");
}
