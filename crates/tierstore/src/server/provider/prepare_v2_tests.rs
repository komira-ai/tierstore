//! One preparation of a version 2 tree list run to its end: the list placed as its own tree, named
//! by its digest, whose every path resolves as in the tree a version 1 list of the same entries
//! gives, the spans read as the walk queues them, a list that
//! does not hash to its key, a record or a header the reader refuses, a tree the store refuses, a
//! release before or during the placement, and a file whose pack is not there.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::watch;

use super::names::SourceId;
use super::prepare::{Job, run};
use super::sources::Stage;
use super::testing::{AGENT, READ_ONLY, Rig, list_of, member, source};
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

/// The offset in a version 2 list's header of the major version.
const VERSION_AT: usize = 8;
/// The offset in a version 2 list's header of the entries section's offset, then its count.
const ENTRIES_AT: usize = 16;

/// The number of entries in a version 2 list.
fn entry_count(bytes: &[u8]) -> usize {
    u32::from_le_bytes(bytes[ENTRIES_AT + 4..ENTRIES_AT + 8].try_into().unwrap()) as usize
}

/// Sets the kind of the entry `index` of `bytes` to an unknown one (3), keeping its mode: the kind
/// is the top four bits of the u16 at byte 12 of the 32-byte record.
fn break_entry(bytes: &mut [u8], index: usize) {
    let start = u32::from_le_bytes(bytes[ENTRIES_AT..ENTRIES_AT + 4].try_into().unwrap()) as usize;
    let at = start + index * 32 + 12;
    let kind_mode = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
    let broken = (3 << 12) | (kind_mode & 0x0fff);
    bytes[at..at + 2].copy_from_slice(&broken.to_le_bytes());
}

/// Asserts that a version 2 list run to its end leaves the source placed, every file's span in the
/// cache from one grouped read of the pack (its three files are one range: one GET), and the tree
/// marked ready; that the tree is the list itself, named by the list's own digest and not by the
/// records a version 1 list makes; and that every path of it (the root, the directories the paths
/// imply, each entry, and a name that is not there) resolves as it does in the tree of the
/// version 1 list of the same entries.
#[tokio::test]
async fn a_v2_list_run_to_its_end_places_the_list_as_its_tree_and_reads_every_file() {
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
    assert_eq!(pack_reads(&rig), 1);
    assert_eq!(located.tree, tree_list_digest(&key).unwrap());

    let (job1, seen1) = held(&rig, "js.1", &rig.key);
    run(Arc::clone(&rig.provider.shared), job1).await;
    assert_eq!(*seen1.borrow(), Stage::Placed);
    let v1 = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert_ne!(located.tree, v1.tree);
    let paths = [
        "", "bin", "bin/other", "bin/tool", "empty", "lib", "lib/deep", "lib/deep/data", "link",
        "zero", "absent", "bin/tool/below",
    ];
    for path in paths {
        assert_eq!(
            store.resolve(&located.placement, path),
            store.resolve(&v1.placement, path),
            "{path}"
        );
    }
}

/// Asserts that a version 2 list whose bytes do not hash to the digest the job names is refused
/// with the message that names both digests and is not placed (the hash gates the placement, not
/// the reads). The cache checks a `cas/l/` object against its key as it reads it, so the job here is
/// one whose digest is not its key's: the check of the preparation itself.
#[tokio::test]
async fn a_v2_list_that_does_not_hash_to_its_digest_is_refused_and_not_placed() {
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
    assert!(rig.bench.store.mount_paths().is_empty());
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_none()
    );
}

/// Asserts that a version 2 list with an entry of kind 3 is refused with the reader's message
/// naming the entry, and that no tree is placed.
#[tokio::test]
async fn a_v2_list_with_a_bad_entry_is_refused_and_not_placed() {
    let rig = Rig::new("prov-v2-record").await;
    let mut bytes = v2_bytes(&rig);
    break_entry(&mut bytes, 1);
    let key = rig.put_bytes(&bytes).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(
        *seen.borrow(),
        Stage::Refused("tree list entry 1: unknown kind 3".to_owned())
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_none()
    );
}

/// Asserts that a version 2 list with a bad entry after its first file spans (so spans were
/// queued before the fault) is refused with the reader's message and has no placement.
#[tokio::test]
async fn a_v2_list_with_a_bad_entry_after_its_first_spans_is_not_placed() {
    let rig = Rig::new("prov-v2-late-record").await;
    let mut bytes = v2_bytes(&rig);
    let last = entry_count(&bytes) - 1;
    break_entry(&mut bytes, last);
    let key = rig.put_bytes(&bytes).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(
        *seen.borrow(),
        Stage::Refused(format!("tree list entry {last}: unknown kind 3"))
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_none()
    );
}

/// Asserts that a version 2 list with both a bad entry and bytes that do not hash to the job's
/// digest reports the digest mismatch, not the entry.
#[tokio::test]
async fn a_v2_list_with_a_bad_entry_and_the_wrong_digest_reports_the_digest() {
    let rig = Rig::new("prov-v2-both").await;
    let mut bytes = v2_bytes(&rig);
    break_entry(&mut bytes, 1);
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
    let later = rig.provider.prepare(source("js.1", 0), &key, READ_ONLY);
    later.expect("prepared");
    rig.provider.mount(source("js.1", 0)).await.expect("mounted");
    let (earlier, seen) = job("js.1", &key, false);
    run(Arc::clone(&rig.provider.shared), earlier).await;
    assert_eq!(*seen.borrow(), Stage::Released);
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_some()
    );
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

/// A version 2 list of the pack's files in which the file `bin/tool` (the blob 0) has a digest no
/// blob has.
fn list_with_a_wrong_digest(rig: &Rig) -> TreeList {
    let pack = &rig.bench.pack;
    let (_, offset, size) = &pack.blobs[0];
    let wrong = ListEntry {
        path: "bin/tool".to_owned(),
        kind: ListKind::File {
            mode: Mode::new(0o755).unwrap(),
            size: *size,
            source: Some(BlobSource {
                key: pack.key.as_str().into(),
                offset: *offset,
            }),
            sha256: Some(TreeDigest::of(b"other bytes")),
        },
    };
    TreeList {
        entries: vec![
            member(pack, "bin/other", 1, 0o644),
            wrong,
            member(pack, "lib/data", 2, 0o644),
        ],
    }
}

/// Asserts that a version 2 list one of whose files has a wrong digest is read as one group: the
/// two good files are in the cache, the bad one is not, the tree is placed and not marked ready.
#[tokio::test]
async fn a_v2_group_with_a_wrong_digest_places_the_tree_and_keeps_the_good_spans() {
    let rig = Rig::new("prov-v2-wrong-digest").await;
    let key = rig.put_list_v2(&list_with_a_wrong_digest(&rig)).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert!(!store.placement(&located.placement).unwrap().ready);
    assert_eq!(pack_reads(&rig), 1);
    assert!(!rig.holds(0));
    assert!(rig.holds(1));
    assert!(rig.holds(2));
}
