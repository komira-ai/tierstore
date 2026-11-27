//! The provider's answers `UNAVAILABLE`, each named and counted under its reason in
//! `tierstore_provider_unavailable_total{reason}`: a provider whose mount no process serves (a
//! file with no `fs` interface) answers no directory at all, and every other `UNAVAILABLE` of
//! `Mount`, `Release` and `Sweep` counts once under the reason it says.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::sources::no_fs;
use super::testing::{AGENT, MOUNT, READ_ONLY, Rig, settings, sidecar_key_of, source, unavailable};
use super::{Provider, ProviderError};
use crate::Op;
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::TreesConfig;
use crate::server::local_cache::trees::testing::Bench;

/// The reasons of the family, in its order.
const REASONS: [&str; 7] = [
    "no_fs",
    "not_ready",
    "not_prepared_here",
    "tree_gone",
    "read_failed",
    "store_failed",
    "not_released",
];

/// The lines of `tierstore_provider_unavailable_total` on the `/metricz` of `cache`.
fn counted(cache: &LocalCache) -> Vec<String> {
    cache
        .metrics()
        .lines()
        .filter(|line| line.starts_with("tierstore_provider_unavailable_total{"))
        .map(str::to_owned)
        .collect()
}

/// The lines of the family with `reason` at `count` and every other reason at zero.
fn only(reason: &str, count: u64) -> Vec<String> {
    REASONS
        .iter()
        .map(|each| {
            let n = if *each == reason { count } else { 0 };
            format!("tierstore_provider_unavailable_total{{reason=\"{each}\"}} {n}")
        })
        .collect()
}

/// Asserts that a provider whose mount no process serves (the node's file has no `fs` interface,
/// as a Mac of the two-process layout has on a release with no `tierstore fs` process) answers
/// `Prepare` and `Mount` `UNAVAILABLE` with the reason that says so, word for word; that it reads
/// nothing of the tree and places nothing, so no directory under the mount is ever answered; that
/// each answer counts once under `no_fs`; that a request that is wrong in itself is still
/// `INVALID_ARGUMENT`, before the mount is looked at; and that `Release` and `Sweep` still answer,
/// so that an agent's clean-up is not held. Catches the provider that answered such a node's
/// `Mount` with a directory under a mount nothing serves, where the agent's look at it failed the
/// start `input-unavailable` on every try.
#[tokio::test]
async fn a_provider_whose_mount_nothing_serves_answers_no_directory() {
    let rig = Rig::with("prov-no-fs", TreesConfig::default(), |settings| {
        settings.served = false;
    })
    .await;
    let why = format!(
        "no file system serves {MOUNT}: the node's file has no [[interface]] with type = \"fs\", \
         and tierstore fs is no longer a process of its own"
    );
    assert_eq!(no_fs(MOUNT.as_ref()), why);
    let prepared = rig.provider.prepare(source("js.1", 0), &rig.key, READ_ONLY);
    assert_eq!(prepared, Err(ProviderError::Unavailable(why.clone())));
    let mounted = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(mounted, Err(ProviderError::Unavailable(why)));
    assert_eq!(counted(&rig.bench.rig.cache), only("no_fs", 2));
    assert_eq!(
        rig.provider
            .prepare(source("js.1", 0), "cas/l/nope", READ_ONLY),
        Err(ProviderError::Invalid(
            "the ref is not the key of a tree list: cas/l/ and 64 lowercase hex digits".into()
        ))
    );
    assert_eq!(
        rig.provider.mount(None).await,
        Err(ProviderError::Invalid("no source".into()))
    );
    assert_eq!(counted(&rig.bench.rig.cache), only("no_fs", 2));
    let sidecar = sidecar_key_of(&rig.key);
    assert_eq!(rig.bench.rig.calls_on(Op::Get, &sidecar), 0);
    assert_eq!(rig.bench.rig.calls_on(Op::Head, &sidecar), 0);
    assert!(rig.bench.store.mount_paths().is_empty());
    assert_eq!(rig.provider.release(source("js.1", 0)), Ok(()));
    assert_eq!(rig.provider.sweep(AGENT, &[], &[]), Ok(0));
    assert_eq!(counted(&rig.bench.rig.cache), only("no_fs", 2));
}

/// Asserts that a served provider counts nothing for a source it prepares, places and mounts, and
/// shows every reason at zero.
#[tokio::test]
async fn a_mounted_source_counts_no_unavailable_answer() {
    let rig = Rig::new("prov-unavail-none").await;
    assert_eq!(rig.mounted("js.1", 0).await, Rig::path_of("js.1", 0));
    assert_eq!(counted(&rig.bench.rig.cache), only("", 0));
}

/// Asserts that a `Mount` past its wait counts once under `not_ready`.
#[tokio::test]
async fn a_mount_past_its_wait_counts_not_ready() {
    let rig = Rig::with("prov-unavail-ready", TreesConfig::default(), |settings| {
        settings.mount_wait = Duration::ZERO;
    })
    .await;
    let _gate = rig.bench.rig.tamper.hold_key(&sidecar_key_of(&rig.key));
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let answer = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(unavailable(answer), "not ready");
    assert_eq!(counted(&rig.bench.rig.cache), only("not_ready", 1));
}

/// Asserts that a `Mount` of a source this process never prepared and finds no placement of (it
/// started after the agent's `Prepare`) counts once under `not_prepared_here`.
#[tokio::test]
async fn a_mount_of_a_source_not_prepared_here_counts_so() {
    let bench = Bench::new("prov-unavail-here").await;
    let provider = Provider::with(bench.store.clone(), bench.rig.cache.clone(), settings());
    let answer = provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(unavailable(answer), "the source is not prepared here");
    assert_eq!(counted(&bench.rig.cache), only("not_prepared_here", 1));
}

/// Asserts that a `Mount` of a source whose placement's lease ended counts once under
/// `tree_gone`, and the `Mount` after it, which answers the directory, counts nothing.
#[tokio::test]
async fn a_mount_of_a_placement_whose_lease_ended_counts_tree_gone() {
    let rig = Rig::new("prov-unavail-gone").await;
    rig.mounted("js.1", 0).await;
    rig.bench.rig.clock.advance(2 * 3600 * 1000);
    let gone = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(
        unavailable(gone),
        "the tree is gone: it is being prepared again"
    );
    let again = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(again, Ok(Rig::path_of("js.1", 0)));
    assert_eq!(counted(&rig.bench.rig.cache), only("tree_gone", 1));
}

/// Asserts that a `Mount` whose preparation failed to read the tree's sidecar (the upstream failed
/// the read once) counts once under `read_failed`, and the `Mount` that begins it again and
/// answers the directory counts nothing.
#[tokio::test]
async fn a_mount_whose_sidecar_was_not_read_counts_read_failed() {
    let rig = Rig::new("prov-unavail-read").await;
    rig.bench.rig.tamper.fail_once.store(true, Ordering::SeqCst);
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let first = unavailable(rig.provider.mount(source("js.1", 0)).await.unwrap_err());
    let not_read = format!("the L2 sidecar {} was not read: ", sidecar_key_of(&rig.key));
    assert!(first.starts_with(&not_read), "{first}");
    let again = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(again, Ok(Rig::path_of("js.1", 0)));
    assert_eq!(counted(&rig.bench.rig.cache), only("read_failed", 1));
}

/// Asserts that a `Mount` whose tree the store cannot make now (as many placements as it may)
/// counts under `store_failed`, once for each such answer, and not as a failed read; and that the
/// next `Mount`, which begins the preparation again, meets the same.
#[tokio::test]
async fn a_mount_of_a_tree_the_store_cannot_make_counts_store_failed() {
    let config = TreesConfig {
        max_placements: 0,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-unavail-store", config, |_| {}).await;
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    for _ in 0..2 {
        let answer = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
        assert_eq!(
            unavailable(answer),
            "the tree was not made: too many placements: 0 are alive"
        );
    }
    assert_eq!(counted(&rig.bench.rig.cache), only("store_failed", 2));
}

/// Asserts that a `Release` and a `Sweep` the store cannot make (the cache does not hold the lock
/// of its directory) count under `not_released`, once each.
#[tokio::test]
async fn a_release_and_a_sweep_the_store_cannot_make_count_not_released() {
    let rig = Rig::new("prov-unavail-release").await;
    rig.mounted("js.1", 0).await;
    let holder = rig.bench.rig.locked();
    let waiting = rig.bench.rig.locked();
    let provider = Provider::with(Bench::reopen(waiting.clone()), waiting.clone(), settings());
    assert!(provider.release(source("js.1", 0)).is_err());
    assert!(provider.sweep(AGENT, &[], &[]).is_err());
    assert_eq!(counted(&waiting), only("not_released", 2));
    drop(holder);
}
