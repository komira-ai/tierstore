//! `trees.prepare_prefetch`: with the default, `background`, a preparation reads every file of
//! its tree with no open; with `none`, it places the tree and reads no file, an open fills exactly
//! the file it opens, the tree is neither pinned nor marked ready, and a release or a sweep removes
//! the placement as before. The line the log has at the start of a provider names the mode.

use std::sync::Arc;

use super::names::SourceId;
use super::prepare::run;
use super::sources::{Stage, mode_line};
use super::testing::{AGENT, MOUNT, Rig, source};
use super::{DEFAULT_FETCH_TIMEOUT, Provider, ProviderFlags};
use crate::Op;
use crate::server::local_cache::trees::{PlacementId, PreparePrefetch, TreesConfig};

/// A rig whose tree store's `prepare_prefetch` is `none`.
async fn lazy(name: &str) -> Rig {
    let config = TreesConfig {
        prepare_prefetch: PreparePrefetch::None,
        ..TreesConfig::default()
    };
    Rig::with(name, config, |_| {}).await
}

/// The GETs the upstream has had of the pack.
fn pack_reads(rig: &Rig) -> u64 {
    rig.bench.rig.calls_on(Op::Get, &rig.bench.pack.key)
}

/// Which of the pack's three blobs the cache holds.
fn held(rig: &Rig) -> [bool; 3] {
    [rig.holds(0), rig.holds(1), rig.holds(2)]
}

/// Runs the preparation of the source `run`/0 with `key` to its end; the stage it leaves.
async fn prepared(rig: &Rig, run_name: &str, key: &str) -> Stage {
    let id = SourceId {
        agent: AGENT.to_owned(),
        run: run_name.to_owned(),
        index: 0,
    };
    let (job, seen) = rig.provider.hold(id, key);
    run(Arc::clone(&rig.provider.shared), job).await;
    seen.borrow().clone()
}

/// The placement of the source `run`/0.
fn placement(rig: &Rig, run_name: &str) -> PlacementId {
    let located = rig.bench.store.locate_fs(&Rig::place_of(run_name, 0));
    located.expect("the source is placed").placement
}

/// Asserts that the default mode is `background`, and that with it a preparation run to its end
/// has read every file of the tree into the cache with no open, and marked the tree ready.
#[tokio::test]
async fn by_default_a_preparation_reads_every_file_with_no_open() {
    assert_eq!(
        TreesConfig::default().prepare_prefetch,
        PreparePrefetch::Background
    );
    let rig = Rig::new("prov-lazy-default").await;
    assert_eq!(held(&rig), [false; 3]);
    assert_eq!(prepared(&rig, "js.1", &rig.key).await, Stage::Placed);
    assert_eq!(held(&rig), [true; 3]);
    // The three files are one range of the list: one read of the pack.
    assert_eq!(pack_reads(&rig), 1);
    let id = placement(&rig, "js.1");
    assert!(rig.bench.store.placement(&id).unwrap().ready);
}

/// Asserts that with `none` a preparation places the tree and reads no file (no GET of the pack,
/// nothing in the cache), though its spans are otherwise queued, grouped by its pack ranges, as
/// soon as the list is walked; leaves the tree not marked ready; and that each open then fills
/// exactly the file it opens, once.
#[tokio::test]
async fn with_none_a_tree_is_placed_with_no_file_read_and_an_open_fills_its_file_alone() {
    let rig = lazy("prov-lazy-v2").await;
    open_fills_one_file_at_a_time(&rig, &rig.key).await;
}

/// Prepares the source `js.1`/0 of `rig` (whose mode is `none`) with `key`, the list of the pack's
/// tree, and asserts what the test above says.
async fn open_fills_one_file_at_a_time(rig: &Rig, key: &str) {
    assert_eq!(prepared(rig, "js.1", key).await, Stage::Placed);
    assert_eq!((held(rig), pack_reads(rig)), ([false; 3], 0));
    let id = placement(rig, "js.1");
    let store = &rig.bench.store;
    assert!(!store.placement(&id).unwrap().ready);
    // `bin/tool` is the pack's blob 0, `lib/deep/data` its blob 2 (`list_of`).
    let opened = store.open_file(&id, "bin/tool").await.unwrap();
    assert!(opened.span.is_some());
    assert_eq!((held(rig), pack_reads(rig)), ([true, false, false], 1));
    store.open_file(&id, "bin/tool").await.unwrap();
    assert_eq!(pack_reads(rig), 1);
    store.open_file(&id, "lib/deep/data").await.unwrap();
    assert_eq!((held(rig), pack_reads(rig)), ([true, false, true], 2));
    // An empty file has no bytes to read.
    let empty = store.open_file(&id, "zero").await.unwrap();
    assert!(empty.span.is_none());
    assert_eq!(pack_reads(rig), 2);
    assert!(!store.placement(&id).unwrap().ready);
}

/// Asserts that with `none` the calls of the contract behave as with the default: a `Mount`
/// answers the source's directory once the tree is placed, a `Release` removes the placement, a
/// `Sweep` removes the placement of a run not kept, and no file is read throughout.
#[tokio::test]
async fn with_none_mount_release_and_sweep_behave_as_before() {
    let rig = lazy("prov-lazy-calls").await;
    let store = &rig.bench.store;
    assert_eq!(rig.mounted("js.1", 0).await, Rig::path_of("js.1", 0));
    assert!(store.locate_fs(&Rig::place_of("js.1", 0)).is_some());
    rig.provider.release(source("js.1", 0)).unwrap();
    assert!(store.locate_fs(&Rig::place_of("js.1", 0)).is_none());
    assert_eq!(rig.mounted("js.2", 0).await, Rig::path_of("js.2", 0));
    assert_eq!(rig.provider.sweep(AGENT, &[], &[]), Ok(1));
    assert!(store.locate_fs(&Rig::place_of("js.2", 0)).is_none());
    assert!(store.mount_paths().is_empty());
    assert_eq!((held(&rig), pack_reads(&rig)), ([false; 3], 0));
}

/// Asserts that the line the log has at the start of a provider names the key and its mode, for
/// each mode, and that a provider over a store whose mode is `none` is made.
#[tokio::test]
async fn the_start_line_states_the_mode() {
    assert_eq!(
        mode_line(PreparePrefetch::Background),
        "tierstore: mount provider: trees.prepare_prefetch = \"background\": Prepare reads every file of a tree in the background"
    );
    assert_eq!(
        mode_line(PreparePrefetch::None),
        "tierstore: mount provider: trees.prepare_prefetch = \"none\": Prepare reads no file: each is read when first opened"
    );
    let rig = lazy("prov-lazy-start").await;
    let flags = ProviderFlags {
        socket: rig.bench.rig.dir.path().join("provider.sock"),
        mountpoint: MOUNT.into(),
        fetch_timeout: DEFAULT_FETCH_TIMEOUT,
    };
    let provider = Provider::new(rig.bench.store.clone(), rig.bench.rig.cache.clone(), &flags);
    assert_eq!(
        provider.shared.store.config().prepare_prefetch,
        PreparePrefetch::None
    );
}
