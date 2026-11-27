//! The bounds of the provider at and past their limits: the table of sources refuses a new source
//! past `trees.max_sources`, `UNAVAILABLE` and counted, until a release makes room; and no more
//! than `trees.prepare_concurrency` preparations read and make their tree at once, while a release
//! ends a preparation that waits for its turn, or that reads its list, at once.

use std::sync::Arc;

use super::names::SourceId;
use super::prepare::run;
use super::sources::Stage;
use super::testing::{AGENT, READ_ONLY, Rig, sidecar_key_of, source, unavailable};
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::TreesConfig;
use crate::{ListEntry, ListKind, Mode, TreeList};

/// A tree of one empty file, `f<i>`, so a tree other than the rig's for each `i`, with no file to
/// read.
fn tree(i: u32) -> TreeList {
    TreeList {
        entries: vec![ListEntry {
            path: format!("f{i}"),
            kind: ListKind::File {
                mode: Mode::new(0o644).expect("a mode"),
                size: 0,
                source: None,
                sha256: None,
            },
        }],
    }
}

/// The source `run`/0 of the tests' agent.
fn id(run: &str) -> SourceId {
    SourceId {
        agent: AGENT.to_owned(),
        run: run.to_owned(),
        index: 0,
    }
}

/// The sources held, the preparations holding a turn and the refusals, as `/metricz` shows them.
fn counts(rig: &Rig) -> (u64, u64, u64) {
    rig.provider.shared.counts.counts()
}

/// Asserts that the table holds no more sources than `trees.max_sources`: with a bound of two, a
/// third new source is refused `UNAVAILABLE` naming the bound, and counted, while a `Prepare` of
/// a source already held is still a success; once one is released, the third is taken. The
/// gauge and the counter on `/metricz` say so.
#[tokio::test]
async fn a_new_source_past_the_bound_of_the_table_is_unavailable_until_one_is_released() {
    let config = TreesConfig {
        max_sources: 2,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-table-bound", config, |_| {}).await;
    let provider = &rig.provider;
    provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .expect("the first");
    provider
        .prepare(source("js.2", 0), &rig.key, READ_ONLY)
        .expect("the second");
    assert_eq!(counts(&rig).0, 2);

    let over = provider
        .prepare(source("js.3", 0), &rig.key, READ_ONLY)
        .unwrap_err();
    assert_eq!(
        unavailable(over),
        "the node holds 2 sources (trees.max_sources): release one, or retry"
    );
    assert_eq!((counts(&rig).0, counts(&rig).2), (2, 1));
    assert!(provider.mount(source("js.3", 0)).await.is_err());
    provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .expect("a source held is prepared again");
    assert_eq!((counts(&rig).0, counts(&rig).2), (2, 1));

    provider.release(source("js.1", 0)).expect("released");
    assert_eq!(counts(&rig).0, 1);
    provider
        .prepare(source("js.3", 0), &rig.key, READ_ONLY)
        .expect("room again");
    assert_eq!(
        provider.mount(source("js.3", 0)).await,
        Ok(Rig::path_of("js.3", 0))
    );
    let text = rig.bench.rig.cache.metrics();
    assert!(text.contains("\ntierstore_provider_sources 2\n"), "{text}");
    assert!(
        text.contains("\ntierstore_provider_prepare_refused_total 1\n"),
        "{text}"
    );
}

/// Asserts that no more preparations read and make their tree than `trees.prepare_concurrency`:
/// with one turn, a preparation whose sidecar read is held takes it, and a second preparation, of
/// another tree, waits for it unplaced while the gauge says one; once the first source is
/// released, its preparation ends at once though its read is still held, the turn is free, and
/// the second is placed.
#[tokio::test]
async fn the_preparations_that_read_their_list_stay_within_the_turns_of_the_node() {
    let config = TreesConfig {
        prepare_concurrency: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-turns", config, |_| {}).await;
    let shared = &rig.provider.shared;
    let _gate = rig.bench.rig.tamper.hold_key(&sidecar_key_of(&rig.key));
    let (first, _) = rig.provider.hold(id("js.1"), &rig.key);
    let reading = tokio::spawn(run(Arc::clone(shared), first));
    until("the first preparation takes the turn", || {
        counts(&rig).1 == 1
    })
    .await;
    assert_eq!(shared.turns.available_permits(), 0);

    let other = rig.put_list_v2(&tree(1)).await;
    let (second, seen) = rig.provider.hold(id("js.2"), &other);
    let waiting = tokio::spawn(run(Arc::clone(shared), second));
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(*seen.borrow(), Stage::Preparing);
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.2", 0))
            .is_none()
    );
    assert_eq!(counts(&rig).1, 1);
    let text = rig.bench.rig.cache.metrics();
    assert!(
        text.contains("\ntierstore_provider_preparing 1\n"),
        "{text}"
    );

    rig.provider.release(source("js.1", 0)).expect("released");
    until("the released preparation ends", || reading.is_finished()).await;
    reading.await.expect("the first preparation");
    waiting.await.expect("the second preparation");
    assert_eq!(*seen.borrow(), Stage::Placed);
    assert_eq!(shared.turns.available_permits(), 1);
    assert_eq!(counts(&rig).1, 0);
}

/// Asserts that a release of a source whose preparation waits for its turn ends the preparation
/// at once, released, with nothing of its tree read or placed, while the turn stays with the
/// preparation that holds it.
#[tokio::test]
async fn a_release_ends_a_preparation_that_waits_for_its_turn() {
    let config = TreesConfig {
        prepare_concurrency: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-turn-release", config, |_| {}).await;
    let shared = &rig.provider.shared;
    let _gate = rig.bench.rig.tamper.hold_key(&sidecar_key_of(&rig.key));
    let (first, _) = rig.provider.hold(id("js.1"), &rig.key);
    let _reading = tokio::spawn(run(Arc::clone(shared), first));
    until("the first preparation takes the turn", || {
        counts(&rig).1 == 1
    })
    .await;

    let other = rig.put_list_v2(&tree(2)).await;
    let (second, seen) = rig.provider.hold(id("js.2"), &other);
    let waiting = tokio::spawn(run(Arc::clone(shared), second));
    rig.provider.release(source("js.2", 0)).expect("released");
    until("the waiting preparation ends", || waiting.is_finished()).await;
    waiting.await.expect("the second preparation");
    assert_eq!(*seen.borrow(), Stage::Released);
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.2", 0))
            .is_none()
    );
    assert_eq!(counts(&rig).1, 1);
    assert_eq!(shared.turns.available_permits(), 0);
}

/// Asserts that a preparation waiting for its turn when the process stops fails, with the reason,
/// and holds no turn.
#[tokio::test]
async fn a_preparation_waiting_for_its_turn_fails_when_the_process_stops() {
    let config = TreesConfig {
        prepare_concurrency: 1,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-turn-halt", config, |_| {}).await;
    let shared = &rig.provider.shared;
    let taken = shared.turns.try_acquire().expect("the only turn");
    let (job, seen) = rig.provider.hold(id("js.1"), &rig.key);
    let waiting = tokio::spawn(run(Arc::clone(shared), job));
    rig.provider.halt();
    waiting.await.expect("the preparation");
    assert_eq!(
        *seen.borrow(),
        Stage::Failed("the provider is stopping".to_owned())
    );
    assert_eq!(counts(&rig).1, 0);
    drop(taken);
}
