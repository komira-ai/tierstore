//! A tree whose L2 sidecar is not there, placed from its list `cas/l/<id>` as the release before
//! the sidecar placed every tree: a version 2 list as its own tree, the image its bytes, a
//! version 1 list built into records with each file read alone, each counted as a missing sidecar
//! and as a tree placed from its list; and the refusals of that list (none there, bytes that do not
//! hash to the id, an entry the walk refuses), counted as a missing sidecar alone.

use std::sync::Arc;

use super::names::SourceId;
use super::prepare::{list_v2, run};
use super::sources::Stage;
use super::testing::{AGENT, Rig, list_of, member, sidecar_key_of};
use crate::server::local_cache::trees::testing::tree_path;
use crate::server::local_cache::trees::{PreparePrefetch, Resolved, TreesConfig};
use crate::{Mode, Op, TreeDigest, TreeList, tree_list_digest, tree_list_key};

/// Runs the preparation of the source `run`/0 with `key`, held by the provider as its own, to its
/// end; the stage it left.
async fn prepared(rig: &Rig, run_id: &str, key: &str) -> Stage {
    let id = SourceId {
        agent: AGENT.to_owned(),
        run: run_id.to_owned(),
        index: 0,
        attempt: 0,
    };
    let (job, seen) = rig.provider.hold(id, key);
    run(Arc::clone(&rig.provider.shared), job).await;
    seen.borrow().clone()
}

/// The GETs the upstream has had of `key`.
fn gets(rig: &Rig, key: &str) -> u64 {
    rig.bench.rig.calls_on(Op::Get, key)
}

/// The tree of the pack's files and one more, which the rig stores no sidecar of unless a test
/// does: the rig's own tree has one.
fn bare(rig: &Rig) -> TreeList {
    let mut list = list_of(&rig.bench.pack);
    list.entries.push(member(&rig.bench.pack, "more", 1, 0o644));
    list
}

/// Asserts that `/metricz` counts `missing` preparations that found no sidecar and `fallback`
/// trees placed from their list, and no refusal under any reason.
fn counted(rig: &Rig, missing: u64, fallback: u64) {
    let metricz = rig.bench.rig.cache.metrics();
    let mut lines = vec![
        format!("tierstore_l2_sidecar_missing_total {missing}\n"),
        format!("tierstore_l2_sidecar_fallback_total {fallback}\n"),
    ];
    for reason in [
        "v1_list",
        "missing_sidecar",
        "unknown_cell_digest",
        "unknown_version",
    ] {
        lines.push(format!(
            "tierstore_list_refused_total{{reason=\"{reason}\"}} 0\n"
        ));
    }
    for line in lines {
        assert!(metricz.contains(&line), "{line} in {metricz}");
    }
}

/// Asserts that a tree whose version 2 list is in the store and whose sidecar is not is placed
/// from the list: the tree is named by the id, its image is the list byte for byte, every file is
/// read and the tree marked ready; the upstream is asked for the sidecar and the list is read once;
/// and the preparation is counted as a missing sidecar and a tree placed from its list, not as a
/// refusal.
#[tokio::test]
async fn a_v2_list_with_no_sidecar_is_placed_as_its_list_and_counted() {
    let rig = Rig::new("prov-fallback-v2").await;
    let v2 = bare(&rig).encode_v2().unwrap();
    let key = rig.put_bytes(&v2).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    let digest = tree_list_digest(&key).unwrap();
    assert_eq!(located.tree, digest);
    let image = std::fs::read(tree_path(&rig.bench.rig, &digest).join("image")).unwrap();
    assert_eq!(image, v2);
    assert!(store.placement(&located.placement).unwrap().ready);
    assert!((0..3).all(|blob| rig.holds(blob)));
    // The cache asks the upstream for the sidecar (the kind of call a miss makes is the cache's
    // own), and is told there is none.
    let sidecar = sidecar_key_of(&key);
    let log = rig.bench.rig.upstream.call_log();
    assert!(log.iter().any(|(_, asked)| *asked == sidecar), "{log:?}");
    assert_eq!(gets(&rig, &key), 1);
    counted(&rig, 1, 1);
}

/// Asserts that a tree whose version 1 list is in the store (no sidecar is made of one) is placed
/// from the list as records, with the list's paths, modes and sizes; that each of its three files
/// is read alone (three reads of the pack, as an open of each makes) and the tree marked ready;
/// and that it is counted as a missing sidecar and a tree placed from its list.
#[tokio::test]
async fn a_v1_list_with_no_sidecar_is_placed_and_each_file_read_alone() {
    let rig = Rig::new("prov-fallback-v1").await;
    let key = rig.put_list(&list_of(&rig.bench.pack)).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    let tool = store.resolve(&located.placement, "bin/tool").unwrap();
    assert!(
        matches!(tool, Resolved::File { mode, size: 1000, .. } if mode == Mode::new(0o755).unwrap())
    );
    let empty = store.resolve(&located.placement, "empty").unwrap();
    assert!(
        matches!(empty, Resolved::Dir { mode, entries: 0 } if mode == Mode::new(0o1777).unwrap())
    );
    assert!(store.placement(&located.placement).unwrap().ready);
    assert!((0..3).all(|blob| rig.holds(blob)));
    assert_eq!(gets(&rig, &rig.bench.pack.key), 3);
    assert_eq!(gets(&rig, &key), 1);
    counted(&rig, 1, 1);
}

/// Asserts that with `trees.prepare_prefetch = "none"` a version 1 list with no sidecar is placed
/// and no file of it read, and is counted as placed from its list.
#[tokio::test]
async fn a_v1_list_with_no_sidecar_and_no_prefetch_reads_no_file() {
    let config = TreesConfig {
        prepare_prefetch: PreparePrefetch::None,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-fallback-lazy", config, |_| {}).await;
    let key = rig.put_list(&list_of(&rig.bench.pack)).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert!(!store.placement(&located.placement).unwrap().ready);
    assert_eq!(gets(&rig, &rig.bench.pack.key), 0);
    counted(&rig, 1, 1);
}

/// Asserts that a tree with neither a sidecar nor a list is refused, naming the list's key, is
/// not placed, and is counted as a missing sidecar alone.
#[tokio::test]
async fn a_tree_with_no_sidecar_and_no_list_is_refused_and_counted_missing() {
    let rig = Rig::new("prov-fallback-none").await;
    let key = tree_list_key(b"never written");
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!("no tree list at {key}"))
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    counted(&rig, 1, 0);
}

/// Asserts that a version 2 list with no sidecar whose entry the walk refuses (an unknown kind) is
/// refused with the walk's reason, is not placed and reads no file, and is counted as a missing
/// sidecar alone.
#[tokio::test]
async fn a_v2_list_with_no_sidecar_and_a_bad_entry_is_refused_and_not_placed() {
    let rig = Rig::new("prov-fallback-entry").await;
    let mut v2 = bare(&rig).encode_v2().unwrap();
    // The kind is the top four bits of the u16 at byte 12 of entry 1's 32-byte record; the entries
    // section's offset is the u32 at byte 16 of the header.
    let entries = u32::from_le_bytes(v2[16..20].try_into().unwrap()) as usize;
    let at = entries + 32 + 12;
    let kind_mode = u16::from_le_bytes(v2[at..at + 2].try_into().unwrap());
    v2[at..at + 2].copy_from_slice(&((3 << 12) | (kind_mode & 0x0fff)).to_le_bytes());
    let key = rig.put_bytes(&v2).await;
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused("tree list entry 1: unknown kind 3".to_owned())
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    assert_eq!(gets(&rig, &rig.bench.pack.key), 0);
    counted(&rig, 1, 0);
}

/// Asserts that a version 2 list read for want of a sidecar is taken only if its bytes hash to the
/// tree id: bytes of another digest are refused, naming both digests, and the list's own pass.
#[test]
fn a_v2_list_that_does_not_hash_to_the_id_is_refused() {
    let list = TreeList::default();
    let v2 = list.encode_v2().unwrap();
    let ours = TreeDigest::of(&v2);
    assert_eq!(list_v2(v2.clone(), &ours), Ok(v2.clone()));
    let other = TreeDigest::of(b"another tree");
    assert_eq!(
        list_v2(v2, &other),
        Err(Stage::Refused(format!(
            "tree list digest mismatch: expected {other}, the bytes hash to {ours}"
        )))
    );
}

/// Asserts that a version 1 list with no sidecar whose reads do not fit under the node's bound of
/// queued reads is placed with none queued, as a version 2 list is: the tree placed and not marked
/// ready, no file read, nothing held, the skip counted, and the tree counted as placed from its
/// list.
#[tokio::test]
async fn a_v1_list_with_no_sidecar_over_the_queued_bound_is_placed_with_no_read() {
    let rig = Rig::with("prov-fallback-bound", TreesConfig::default(), |settings| {
        settings.max_queued_bytes = 0;
    })
    .await;
    let key = rig.put_list(&list_of(&rig.bench.pack)).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    let reads = &rig.provider.shared.queued;
    assert_eq!((reads.bytes(), reads.skipped()), (0, 1));
    assert_eq!(gets(&rig, &rig.bench.pack.key), 0);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    assert!(!store.placement(&located.placement).unwrap().ready);
    counted(&rig, 1, 1);
}
