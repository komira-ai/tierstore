//! One preparation run to its end, and the reads of its files: the stage it leaves the source at,
//! whether the tree is marked ready, a release that comes before the placement or during the
//! reads, a stop of the process, and the bound of a read.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::watch;

use super::groups::Group;
use super::names::SourceId;
use super::prepare::{Job, fill, report, run};
use super::queued::{Queued, admit};
use super::sources::Shared;
use super::sources::Stage;
use super::testing::{AGENT, READ_ONLY, Rig, member, source};
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::TreesConfig;
use crate::{BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_digest};

/// A preparation of the source `run`/0 with `key` that the provider does not hold (its source was
/// released, or prepared again by another), and the stage it reports.
fn job(run: &str, key: &str, released: bool) -> (Job, watch::Receiver<Stage>) {
    let (stage, seen) = watch::channel(Stage::Preparing);
    let job = Job {
        id: SourceId {
            agent: AGENT.to_owned(),
            run: run.to_owned(),
            index: 0,
        },
        key: key.to_owned(),
        digest: tree_list_digest(key).expect("a list key"),
        stage: Arc::new(stage),
        released: Arc::new(AtomicBool::new(released)),
    };
    (job, seen)
}

/// The preparation of the source `run`/0 with `key`, held by the provider as its own.
fn held(rig: &Rig, run: &str, key: &str) -> (Job, watch::Receiver<Stage>) {
    let id = SourceId {
        agent: AGENT.to_owned(),
        run: run.to_owned(),
        index: 0,
    };
    rig.provider.hold(id, key)
}

/// The GETs the upstream has had of the pack.
fn pack_reads(rig: &Rig) -> u64 {
    rig.bench.rig.calls_on(Op::Get, &rig.bench.pack.key)
}

/// The reads of the pack's three blobs, each alone, in the pack at `pack`.
fn singles(rig: &Rig, pack: &str) -> Vec<Group> {
    rig.bench
        .pack
        .blobs
        .iter()
        .map(|(sha, offset, size)| Group {
            pack: pack.into(),
            parts: vec![(*offset, *size, TreeDigest::from_hex(sha).unwrap())],
        })
        .collect()
}

/// `groups` queued as a preparation queues them, under no bound.
fn queued(
    shared: &Shared,
    groups: Vec<Group>,
) -> futures::stream::Iter<std::vec::IntoIter<Queued>> {
    let reads = admit(&shared.queued, groups, u64::MAX).expect("no bound");
    futures::stream::iter(reads)
}

/// The ends of a fill with the release flag `released` and a stage that is never released.
fn ends(released: &AtomicBool) -> (&AtomicBool, watch::Receiver<Stage>) {
    (released, watch::channel(Stage::Placed).1)
}

/// Reads `groups`, with no refusal.
async fn fill_groups(shared: &Shared, groups: Vec<Group>, released: &AtomicBool) -> (usize, usize) {
    let reads = queued(shared, groups);
    fill(shared, reads, ends(released), &AtomicBool::new(false)).await
}

/// Asserts that the spans of a refused list still queued are not read: each is counted as not
/// read.
#[tokio::test]
async fn a_refused_list_reads_no_more_files() {
    let rig = Rig::new("prov-fill-refused").await;
    let shared = &rig.provider.shared;
    let spans = queued(shared, singles(&rig, &rig.bench.pack.key));
    let unreleased = AtomicBool::new(false);
    let read = fill(shared, spans, ends(&unreleased), &AtomicBool::new(true)).await;
    assert_eq!(read, (0, 3));
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that a preparation run to its end leaves the source placed, every file in the cache,
/// and the tree marked ready.
#[tokio::test]
async fn a_tree_whose_files_are_all_read_is_marked_ready() {
    let rig = Rig::new("prov-run-ready").await;
    let (job, seen) = held(&rig, "js.1", &rig.key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    let located = rig
        .bench
        .store
        .locate_fs(&Rig::place_of("js.1", 0))
        .unwrap();
    assert!(rig.bench.store.placement(&located.placement).unwrap().ready);
    assert!((0..3).all(|blob| rig.holds(blob)));
}

/// Asserts that a tree one of whose files is not read (its bytes do not hash to the digest the list
/// gives) is placed, its other files read, and not marked ready: an open of that file reads it, and
/// fails as the mount fails a read.
#[tokio::test]
async fn a_tree_with_a_file_not_read_is_placed_and_not_marked_ready() {
    let rig = Rig::new("prov-run-unready").await;
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
            // A digest no blob of the pack has: the bytes the fill reads do not hash to it.
            sha256: Some(TreeDigest::of(b"other bytes")),
        },
    };
    let list = TreeList {
        entries: vec![member(pack, "bin/other", 1, 0o644), wrong],
    };
    let key = rig.put_list_v2(&list).await;
    let (job, seen) = held(&rig, "js.1", &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    let located = rig
        .bench
        .store
        .locate_fs(&Rig::place_of("js.1", 0))
        .unwrap();
    assert!(!rig.bench.store.placement(&located.placement).unwrap().ready);
    assert!(rig.holds(1));
    assert!(!rig.holds(0));
}

/// Asserts that a source released before its tree is placed gives the placement up as soon as it
/// is made: nothing is left at its path, no file is read, and the stage is released.
#[tokio::test]
async fn a_source_released_before_its_tree_is_placed_gives_the_placement_up() {
    let rig = Rig::new("prov-run-released").await;
    let (job, seen) = job("js.1", &rig.key, true);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Released);
    assert!(rig.bench.store.mount_paths().is_empty());
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that a source released while its files are being read ends its preparation without
/// error once the reads in flight end, its tree gone with its placement.
///
/// The reads are queued as soon as the list is walked, before the tree is placed, so the read can
/// be at the upstream before the stage is `Placed`: the test waits for each of the two, the stage
/// first.
#[tokio::test]
async fn a_source_released_during_its_reads_ends_with_its_tree_gone() {
    let rig = Rig::new("prov-run-release-reads").await;
    let gate = rig.bench.rig.tamper.hold_key(&rig.bench.pack.key);
    let (job, mut seen) = held(&rig, "js.1", &rig.key);
    let released = Arc::clone(&job.released);
    let running = tokio::spawn(run(Arc::clone(&rig.provider.shared), job));
    let reported = seen.wait_for(|stage| *stage != Stage::Preparing).await;
    let reported = reported.expect("the preparation reports").clone();
    assert_eq!(reported, Stage::Placed);
    // The three files are one range of the list: one read of the pack, held at the upstream.
    until("the read is at the upstream", || pack_reads(&rig) == 1).await;
    released.store(true, std::sync::atomic::Ordering::SeqCst);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    store.release(&located.placement).unwrap();
    gate.add_permits(1);
    running.await.unwrap();
    assert!(store.placement(&located.placement).is_none());
    assert_eq!(store.sizes(), (0, 0));
}

/// Asserts that a provider whose process is stopping reads no file: every span is counted as not
/// read.
#[tokio::test]
async fn a_halted_provider_reads_no_file() {
    let rig = Rig::new("prov-fill-halted").await;
    rig.provider.halt();
    let released = AtomicBool::new(false);
    let read = fill_groups(
        &rig.provider.shared,
        singles(&rig, &rig.bench.pack.key),
        &released,
    )
    .await;
    assert_eq!(read, (0, 3));
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that a released source reads no more files.
#[tokio::test]
async fn a_released_source_reads_no_more_files() {
    let rig = Rig::new("prov-fill-released").await;
    let released = AtomicBool::new(true);
    let read = fill_groups(
        &rig.provider.shared,
        singles(&rig, &rig.bench.pack.key),
        &released,
    )
    .await;
    assert_eq!(read, (0, 3));
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that a file not read within the bound of a file is counted as not read (its read goes on
/// in the cache, and an open joins it), and that a file whose object is not there is too.
#[tokio::test]
async fn files_not_read_within_their_bound_or_not_there_are_counted() {
    let rig = Rig::with("prov-fill-bound", TreesConfig::default(), |settings| {
        settings.fill_timeout = Duration::ZERO;
    })
    .await;
    let _gate = rig.bench.rig.tamper.hold_key(&rig.bench.pack.key);
    let released = AtomicBool::new(false);
    let read = fill_groups(
        &rig.provider.shared,
        singles(&rig, &rig.bench.pack.key),
        &released,
    )
    .await;
    assert_eq!(read, (0, 3));

    let rig = Rig::new("prov-fill-missing").await;
    let missing = singles(&rig, "cas/pack/missing");
    let read = fill_groups(&rig.provider.shared, missing, &released).await;
    assert_eq!(read, (0, 3));
}

/// Asserts that a release is the last stage of a source: a stage reported after it does not
/// replace it, and one reported before it does.
#[test]
fn a_released_stage_is_never_replaced() {
    let (stage, seen) = watch::channel(Stage::Preparing);
    report(&stage, Stage::Placed);
    assert_eq!(*seen.borrow(), Stage::Placed);
    stage.send_replace(Stage::Released);
    for later in [
        Stage::Placed,
        Stage::Failed("x".into()),
        Stage::Refused("y".into()),
    ] {
        report(&stage, later);
        assert_eq!(*seen.borrow(), Stage::Released);
    }
}

/// Asserts that a preparation that places its tree after its source was released and prepared again
/// leaves the placement to the later preparation, whose source it now is: the tree stays at the
/// path and the earlier preparation ends released.
#[tokio::test]
async fn an_earlier_preparation_leaves_a_later_ones_placement() {
    let rig = Rig::new("prov-run-newer").await;
    rig.mounted("js.1", 0).await;
    let (earlier, seen) = job("js.1", &rig.key, false);
    run(Arc::clone(&rig.provider.shared), earlier).await;
    assert_eq!(*seen.borrow(), Stage::Released);
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_some()
    );
    assert_eq!(
        rig.provider.mount(source("js.1", 0)).await,
        Ok(Rig::path_of("js.1", 0))
    );
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
}

/// The group of the pack's blobs `blobs` read together, each part with the digest of its blob except
/// that of the blob `wrong`, if any, which is given a digest no blob has.
fn group_of(rig: &Rig, blobs: &[usize], wrong: Option<usize>) -> Group {
    let pack = &rig.bench.pack;
    let parts = blobs
        .iter()
        .map(|blob| {
            let (sha, offset, size) = &pack.blobs[*blob];
            let digest = if wrong == Some(*blob) {
                TreeDigest::of(b"other bytes")
            } else {
                TreeDigest::from_hex(sha).unwrap()
            };
            (*offset, *size, digest)
        })
        .collect();
    Group {
        pack: pack.key.as_str().into(),
        parts,
    }
}

/// Reads `group` alone, with no release and no refusal.
async fn fill_group(shared: &Shared, group: Group, refused: bool) -> (usize, usize) {
    let reads = queued(shared, vec![group]);
    let unreleased = AtomicBool::new(false);
    fill(shared, reads, ends(&unreleased), &AtomicBool::new(refused)).await
}

/// Asserts that a group of three spans read in one GET of the pack is counted as three spans in
/// the cache.
#[tokio::test]
async fn a_group_of_several_spans_is_one_read_and_all_its_spans_are_counted() {
    let rig = Rig::new("prov-group-ok").await;
    let group = group_of(&rig, &[0, 1, 2], None);
    assert_eq!(fill_group(&rig.provider.shared, group, false).await, (3, 0));
    assert_eq!(pack_reads(&rig), 1);
    assert!((0..3).all(|blob| rig.holds(blob)));
}

/// Asserts that in a group of three spans one of which has a wrong digest the two good spans are
/// counted as read and kept and the bad one as failed.
#[tokio::test]
async fn a_group_with_a_span_of_a_wrong_digest_counts_the_good_spans_and_the_bad_one() {
    let rig = Rig::new("prov-group-wrong").await;
    let group = group_of(&rig, &[0, 1, 2], Some(1));
    assert_eq!(fill_group(&rig.provider.shared, group, false).await, (2, 1));
    assert_eq!(pack_reads(&rig), 1);
    assert!(rig.holds(0));
    assert!(!rig.holds(1));
    assert!(rig.holds(2));
}

/// Asserts that a group not read within the bound of a read has every span counted as not read.
#[tokio::test]
async fn a_group_not_read_within_its_bound_fails_every_span() {
    let rig = Rig::with("prov-group-bound", TreesConfig::default(), |settings| {
        settings.fill_timeout = Duration::ZERO;
    })
    .await;
    let _gate = rig.bench.rig.tamper.hold_key(&rig.bench.pack.key);
    let group = group_of(&rig, &[0, 1, 2], None);
    assert_eq!(fill_group(&rig.provider.shared, group, false).await, (0, 3));
    assert!(!(0..3).any(|blob| rig.holds(blob)));
}

/// Asserts that a group of several spans of a pack that is not there has every span counted as not
/// read.
#[tokio::test]
async fn a_group_of_a_missing_pack_fails_every_span() {
    let rig = Rig::new("prov-group-missing").await;
    let mut group = group_of(&rig, &[0, 1, 2], None);
    group.pack = "cas/pack/missing".into();
    assert_eq!(fill_group(&rig.provider.shared, group, false).await, (0, 3));
}

/// Asserts that a group queued from a refused list is not read: its spans are counted as not read
/// and the upstream is not asked.
#[tokio::test]
async fn a_refused_list_reads_no_group_of_several_spans() {
    let rig = Rig::new("prov-group-refused").await;
    let group = group_of(&rig, &[0, 1, 2], None);
    assert_eq!(fill_group(&rig.provider.shared, group, true).await, (0, 3));
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that a halted provider counts every span of a group as not read and reads nothing.
#[tokio::test]
async fn a_halted_provider_reads_no_group() {
    let rig = Rig::new("prov-group-halted").await;
    rig.provider.halt();
    let group = group_of(&rig, &[0, 1, 2], None);
    assert_eq!(fill_group(&rig.provider.shared, group, false).await, (0, 3));
    assert_eq!(pack_reads(&rig), 0);
}
