//! The heap the reads queued by preparations hold while every read of the node is taken, as when
//! the upstream does not answer: a release drops a source's queued reads at once, the reads of
//! every source together stay under the node's bound, and a source whose reads do not fit queues
//! none and is placed as with `trees.prepare_prefetch = "none"`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::channel::mpsc::unbounded;
use tokio::sync::{SemaphorePermit, watch};

use super::groups::{Group, groups};
use super::names::SourceId;
use super::prepare::{fill, run};
use super::queued::{admit, held};
use super::sources::Stage;
use super::testing::{AGENT, READ_ONLY, Rig, list_of, source};
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::TreesConfig;
use crate::{Op, TreeDigest, TreeImage};

/// Takes every read of the node, so that no queued read gets its turn: a stalled upstream, as the
/// queue sees it.
fn every_read(rig: &Rig) -> SemaphorePermit<'_> {
    let reads = rig.bench.store.config().fill_concurrency;
    let reads = u32::try_from(reads).expect("a few reads");
    rig.provider
        .shared
        .fills
        .try_acquire_many(reads)
        .expect("every read is free")
}

/// The GETs the upstream has had of the pack.
fn pack_reads(rig: &Rig) -> u64 {
    rig.bench.rig.calls_on(Op::Get, &rig.bench.pack.key)
}

/// The bytes the reads queued on the node hold now.
fn queued_bytes(rig: &Rig) -> u64 {
    rig.provider.shared.queued.bytes()
}

/// The bytes the reads of one source of the pack's list hold once queued: the groups the
/// preparation makes of its walk, each as [`held`] counts it.
fn one_list(rig: &Rig) -> u64 {
    let list = list_of(&rig.bench.pack)
        .encode_v2()
        .expect("a list to encode");
    let limits = rig.bench.store.config().limits();
    let mut spans = Vec::new();
    let image = TreeImage::open(list.into(), &limits, |span| spans.push(span)).expect("a list");
    let reads = groups(image.keys(), spans, image.ranges());
    assert!(!reads.is_empty());
    reads.iter().map(held).sum()
}

/// Prepares the source `run`/0 with `key` and waits for its tree to be placed.
async fn placed(rig: &Rig, run: &str, key: &str) {
    rig.provider
        .prepare(source(run, 0), key, READ_ONLY)
        .expect("prepared");
    rig.provider.mount(source(run, 0)).await.expect("placed");
}

/// Asserts that a source released while every read of the node is taken ends its preparation at
/// once: the reads it queued are dropped, so they hold nothing any more, and none of them is made,
/// even once the reads of the node are free again. Before, the preparation went on until each
/// queued read had had its turn, holding every one of them meanwhile.
#[tokio::test]
async fn a_released_source_drops_its_queued_reads_while_every_read_of_the_node_is_taken() {
    let rig = Rig::new("prov-bound-release").await;
    let taken = every_read(&rig);
    let id = SourceId {
        agent: AGENT.to_owned(),
        run: "js.1".to_owned(),
        index: 0,
    };
    let (job, mut seen) = rig.provider.hold(id, &rig.key);
    let running = tokio::spawn(run(Arc::clone(&rig.provider.shared), job));
    seen.wait_for(|stage| *stage == Stage::Placed)
        .await
        .expect("placed");
    assert_eq!(queued_bytes(&rig), one_list(&rig));

    rig.provider.release(source("js.1", 0)).expect("released");
    until("the released preparation ends", || running.is_finished()).await;
    running.await.expect("the preparation");
    assert_eq!(queued_bytes(&rig), 0);
    drop(taken);
    tokio::task::yield_now().await;
    assert_eq!(pack_reads(&rig), 0);
}

/// Asserts that the reads queued by every source together stay under the node's bound: with room
/// for exactly one source's reads and every read of the node taken, the first source queues its
/// reads, filling the bound to the byte, and the three after it queue none, each counted, while
/// all four are placed; once the first is released its room is free, and the next source queues
/// its reads again.
#[tokio::test]
async fn the_queued_reads_of_every_source_stay_under_the_bound_of_the_node() {
    let probe = Rig::new("prov-bound-probe").await;
    let room = one_list(&probe);
    let rig = Rig::with("prov-bound-sources", TreesConfig::default(), |settings| {
        settings.max_queued_bytes = room;
    })
    .await;
    let _taken = every_read(&rig);
    let reads = Arc::clone(&rig.provider.shared.queued);
    for run in ["js.1", "js.2", "js.3", "js.4"] {
        placed(&rig, run, &rig.key).await;
        assert!(reads.bytes() <= room, "{run}");
    }
    assert_eq!((reads.bytes(), reads.skipped()), (room, 3));

    rig.provider.release(source("js.1", 0)).expect("released");
    until("the first source's reads are dropped", || {
        reads.bytes() == 0
    })
    .await;
    placed(&rig, "js.5", &rig.key).await;
    assert_eq!((reads.bytes(), reads.skipped()), (room, 3));
    assert_eq!(pack_reads(&rig), 0);
    let text = rig.bench.rig.cache.metrics();
    let gauge = format!("\ntierstore_provider_prefetch_queued_bytes {room}\n");
    assert!(text.contains(&gauge), "{text}");
    assert!(
        text.contains("\ntierstore_provider_prefetch_skipped_total 3\n"),
        "{text}"
    );
}

/// Asserts that a version 2 list whose reads do not fit under the bound queues none and is placed
/// as with `trees.prepare_prefetch = "none"`: the preparation ends with the tree placed, not marked
/// ready, no file read, nothing held, and the refusal counted.
#[tokio::test]
async fn a_version_2_list_over_the_bound_is_placed_with_no_read_queued() {
    let rig = Rig::with("prov-bound-v2", TreesConfig::default(), |settings| {
        settings.max_queued_bytes = 0;
    })
    .await;
    let key = rig.put_list_v2(&list_of(&rig.bench.pack)).await;
    let id = SourceId {
        agent: AGENT.to_owned(),
        run: "js.1".to_owned(),
        index: 0,
    };
    let (job, seen) = rig.provider.hold(id, &key);
    run(Arc::clone(&rig.provider.shared), job).await;
    assert_eq!(*seen.borrow(), Stage::Placed);
    let reads = &rig.provider.shared.queued;
    assert_eq!((reads.bytes(), reads.skipped()), (0, 1));
    assert_eq!(pack_reads(&rig), 0);
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).expect("placed");
    let placement = store.placement(&located.placement).expect("a placement");
    assert!(!placement.ready);
}

/// Asserts that a fill whose source is released, with more reads queued than may wait for a turn
/// at once and every read of the node taken, ends at once: the reads waiting for their turn give it
/// up and are counted as not read, the reads still queued are dropped uncounted, the room of all
/// of them is given back, and no read is made.
#[tokio::test]
async fn a_release_ends_the_fill_and_gives_back_the_room_of_every_queued_read() {
    let rig = Rig::new("prov-bound-fill").await;
    let shared = &rig.provider.shared;
    let taken = every_read(&rig);
    let waiting = shared.store.config().fill_concurrency;
    let pack = &rig.bench.pack;
    let (_, offset, size) = &pack.blobs[0];
    let group = |at: u64| Group {
        pack: Arc::from(pack.key.as_str()),
        parts: vec![(offset + at, *size, TreeDigest::of(&at.to_le_bytes()))],
    };
    let groups: Vec<Group> = (0..waiting as u64 + 2).map(group).collect();
    let queued = admit(&shared.queued, groups, u64::MAX).expect("no bound");
    let (sender, reads) = unbounded();
    for read in queued {
        sender.unbounded_send(read).expect("queued");
    }
    let (stage, seen) = watch::channel(Stage::Placed);
    let (released, refused) = (AtomicBool::new(false), AtomicBool::new(false));
    let filling = fill(shared, reads, (&released, seen), &refused);
    let mut filling = std::pin::pin!(filling);
    assert!(futures::poll!(filling.as_mut()).is_pending());
    assert!(shared.queued.bytes() > 0);
    assert!(!sender.is_closed());

    stage.send_replace(Stage::Released);
    assert_eq!(filling.await, (0, waiting));
    assert_eq!(shared.queued.bytes(), 0);
    // The queue was dropped by the release: its sender, still here, finds it closed.
    assert!(sender.is_closed());
    drop(taken);
    assert_eq!(pack_reads(&rig), 0);
}
