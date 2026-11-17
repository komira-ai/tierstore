//! One preparation run to its end, and the reads of its files: the stage it leaves the source at,
//! whether the tree is marked ready, a release that comes before the placement or during the
//! reads, a stop of the process, the bound of a read, and spans shared by several files.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::watch;

use super::names::SourceId;
use super::prepare::{Job, fill, run};
use super::sources::Stage;
use super::testing::{AGENT, Rig, member};
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::TreesConfig;
use crate::{
    BlobSource, BuiltFile, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_digest,
};

/// A preparation of the source `run`/0 with `key`, released or not, and the stage it reports.
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

/// The GETs the upstream has had of the pack.
fn pack_reads(rig: &Rig) -> u64 {
    rig.bench.rig.calls_on(Op::Get, &rig.bench.pack.key)
}

/// The files of the pack, as the built tree has them, with the first listed twice.
fn files(rig: &Rig) -> Vec<BuiltFile> {
    let pack = &rig.bench.pack;
    let file = |path: &str, blob: usize| {
        let (sha, offset, size) = &pack.blobs[blob];
        BuiltFile {
            path: path.to_owned(),
            sha256: TreeDigest::from_hex(sha).unwrap(),
            size: *size,
            source: Some(BlobSource {
                key: pack.key.as_str().into(),
                offset: *offset,
            }),
        }
    };
    let empty = BuiltFile {
        path: "zero".to_owned(),
        sha256: TreeDigest::of(b""),
        size: 0,
        source: None,
    };
    vec![
        file("a", 0),
        file("b", 0),
        file("c", 1),
        file("d", 2),
        empty,
    ]
}

/// Asserts that a preparation run to its end leaves the source placed, every file in the cache,
/// and the tree marked ready.
#[tokio::test]
async fn a_tree_whose_files_are_all_read_is_marked_ready() {
    let rig = Rig::new("prov-run-ready").await;
    let (job, seen) = job("js.1", &rig.key, false);
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
            sha256: Some(TreeDigest::from_hex(&pack.blobs[1].0).unwrap()),
        },
    };
    let list = TreeList {
        entries: vec![member(pack, "bin/other", 1, 0o644), wrong],
    };
    let key = rig.put_list(&list).await;
    let (job, seen) = job("js.1", &key, false);
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
#[tokio::test]
async fn a_source_released_during_its_reads_ends_with_its_tree_gone() {
    let rig = Rig::new("prov-run-release-reads").await;
    let gate = rig.bench.rig.tamper.hold_key(&rig.bench.pack.key);
    let (job, seen) = job("js.1", &rig.key, false);
    let released = Arc::clone(&job.released);
    let running = tokio::spawn(run(Arc::clone(&rig.provider.shared), job));
    until("the three reads are at the upstream", || {
        pack_reads(&rig) == 3
    })
    .await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    released.store(true, std::sync::atomic::Ordering::SeqCst);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).unwrap();
    store.release(&located.placement).unwrap();
    gate.add_permits(3);
    running.await.unwrap();
    assert!(store.placement(&located.placement).is_none());
    assert_eq!(store.sizes(), (0, 0));
}

/// Asserts that files that share one span are read once, that a file of size 0 is not read, and
/// that every span is counted as in the cache.
#[tokio::test]
async fn files_sharing_a_span_are_read_once() {
    let rig = Rig::new("prov-fill-shared").await;
    let released = AtomicBool::new(false);
    let read = fill(&rig.provider.shared, &files(&rig), &released).await;
    assert_eq!(read, (3, 0));
    assert_eq!(pack_reads(&rig), 3);
}

/// Asserts that a provider whose process is stopping reads no file: every span is counted as not
/// read.
#[tokio::test]
async fn a_halted_provider_reads_no_file() {
    let rig = Rig::new("prov-fill-halted").await;
    rig.provider.halt();
    let released = AtomicBool::new(false);
    let read = fill(&rig.provider.shared, &files(&rig), &released).await;
    assert_eq!(read, (0, 3));
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that a released source reads no more files.
#[tokio::test]
async fn a_released_source_reads_no_more_files() {
    let rig = Rig::new("prov-fill-released").await;
    let released = AtomicBool::new(true);
    let read = fill(&rig.provider.shared, &files(&rig), &released).await;
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
    let read = fill(&rig.provider.shared, &files(&rig), &released).await;
    assert_eq!(read, (0, 3));

    let rig = Rig::new("prov-fill-missing").await;
    let mut missing = files(&rig);
    for file in &mut missing {
        file.source = file.source.take().map(|source| BlobSource {
            key: "cas/pack/missing".into(),
            ..source
        });
    }
    let read = fill(&rig.provider.shared, &missing, &released).await;
    assert_eq!(read, (0, 3));
}
