//! One preparation of a version 2 tree list run to its end: the tree placed with the root a
//! version 1 list of the same entries gives, the spans read as the walk queues them, a list that
//! does not hash to its key, a record or a header the reader refuses, a tree the store refuses, a
//! release before or during the placement, and a file whose pack is not there.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::watch;

use super::names::SourceId;
use super::prepare::{Job, run};
use super::sources::Stage;
use super::testing::{AGENT, READ_ONLY, Rig, list_of, member};
use crate::server::local_cache::trees::TreesConfig;
use crate::{BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_digest};

/// The id of the source `run`/0.
fn id_of(run: &str) -> SourceId {
    SourceId {
        agent: AGENT.to_owned(),
        run: run.to_owned(),
        index: 0,
    }
}

/// A preparation of the source `run`/0 with `key` that the provider does not hold (its source was
/// released, or prepared again by another), and the stage it reports.
fn job(run: &str, key: &str, released: bool) -> (Job, watch::Receiver<Stage>) {
    let (stage, seen) = watch::channel(Stage::Preparing);
    let job = Job {
        id: id_of(run),
        key: key.to_owned(),
        digest: tree_list_digest(key).expect("a list key"),
        stage: Arc::new(stage),
        released: Arc::new(AtomicBool::new(released)),
    };
    (job, seen)
}

/// The preparation of the source `run`/0 with `key`, held by the provider as its own.
fn held(rig: &Rig, run: &str, key: &str) -> (Job, watch::Receiver<Stage>) {
    rig.provider.hold(id_of(run), key)
}

/// The GETs the upstream has had of the pack.
fn pack_reads(rig: &Rig) -> u64 {
    rig.bench.rig.calls_on(Op::Get, &rig.bench.pack.key)
}

/// The bytes of the version 2 list of the pack's tree.
fn v2_bytes(rig: &Rig) -> Vec<u8> {
    list_of(&rig.bench.pack).encode_v2().expect("a list")
}

/// A version 2 list with no file in the pack and one file in a pack that is not there.
fn list_of_missing_pack(rig: &Rig) -> TreeList {
    let pack = &rig.bench.pack;
    let (sha, _, size) = &pack.blobs[0];
    TreeList {
        entries: vec![
            member(pack, "kept", 1, 0o644),
            ListEntry {
                path: "gone".to_owned(),
                kind: ListKind::File {
                    mode: Mode::new(0o644).unwrap(),
                    size: *size,
                    source: Some(BlobSource {
                        key: "cas/pack/missing".into(),
                        offset: 0,
                    }),
                    sha256: Some(TreeDigest::from_hex(sha).unwrap()),
                },
            },
        ],
    }
}

/// The offset in a version 2 list's header of the number of the version.
const VERSION_AT: usize = 8;
/// The offset in a version 2 list's header of where the records begin.
const RECORDS_AT: usize = 36;

/// The record at `index` of `bytes`, from the header's start of the records, 64 bytes each.
fn record_at(bytes: &[u8], index: usize) -> usize {
    let start = u32::from_le_bytes(bytes[RECORDS_AT..RECORDS_AT + 4].try_into().unwrap());
    start as usize + index * 64
}

/// Asserts that a version 2 list run to its end leaves the source placed, every file's span read
/// once into the cache, and the tree marked ready, and that the tree has the root of the version 1
/// list of the same entries.
#[tokio::test]
async fn a_v2_list_run_to_its_end_places_the_tree_of_the_v1_list_and_reads_every_file() {
    let rig = Rig::new("prov-v2-ready").await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    assert_ne!(key, rig.key);
    let (job2, seen2) = held(&rig, "js.2", &key);
    run(Arc::clone(&rig.provider.shared), job2).await;
    assert_eq!(*seen2.borrow(), Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.2", 0)).unwrap();
    assert!(store.placement(&located.placement).unwrap().ready);
    assert!((0..3).all(|blob| rig.holds(blob)));
    assert_eq!(pack_reads(&rig), 3);

    let (job1, seen1) = held(&rig, "js.1", &rig.key);
    run(Arc::clone(&rig.provider.shared), job1).await;
    assert_eq!(*seen1.borrow(), Stage::Placed);
    let v1 = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert_eq!(located.tree, v1.tree);
}

/// Asserts that a version 2 list whose bytes do not hash to the digest the job names is refused
/// with the message that names both digests, before any span is read and before any tree is
/// placed. The cache checks a `cas/l/` object against its key as it reads it, so the job here is
/// one whose digest is not its key's: the check of the preparation itself.
#[tokio::test]
async fn a_v2_list_that_does_not_hash_to_its_digest_is_refused_and_reads_nothing() {
    let rig = Rig::new("prov-v2-digest").await;
    let bytes = v2_bytes(&rig);
    let key = rig.put_bytes(&bytes).await;
    let (mut job, seen) = held(&rig, "js.1", &key);
    let expected = TreeDigest::of(b"other bytes");
    job.digest = expected;
    run(Arc::clone(&rig.provider.shared), job).await;
    let actual = TreeDigest::of(&bytes);
    assert_eq!(
        *seen.borrow(),
        Stage::Refused(format!(
            "tree list digest mismatch: expected {expected}, the bytes hash to {actual}"
        ))
    );
    assert_eq!(pack_reads(&rig), 0);
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a version 2 list with a record of an unknown kind is refused with the reader's
/// message naming the record, and that no tree is placed.
#[tokio::test]
async fn a_v2_list_with_a_bad_record_is_refused_and_not_placed() {
    let rig = Rig::new("prov-v2-record").await;
    let mut bytes = v2_bytes(&rig);
    let at = record_at(&bytes, 0);
    bytes[at] = 9;
    let key = rig.put_bytes(&bytes).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(
        *seen.borrow(),
        Stage::Refused("tree list record 0: unknown kind 9".to_owned())
    );
    assert_eq!(pack_reads(&rig), 0);
    assert!(rig.bench.store.mount_paths().is_empty());
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_none()
    );
}

/// Asserts that a version 2 list refused after some of its spans were queued (a bad record among
/// the files) is refused with the reader's message and has no placement: the reads it started are
/// the only cost.
#[tokio::test]
async fn a_v2_list_with_a_bad_file_record_after_its_first_spans_is_not_placed() {
    let rig = Rig::new("prov-v2-late-record").await;
    let mut bytes = v2_bytes(&rig);
    // The last record is the file of size 0: a kind below the one before it is out of order.
    let count = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
    let at = record_at(&bytes, count - 1);
    bytes[at] = 1;
    let key = rig.put_bytes(&bytes).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(
        *seen.borrow(),
        Stage::Refused(format!(
            "tree list record {}: out of order: the directories come first, then the links, then the files",
            count - 1
        ))
    );
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a version 2 list of a version this node does not read is refused with the
/// header's message, by the open before any walk.
#[tokio::test]
async fn a_v2_list_of_another_version_is_refused() {
    let rig = Rig::new("prov-v2-version").await;
    let mut bytes = v2_bytes(&rig);
    bytes[VERSION_AT..VERSION_AT + 4].copy_from_slice(&3u32.to_le_bytes());
    let key = rig.put_bytes(&bytes).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(
        *seen.borrow(),
        Stage::Refused("unsupported tree list version 3".to_owned())
    );
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a version 2 list with more entries than `trees.max_entries` is refused by the
/// walk's first check, with the tree's refusal in the message, and is not placed.
#[tokio::test]
async fn a_v2_list_over_the_entry_limit_is_refused() {
    let config = TreesConfig {
        max_entries: 2,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-v2-limit", config, |_| {}).await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    let Stage::Refused(why) = seen.borrow().clone() else {
        panic!("expected a refusal, got {:?}", *seen.borrow());
    };
    assert_eq!(
        why,
        "the tree is refused: tree too large: more than 2 entries"
    );
    assert_eq!(pack_reads(&rig), 0);
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a version 2 list whose tree the tree store refuses (its files are more bytes than
/// `trees.max_bytes`) is refused, with the store's reason, and is not placed.
#[tokio::test]
async fn a_v2_list_whose_tree_the_store_refuses_is_refused() {
    let config = TreesConfig {
        max_bytes: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-v2-store-refuses", config, |_| {}).await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    let Stage::Refused(why) = seen.borrow().clone() else {
        panic!("expected a refusal, got {:?}", *seen.borrow());
    };
    assert!(
        why.starts_with("the tree is refused: tree too large: "),
        "{why}"
    );
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a version 2 list whose tree this node cannot make now (no placement is free) is
/// failed, not refused, so that the next mount tries again.
#[tokio::test]
async fn a_v2_list_the_store_cannot_place_now_is_failed() {
    let config = TreesConfig {
        max_placements: 0,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-v2-failed", config, |_| {}).await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(
        *seen.borrow(),
        Stage::Failed("the tree was not made: too many placements: 0 are alive".to_owned())
    );
}

/// Asserts that a version 2 list of a source released before its placement ends released with no
/// placement and no span read.
#[tokio::test]
async fn a_v2_list_of_a_released_source_ends_released_with_no_placement() {
    let rig = Rig::new("prov-v2-released").await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    let (job, seen) = job("js.1", &key, true);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Released);
    assert!(rig.bench.store.mount_paths().is_empty());
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that an earlier preparation of a version 2 list that places its tree after its source
/// was prepared again leaves the later preparation's placement at the path and ends released.
#[tokio::test]
async fn an_earlier_v2_preparation_leaves_a_later_ones_placement() {
    let rig = Rig::new("prov-v2-newer").await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    rig.mounted("js.1", 0).await;
    let (earlier, seen) = job("js.1", &key, false);
    run(Arc::clone(&rig.provider.shared), earlier).await;
    assert_eq!(*seen.borrow(), Stage::Released);
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_some()
    );
    rig.provider
        .prepare(super::testing::source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
}

/// Asserts that a version 2 list with a file of a pack that is not there leaves the tree placed,
/// the file of the pack that is there read, and the tree not marked ready (pinned instead).
#[tokio::test]
async fn a_v2_file_of_a_missing_pack_leaves_the_tree_placed_and_not_ready() {
    let rig = Rig::new("prov-v2-missing").await;
    let key = rig.put_list_v2(&list_of_missing_pack(&rig)).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert!(!store.placement(&located.placement).unwrap().ready);
    assert!(rig.holds(1));
}
