//! Attempts of one run: a late `Release` of an earlier attempt of a job leaves the tree of a later
//! attempt mounted and readable; a `Release` of the attempt itself, or of a source that names no
//! attempt, releases it as before; a source placed without an attempt (by a provider before
//! attempts) is mounted and released by the attempt that asks; and a `Sweep` that keeps a run from
//! an attempt on releases the earlier attempts alone.

use std::path::{Path, PathBuf};

use tierstore_trees_api::provider::v1::KeptAttempt;

use super::Provider;
use super::testing::{
    AGENT, MOUNT, READ_ONLY, Rig, attempt_of, refused, settings, sidecar_key_of, source,
    unavailable,
};
use crate::TreeList;
use crate::server::local_cache::trees::testing::Bench;
use crate::server::local_cache::trees::{PreparePrefetch, Resolved, TreeStore, TreesConfig};

/// The directory the provider answers for the source `index` of the attempt `attempt` of `run`.
fn attempt_path(run: &str, index: u32, attempt: u64) -> PathBuf {
    Path::new(MOUNT).join(Rig::attempt_place_of(run, index, attempt))
}

/// Whether the tree store has a placement at `place` whose file `bin/tool` resolves to the pack's
/// file of 1000 bytes: the tree is there and can be read.
fn readable(store: &TreeStore, place: &str) -> bool {
    store.locate_fs(place).is_some_and(|located| {
        located.path.is_empty()
            && matches!(
                store.resolve(&located.placement, "bin/tool"),
                Ok(Resolved::File { size: 1000, .. })
            )
    })
}

/// A run kept from `attempt` on.
fn from(run: &str, attempt: u64) -> KeptAttempt {
    KeptAttempt {
        run: run.to_owned(),
        attempt,
    }
}

/// Asserts the case of a late release: attempt 1 of the job is prepared and mounted, and its
/// release is still owed when attempt 2 of the same job is prepared and mounted on the same agent.
/// The late release of attempt 1 succeeds, removes attempt 1's placement, and leaves attempt 2's
/// tree placed, readable, mounted at its own path and its run's request stamp kept; asked again,
/// it still removes nothing.
#[tokio::test]
async fn a_late_release_of_an_earlier_attempt_leaves_the_later_attempts_tree() {
    let rig = Rig::new("prov-attempt-late").await;
    let store = &rig.bench.store;
    let first = rig.mounted_attempt("js.1", 0, 1).await;
    let second = rig.mounted_attempt("js.1", 0, 2).await;
    assert_eq!(first, attempt_path("js.1", 0, 1));
    assert_eq!(second, attempt_path("js.1", 0, 2));
    assert_eq!(rig.bench.rig.cache.runs_kept(), 2);
    assert_eq!(rig.provider.release(attempt_of("js.1", 0, 1)), Ok(()));
    assert!(
        store
            .locate_fs(&Rig::attempt_place_of("js.1", 0, 1))
            .is_none()
    );
    assert!(readable(store, &Rig::attempt_place_of("js.1", 0, 2)));
    assert_eq!(rig.bench.rig.cache.runs_kept(), 1);
    assert_eq!(
        rig.provider.mount(attempt_of("js.1", 0, 2)).await,
        Ok(second)
    );
    assert_eq!(rig.provider.release(attempt_of("js.1", 0, 1)), Ok(()));
    assert!(readable(store, &Rig::attempt_place_of("js.1", 0, 2)));
    assert_eq!(store.mount_paths().len(), 1);
}

/// Asserts that a release of an earlier attempt that arrives while the later attempt is still being
/// prepared (its tree not yet placed) leaves the later attempt's preparation going: its `Mount`
/// then answers its directory. A release of an attempt never prepared here, earlier or later than
/// the one that is, is a success that removes nothing.
#[tokio::test]
async fn a_release_of_another_attempt_does_not_stop_a_preparation() {
    let rig = Rig::new("prov-attempt-preparing").await;
    let gate = rig.bench.rig.tamper.hold_key(&sidecar_key_of(&rig.key));
    rig.provider
        .prepare(attempt_of("js.1", 0, 2), &rig.key, READ_ONLY)
        .unwrap();
    assert_eq!(rig.provider.release(attempt_of("js.1", 0, 1)), Ok(()));
    assert_eq!(rig.provider.release(attempt_of("js.1", 0, 3)), Ok(()));
    assert!(rig.bench.store.mount_paths().is_empty());
    gate.add_permits(1);
    let mounted = rig.provider.mount(attempt_of("js.1", 0, 2)).await;
    assert_eq!(mounted, Ok(attempt_path("js.1", 0, 2)));
    assert!(readable(
        &rig.bench.store,
        &Rig::attempt_place_of("js.1", 0, 2)
    ));
}

/// Asserts that a release of the attempt that is placed removes its tree, that a mount after it is
/// not prepared here, and that releasing it again is a success.
#[tokio::test]
async fn a_release_of_the_same_attempt_removes_its_tree() {
    let rig = Rig::new("prov-attempt-same").await;
    rig.mounted_attempt("js.1", 0, 2).await;
    assert_eq!(rig.provider.release(attempt_of("js.1", 0, 2)), Ok(()));
    assert!(rig.bench.store.mount_paths().is_empty());
    let after = rig
        .provider
        .mount(attempt_of("js.1", 0, 2))
        .await
        .unwrap_err();
    assert_eq!(unavailable(after), "the source is not prepared here");
    assert_eq!(rig.provider.release(attempt_of("js.1", 0, 2)), Ok(()));
}

/// Asserts that a release of an attempt the store cannot make (the cache does not hold the lock of
/// its directory) is unavailable, so that the agent asks again: of the attempt's own placement,
/// and of the same source placed without an attempt. The trees are prepared with
/// `trees.prepare_prefetch = "none"`, so that no read of their files is still going on, or has
/// marked a tree ready, when the store is read again without the lock: a ready tree whose bytes
/// that store's cache does not yet see would be dropped, and its placement with it.
#[tokio::test]
async fn a_release_of_an_attempt_the_store_cannot_make_is_unavailable() {
    let lazy = TreesConfig {
        prepare_prefetch: PreparePrefetch::None,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-attempt-ro", lazy, |_| {}).await;
    rig.mounted_attempt("js.1", 0, 2).await;
    rig.mounted("js.2", 0).await;
    let holder = rig.bench.rig.locked();
    let waiting = rig.bench.rig.locked();
    let provider = Provider::with(Bench::reopen(waiting.clone()), waiting, settings());
    let why = "the placement was not released: this node does not hold the lock of its cache \
               directory";
    for source in [attempt_of("js.1", 0, 2), attempt_of("js.2", 0, 5)] {
        let answer = provider.release(source).unwrap_err();
        assert_eq!(unavailable(answer), why);
    }
    drop(holder);
}

/// Asserts that a source that names no attempt, as an agent that does not count attempts sends it,
/// is placed at the path every release before attempts placed it at, is answered there, and is
/// released by a release that names no attempt; and that such a release leaves a source of the
/// same run that names its attempt as it is (an agent sends one or the other, never both).
#[tokio::test]
async fn a_source_that_names_no_attempt_is_released_as_before() {
    let rig = Rig::new("prov-attempt-none").await;
    let store = &rig.bench.store;
    let path = rig.mounted("js.1", 0).await;
    assert_eq!(path, Rig::path_of("js.1", 0));
    assert!(readable(store, &Rig::place_of("js.1", 0)));
    rig.mounted_attempt("js.1", 0, 4).await;
    assert_eq!(rig.provider.release(source("js.1", 0)), Ok(()));
    assert!(store.locate_fs(&Rig::place_of("js.1", 0)).is_none());
    assert!(readable(store, &Rig::attempt_place_of("js.1", 0, 4)));
    let after = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(unavailable(after), "the source is not prepared here");
}

/// Asserts the upgrade from a provider before attempts: a source it placed without an attempt is
/// found by a provider started over the same store, which answers its directory to a `Mount` of
/// any attempt of that source that it did not prepare itself, and removes it at a `Release` of
/// that attempt; an attempt this provider prepared is answered at its own path instead.
#[tokio::test]
async fn an_attempt_finds_the_same_source_placed_without_one() {
    let rig = Rig::new("prov-attempt-upgrade").await;
    let store = &rig.bench.store;
    rig.mounted("js.1", 0).await;
    rig.mounted("js.2", 0).await;
    let restarted = Provider::with(store.clone(), rig.bench.rig.cache.clone(), settings());
    assert_eq!(
        restarted.mount(attempt_of("js.1", 0, 3)).await,
        Ok(Rig::path_of("js.1", 0))
    );
    let never = restarted.mount(attempt_of("js.9", 0, 3)).await.unwrap_err();
    assert_eq!(unavailable(never), "the source is not prepared here");
    assert_eq!(restarted.release(attempt_of("js.1", 0, 3)), Ok(()));
    assert!(store.locate_fs(&Rig::place_of("js.1", 0)).is_none());
    restarted
        .prepare(attempt_of("js.2", 0, 1), &rig.key, READ_ONLY)
        .unwrap();
    assert_eq!(
        restarted.mount(attempt_of("js.2", 0, 1)).await,
        Ok(attempt_path("js.2", 0, 1))
    );
    assert!(readable(store, &Rig::place_of("js.2", 0)));
    assert!(readable(store, &Rig::attempt_place_of("js.2", 0, 1)));
}

/// Asserts that a prepare of an attempt with another ref than an earlier attempt of the same
/// source is not refused (the attempts are two sources), while a prepare of the same attempt with
/// another ref still is.
#[tokio::test]
async fn each_attempt_is_prepared_with_its_own_ref() {
    let rig = Rig::new("prov-attempt-ref").await;
    let empty = rig.put_list(&TreeList::default()).await;
    rig.mounted_attempt("js.1", 0, 1).await;
    assert_eq!(
        rig.provider
            .prepare(attempt_of("js.1", 0, 2), &empty, READ_ONLY),
        Ok(())
    );
    let refusal = rig
        .provider
        .prepare(attempt_of("js.1", 0, 1), &empty, READ_ONLY)
        .unwrap_err();
    assert_eq!(
        refused(refusal),
        format!("the source was prepared with another ref, {}", rig.key)
    );
}

/// Asserts a sweep over attempts of several runs: a run kept from an attempt on loses its earlier
/// attempts and keeps that attempt, later ones and the source that names no attempt; a run named
/// only in `keep` keeps every attempt; an attempt of 0 in `keep_attempts` keeps every attempt; of
/// several attempts named for one run the earliest counts; a run named only in `keep_attempts` is
/// kept; and a run named nowhere loses every attempt. A provider started over the same store does
/// the same with the placements it finds by their paths.
#[tokio::test]
async fn a_sweep_keeps_a_run_from_the_attempt_it_names() {
    let rig = Rig::new("prov-attempt-sweep").await;
    let store = &rig.bench.store;
    let sources: [(&str, u64); 9] = [
        ("js.1", 1),
        ("js.1", 2),
        ("js.1", 3),
        ("js.1", 0),
        ("js.2", 1),
        ("js.3", 1),
        ("js.4", 1),
        ("js.4", 3),
        ("js.5", 2),
    ];
    for (run, attempt) in sources {
        rig.mounted_attempt(run, 0, attempt).await;
    }
    let place = |run: &str, attempt: u64| match attempt {
        0 => Rig::place_of(run, 0),
        attempt => Rig::attempt_place_of(run, 0, attempt),
    };
    let keep = ["js.1".to_owned(), "js.2".to_owned(), "js.5".to_owned()];
    let kept_from = [
        from("js.1", 2),
        from("js.2", 0),
        from("js.4", 5),
        from("js.4", 2),
    ];
    assert_eq!(rig.provider.sweep(AGENT, &keep, &kept_from), Ok(3));
    for (run, attempt) in [("js.1", 1), ("js.3", 1), ("js.4", 1)] {
        assert!(
            store.locate_fs(&place(run, attempt)).is_none(),
            "{run} {attempt}"
        );
    }
    for (run, attempt) in [
        ("js.1", 2),
        ("js.1", 3),
        ("js.1", 0),
        ("js.2", 1),
        ("js.4", 3),
    ] {
        assert!(readable(store, &place(run, attempt)), "{run} {attempt}");
    }
    assert_eq!(store.mount_paths().len(), 6);
    let restarted = Provider::with(store.clone(), rig.bench.rig.cache.clone(), settings());
    let keep = ["js.1".to_owned(), "js.5".to_owned()];
    assert_eq!(restarted.sweep(AGENT, &keep, &[from("js.1", 3)]), Ok(3));
    let mut left = store.mount_paths();
    left.sort();
    let mut expected = vec![place("js.1", 3), place("js.1", 0), place("js.5", 2)];
    expected.sort();
    assert_eq!(left, expected);
}
