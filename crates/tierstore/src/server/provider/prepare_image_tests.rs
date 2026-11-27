//! A tree's version 2 list, the projection of its L2 sidecar, prepared as the tree it is: the image
//! written beside the walk, each span read once.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use super::Provider;
use super::names::SourceId;
use super::prepare::{Job, run};
use super::sources::Stage;
use super::testing::{AGENT, Rig, list_of, member, settings};
use crate::server::local_cache::trees::testing::{Bench, tree_path};
use crate::server::local_cache::trees::{Resolved, TreesConfig};
use crate::{BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_digest};

/// The preparation of the source `run`/0 with `key`, held by the provider as its own.
fn held(rig: &Rig, run: &str, key: &str) -> (Job, watch::Receiver<Stage>) {
    let id = SourceId {
        agent: AGENT.to_owned(),
        run: run.to_owned(),
        index: 0,
        attempt: 0,
    };
    rig.provider.hold(id, key)
}

/// A preparation of `key` that the provider holds as its own, run to its end: the stage it ends at.
async fn prepared(rig: &Rig, run: &str, key: &str) -> Stage {
    let (job, seen) = held(rig, run, key);
    run_job(rig, job).await;
    seen.borrow().clone()
}

async fn run_job(rig: &Rig, job: Job) {
    run(Arc::clone(&rig.provider.shared), job).await;
}

/// The `trees` directory of the rig's cache.
fn trees_dir(rig: &Rig) -> PathBuf {
    rig.bench.rig.config.path.join("trees")
}

/// The names in the `trees` directory that are staging directories.
fn staging(rig: &Rig) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(trees_dir(rig)) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".new-"))
        .collect()
}

/// The names in the `trees` directory that are not staging directories: the trees there.
fn trees(rig: &Rig) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(trees_dir(rig)) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

/// The GETs the upstream has had of the pack.
fn pack_reads(rig: &Rig) -> u64 {
    rig.bench.rig.calls_on(Op::Get, &rig.bench.pack.key)
}

/// A file of `size` bytes with the digest `sha256`, whose bytes are in `key` at `offset`.
fn file_at(path: &str, key: &str, offset: u64, size: u64, sha256: &str) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::new(0o644).unwrap(),
            size,
            source: Some(BlobSource {
                key: key.into(),
                offset,
            }),
            sha256: Some(TreeDigest::from_hex(sha256).unwrap()),
        },
    }
}

/// Asserts that a tree run to its end from its sidecar leaves the bytes of its version 2 list, the
/// sidecar's projection byte for byte, as the file `image` of the tree directory named by the
/// list's digest, the tree directory the only one, and no staging directory.
#[tokio::test]
async fn a_placed_v2_list_leaves_its_bytes_as_the_image_and_no_staging_directory() {
    let rig = Rig::new("prov-image-placed").await;
    let bytes = list_of(&rig.bench.pack).encode_v2().unwrap();
    // The rig's own tree: its list is these bytes.
    let key = rig.key.clone();
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    let digest = tree_list_digest(&key).unwrap();
    let image = std::fs::read(tree_path(&rig.bench.rig, &digest).join("image")).unwrap();
    assert_eq!(image, bytes);
    assert_eq!(trees(&rig), [digest.to_string()]);
    assert_eq!(staging(&rig), Vec::<String>::new());
}

/// Asserts that a list the walk refuses (more entries than `trees.max_entries`) while its image is
/// being written beside the walk is refused and leaves no tree directory and no staging directory.
#[tokio::test]
async fn a_refused_v2_list_leaves_nothing_staged() {
    let config = TreesConfig {
        max_entries: 2,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-image-refused", config, |_| {}).await;
    let key = rig.key.clone();
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused("the tree is refused: tree too large: more than 2 entries".to_owned())
    );
    assert_eq!(staging(&rig), Vec::<String>::new());
    assert_eq!(trees(&rig), Vec::<String>::new());
}

/// Asserts that a second source with the list of a tree the node already has is placed on that
/// tree: one tree directory, two placements of it, the image the list's bytes, and the staging
/// directory the second run wrote ahead gone.
#[tokio::test]
async fn a_second_source_of_the_same_v2_list_shares_the_tree() {
    let rig = Rig::new("prov-image-twice").await;
    let bytes = list_of(&rig.bench.pack).encode_v2().unwrap();
    // The rig's own tree: its list is these bytes.
    let key = rig.key.clone();
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    assert_eq!(prepared(&rig, "js.2", &key).await, Stage::Placed);
    let store = &rig.bench.store;
    let first = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    let second = store.locate_fs(&Rig::place_of("js.2", 0)).unwrap();
    assert_eq!(first.tree, second.tree);
    assert_ne!(first.placement, second.placement);
    let digest = tree_list_digest(&key).unwrap();
    assert_eq!(trees(&rig), [digest.to_string()]);
    assert_eq!(staging(&rig), Vec::<String>::new());
    let image = std::fs::read(tree_path(&rig.bench.rig, &digest).join("image")).unwrap();
    assert_eq!(image, bytes);
}

/// Asserts that a list whose two files have one span (the same pack, offset, size and digest) is
/// read once, one GET of the pack, and both files are in the tree as files of that digest and size.
#[tokio::test]
async fn two_files_sharing_one_span_are_read_once() {
    let rig = Rig::new("prov-image-shared-span").await;
    let list = TreeList {
        entries: vec![
            member(&rig.bench.pack, "a", 1, 0o644),
            member(&rig.bench.pack, "b", 1, 0o644),
        ],
    };
    let key = rig.put_list_v2(&list).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    assert_eq!(pack_reads(&rig), 1);
    assert!(rig.holds(1));
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert!(store.placement(&located.placement).unwrap().ready);
    let (sha, _, size) = &rig.bench.pack.blobs[1];
    for path in ["a", "b"] {
        let Resolved::File {
            size: found,
            sha256,
            ..
        } = store.resolve(&located.placement, path).unwrap()
        else {
            panic!("{path} is not a file");
        };
        assert_eq!(found, *size, "{path}");
        assert_eq!(sha256, TreeDigest::from_hex(sha).unwrap(), "{path}");
    }
}

/// Asserts that two files of one digest at two spans (the same bytes twice in a pack) are both
/// read: the two spans are in the cache, and the tree is ready.
#[tokio::test]
async fn two_files_of_one_digest_at_two_spans_are_both_read() {
    let rig = Rig::new("prov-image-two-spans").await;
    let pack = &rig.bench.pack;
    let (sha, _, size) = pack.blobs[0].clone();
    let twice = [pack.content(0), pack.content(0)].concat();
    let twice_key = "cas/pack/twice";
    rig.bench.rig.put(twice_key, &twice).await;
    let list = TreeList {
        entries: vec![
            file_at("a", twice_key, 0, size, &sha),
            file_at("b", twice_key, size, size, &sha),
        ],
    };
    let key = rig.put_list_v2(&list).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    let cache = &rig.bench.rig.cache;
    assert!(cache.span_of(twice_key, 0, size).is_some());
    assert!(cache.span_of(twice_key, size, size).is_some());
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert!(store.placement(&located.placement).unwrap().ready);
}

/// Asserts that a run over a store with trees disabled fails as a tree not made, with nothing
/// staged and no tree directory.
#[tokio::test]
async fn a_v2_list_over_a_store_with_trees_disabled_fails_and_stages_nothing() {
    let config = TreesConfig {
        enabled: false,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-image-disabled", config, |_| {}).await;
    let key = rig.key.clone();
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Failed("the tree was not made: trees are off on this node".to_owned())
    );
    assert_eq!(staging(&rig), Vec::<String>::new());
    assert_eq!(trees(&rig), Vec::<String>::new());
}

/// Asserts that a run over a store of a cache that holds no lock (read-only) fails as a tree not
/// made, with nothing staged.
#[tokio::test]
async fn a_v2_list_over_a_read_only_store_fails_and_stages_nothing() {
    let mut rig = Rig::new("prov-image-read-only").await;
    let holder = rig.bench.rig.locked();
    let waiting = rig.bench.rig.locked();
    let store = Bench::reopen(waiting);
    rig.provider = Provider::with(store, rig.bench.rig.cache.clone(), settings());
    let key = rig.key.clone();
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Failed(
            "the tree was not made: this node does not hold the lock of its cache directory"
                .to_owned()
        )
    );
    assert_eq!(staging(&rig), Vec::<String>::new());
    assert_eq!(trees(&rig), Vec::<String>::new());
    drop(holder);
}
