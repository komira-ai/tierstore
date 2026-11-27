//! One preparation of a tree from its L2 sidecar run to its end: its version 2 projection placed as
//! its own tree, named by the tree id, the spans read as the walk queues them; a sidecar whose
//! projection is not the tree, one of another tree, a list of the old version or a major version
//! at the sidecar's key, a cell shift other than 20, a record the reader refuses, a pack flag it
//! does not know; a tree the store refuses, a release before or during the placement, and a file
//! whose pack is not there.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tierstore_list::{ENTRY_LEN, Section, TreeListL2};
use tokio::sync::watch;

use super::names::SourceId;
use super::prepare::{Job, run};
use super::sources::Stage;
use super::testing::{AGENT, READ_ONLY, Rig, list_of, member, sidecar_key_of, sidecar_of, source};
use crate::server::local_cache::trees::TreesConfig;
use crate::{BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_digest};

/// The id of the source `run`/0.
fn id_of(run: &str) -> SourceId {
    SourceId {
        agent: AGENT.to_owned(),
        run: run.to_owned(),
        index: 0,
        attempt: 0,
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

/// Asserts that a tree run to its end from its L2 sidecar leaves the source placed, every file's
/// span in the cache from one grouped read of the pack (its three files are one range: one GET),
/// and the tree marked ready; that the tree is the sidecar's version 2 projection, named by the
/// tree id, the digest of the key the job names; that the sidecar is read once and the list
/// `cas/l/<id>` never; and that nothing is counted as refused, as a missing sidecar or as a tree
/// placed from its list.
#[tokio::test]
async fn a_tree_run_to_its_end_from_its_sidecar_is_placed_as_its_list_and_reads_every_file() {
    let rig = Rig::new("prov-v2-ready").await;
    let key = rig.key.clone();
    let (job, seen) = held(&rig, "js.2", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.2", 0)).unwrap();
    assert!(store.placement(&located.placement).unwrap().ready);
    assert!((0..3).all(|blob| rig.holds(blob)));
    assert_eq!(pack_reads(&rig), 1);
    assert_eq!(located.tree, tree_list_digest(&key).unwrap());
    assert_eq!(located.tree, TreeDigest::of(&v2_bytes(&rig)));
    assert_eq!(rig.bench.rig.calls_on(Op::Get, &key), 0);
    assert_eq!(rig.bench.rig.calls_on(Op::Get, &sidecar_key_of(&key)), 1);
    let metricz = rig.bench.rig.cache.metrics();
    for line in [
        "tierstore_l2_sidecar_missing_total 0\n",
        "tierstore_l2_sidecar_fallback_total 0\n",
    ] {
        assert!(metricz.contains(line), "{line} in {metricz}");
    }
}

/// A tree of the pack's files and one more, whose version 2 list is stored with no sidecar, for a
/// test to store the object at the sidecar's key: the list's key and bytes.
async fn bare_tree(rig: &Rig) -> (String, Vec<u8>) {
    let mut list = list_of(&rig.bench.pack);
    list.entries.push(member(&rig.bench.pack, "more", 1, 0o644));
    let v2 = list.encode_v2().unwrap();
    (rig.put_bytes(&v2).await, v2)
}

/// The first byte of `section` in the L2 list `l2`.
fn section_at(l2: &[u8], section: Section) -> usize {
    let list = TreeListL2::open(l2.to_vec()).expect("an L2 list");
    usize::try_from(list.section_range(section).start).unwrap()
}

/// Runs the preparation of the source `run`/0 with `key` to its end; the stage it left.
async fn prepared(rig: &Rig, run_id: &str, key: &str) -> Stage {
    let (job, seen) = held(rig, run_id, key);
    run(Arc::clone(&rig.provider.shared), job).await;
    seen.borrow().clone()
}

/// Asserts that nothing is placed at the source `run`/0, no file of the pack was read, no refusal
/// of a version was counted, and no missing sidecar or tree placed from its list: a sidecar that
/// is there and refused is never answered with the list.
fn nothing_placed_or_counted(rig: &Rig, run_id: &str) {
    assert!(rig.bench.store.mount_paths().is_empty());
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of(run_id, 0))
            .is_none()
    );
    assert_eq!(pack_reads(rig), 0);
    let metricz = rig.bench.rig.cache.metrics();
    for reason in [
        "v1_list",
        "missing_sidecar",
        "unknown_cell_digest",
        "unknown_version",
    ] {
        let line = format!("tierstore_list_refused_total{{reason=\"{reason}\"}} 0\n");
        assert!(metricz.contains(&line), "{line} in {metricz}");
    }
    for line in [
        "tierstore_l2_sidecar_missing_total 0\n",
        "tierstore_l2_sidecar_fallback_total 0\n",
    ] {
        assert!(metricz.contains(line), "{line} in {metricz}");
    }
}

/// Asserts that a sidecar whose version 2 projection does not hash to the tree id (a mode of an
/// entry edited, which the header's CRC does not cover) is refused as corrupt, with the digest the
/// projection has, is never placed, and is not counted as a refusal of the version.
#[tokio::test]
async fn a_sidecar_whose_projection_is_not_the_tree_is_refused_as_corrupt_and_not_placed() {
    let rig = Rig::new("prov-l2-projection").await;
    let (key, v2) = bare_tree(&rig).await;
    let mut l2 = sidecar_of(&rig.bench.pack, &v2);
    // The mode of entry 1 is the low 12 bits of the u16 at byte 12 of its record.
    let at = section_at(&l2, Section::Entries) + ENTRY_LEN + 12;
    l2[at] ^= 0o004;
    let found = TreeListL2::open(l2.clone()).unwrap().projection_digest();
    rig.put_sidecar(&key, &l2).await;
    let sidecar = sidecar_key_of(&key);
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!(
            "the L2 sidecar {sidecar} is refused: the version 2 list it holds hashes to {}, \
             not to its tree id",
            hex::encode(found)
        ))
    );
    nothing_placed_or_counted(&rig, "js.1");
}

/// Asserts that the sidecar of another tree stored at a tree's sidecar key is refused as the list
/// of that other tree, and not placed.
#[tokio::test]
async fn a_sidecar_of_another_tree_is_refused_and_not_placed() {
    let rig = Rig::new("prov-l2-other").await;
    let (key, _) = bare_tree(&rig).await;
    let other = TreeList {
        entries: vec![member(&rig.bench.pack, "other", 2, 0o644)],
    };
    let other_v2 = other.encode_v2().unwrap();
    rig.put_sidecar(&key, &sidecar_of(&rig.bench.pack, &other_v2))
        .await;
    let sidecar = sidecar_key_of(&key);
    let ours = tree_list_digest(&key).unwrap();
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!(
            "the L2 sidecar {sidecar} is refused: the list is of tree {}, not {ours}",
            TreeDigest::of(&other_v2)
        ))
    );
    nothing_placed_or_counted(&rig, "js.1");
}

/// Asserts that a version 2 list stored where the sidecar belongs is refused with its own reason,
/// `tree list not supported: v1_list` (the design's "v1" is the version 2 list), and counted under
/// it alone: it is not a missing sidecar.
#[tokio::test]
async fn a_list_of_the_old_version_at_the_sidecar_key_is_refused_v1_list_and_counted() {
    let rig = Rig::new("prov-l2-v1").await;
    let (key, v2) = bare_tree(&rig).await;
    rig.put_sidecar(&key, &v2).await;
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused("tree list not supported: v1_list".to_owned())
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    let metricz = rig.bench.rig.cache.metrics();
    for line in [
        "tierstore_list_refused_total{reason=\"v1_list\"} 1\n",
        "tierstore_list_refused_total{reason=\"missing_sidecar\"} 0\n",
        "tierstore_l2_sidecar_missing_total 0\n",
        "tierstore_l2_sidecar_fallback_total 0\n",
    ] {
        assert!(metricz.contains(line), "{line} in {metricz}");
    }
}

/// Asserts that an L2 sidecar of a major version this release does not read is refused with its
/// reason, `unknown_version`, and counted under it.
#[tokio::test]
async fn a_sidecar_of_another_major_version_is_refused_unknown_version_and_counted() {
    let rig = Rig::new("prov-l2-version").await;
    let (key, v2) = bare_tree(&rig).await;
    let mut l2 = sidecar_of(&rig.bench.pack, &v2);
    l2[8..10].copy_from_slice(&2u16.to_le_bytes());
    rig.put_sidecar(&key, &l2).await;
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused("tree list not supported: unknown_version".to_owned())
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    let metricz = rig.bench.rig.cache.metrics();
    assert!(
        metricz.contains("tierstore_list_refused_total{reason=\"unknown_version\"} 1\n"),
        "{metricz}"
    );
}

/// Asserts that an object at the sidecar's key that is no list at all (bytes of no known magic, and
/// none) is refused as a malformed sidecar, never placed, and not counted as a refusal of the
/// version.
#[tokio::test]
async fn a_sidecar_that_is_no_list_is_refused_bad_magic_and_not_placed() {
    let rig = Rig::new("prov-l2-garbage").await;
    let (key, _) = bare_tree(&rig).await;
    rig.put_sidecar(&key, b"not a tree list").await;
    let sidecar = sidecar_key_of(&key);
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!("the L2 sidecar {sidecar} is refused: bad magic"))
    );
    nothing_placed_or_counted(&rig, "js.1");
}

/// Asserts that a sidecar naming a pack of a cell shift other than 20 is refused, never placed,
/// and not counted as a refusal of the version.
#[tokio::test]
async fn a_sidecar_with_a_cell_shift_other_than_20_is_refused() {
    let rig = Rig::new("prov-l2-shift").await;
    let (key, v2) = bare_tree(&rig).await;
    let mut l2 = sidecar_of(&rig.bench.pack, &v2);
    // The cell shift of pack 0 is byte 28 of its 48-byte record.
    let shift = section_at(&l2, Section::Packs) + 28;
    l2[shift] = 21;
    rig.put_sidecar(&key, &l2).await;
    let sidecar = sidecar_key_of(&key);
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!(
            "the L2 sidecar {sidecar} is refused: pack 0: cell shift 21, not 20"
        ))
    );
    nothing_placed_or_counted(&rig, "js.1");
}

/// Asserts that a pack flag this release does not know (bit 7 of `pack_flags`, byte 29 of the
/// pack's record) is ignored: the tree is placed and every file read.
#[tokio::test]
async fn an_unknown_pack_flags_bit_is_ignored_and_the_tree_placed() {
    let rig = Rig::new("prov-l2-flags").await;
    let (key, v2) = bare_tree(&rig).await;
    let mut l2 = sidecar_of(&rig.bench.pack, &v2);
    let flags = section_at(&l2, Section::Packs) + 29;
    l2[flags] |= 0x80;
    rig.put_sidecar(&key, &l2).await;
    assert_eq!(prepared(&rig, "js.1", &key).await, Stage::Placed);
    assert!((0..3).all(|blob| rig.holds(blob)));
}

/// Asserts that a tree whose list has an entry of kind 3 is refused by the sidecar's reader with
/// its message naming the entry, and that no tree is placed.
#[tokio::test]
async fn a_sidecar_with_a_bad_entry_is_refused_and_not_placed() {
    let rig = Rig::new("prov-v2-record").await;
    let mut bytes = v2_bytes(&rig);
    break_entry(&mut bytes, 1);
    let key = rig.put_tree_bytes(&bytes).await;
    let sidecar = sidecar_key_of(&key);
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!(
            "the L2 sidecar {sidecar} is refused: entry 1: unknown kind 3"
        ))
    );
    nothing_placed_or_counted(&rig, "js.1");
}

/// Asserts that a tree whose list has a bad entry after its files is refused with the reader's
/// message, has no placement and reads no file: the sidecar is checked whole before a span is
/// queued.
#[tokio::test]
async fn a_sidecar_with_a_bad_entry_after_its_files_is_not_placed() {
    let rig = Rig::new("prov-v2-late-record").await;
    let mut bytes = v2_bytes(&rig);
    let last = entry_count(&bytes) - 1;
    break_entry(&mut bytes, last);
    let key = rig.put_tree_bytes(&bytes).await;
    let sidecar = sidecar_key_of(&key);
    assert_eq!(
        prepared(&rig, "js.1", &key).await,
        Stage::Refused(format!(
            "the L2 sidecar {sidecar} is refused: entry {last}: unknown kind 3"
        ))
    );
    nothing_placed_or_counted(&rig, "js.1");
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
    let key = rig.key.clone();
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
    let key = rig.key.clone();
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
    let key = rig.key.clone();
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
    let key = rig.key.clone();
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
    let key = rig.key.clone();
    let later = rig.provider.prepare(source("js.1", 0), &key, READ_ONLY);
    later.expect("prepared");
    rig.provider
        .mount(source("js.1", 0))
        .await
        .expect("mounted");
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
