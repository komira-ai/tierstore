//! The four calls of the provider over a real tree store and cache: a source prepared, placed at
//! its path and filled; an open that joins the read of its file; the waits and refusals of
//! `Mount`; the idempotence of `Prepare` and `Release`; a sweep; and a restart of the process,
//! after which the placements are still found by their paths.

use std::time::Duration;

use tierstore_trees_api::provider::v1::{MountMode, Source};

use super::testing::{
    AGENT, READ_ONLY, Rig, member, refused, settings, sidecar_key_of, source, unavailable,
};
use super::{Provider, ProviderError};
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::testing::{Bench, fs_at};
use crate::server::local_cache::trees::{FileSpan, Resolved, TreesConfig};
use crate::{BlobSource, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_key};

/// The calls the upstream has had on `key`: GETs.
fn gets(rig: &Rig, key: &str) -> u64 {
    rig.bench.rig.calls_on(Op::Get, key)
}

/// The key of the L2 sidecar of the rig's tree, the one object of the tree a preparation reads.
fn sidecar(rig: &Rig) -> String {
    sidecar_key_of(&rig.key)
}

/// Asserts that a prepared source is placed at its path under the mount with exactly the list's
/// tree, that `Mount` answers that directory, that every file of the tree is read into the cache
/// (the three files are one range of the list: one read of the pack) and the tree then marked
/// ready, and that the tree's sidecar is read once and its list never.
#[tokio::test]
async fn a_prepared_source_is_placed_at_its_path_and_its_files_are_read() {
    let rig = Rig::new("prov-place").await;
    let path = rig.mounted("js.1", 0).await;
    assert_eq!(path, Rig::path_of("js.1", 0));
    let store = &rig.bench.store;
    let located = store.locate_fs(&Rig::place_of("js.1", 0)).expect("placed");
    assert_eq!(located.path, "");
    let tool = store.resolve(&located.placement, "bin/tool").unwrap();
    assert!(
        matches!(tool, Resolved::File { mode, size: 1000, .. } if mode == Mode::new(0o755).unwrap())
    );
    let empty = store.resolve(&located.placement, "empty").unwrap();
    assert!(
        matches!(empty, Resolved::Dir { mode, entries: 0 } if mode == Mode::new(0o1777).unwrap())
    );
    rig.filled().await;
    until("the tree is marked ready", || {
        store
            .placement(&located.placement)
            .is_some_and(|placement| placement.ready)
    })
    .await;
    assert_eq!(gets(&rig, &rig.bench.pack.key), 1);
    assert_eq!(gets(&rig, &sidecar(&rig)), 1);
    assert_eq!(gets(&rig, &rig.key), 0);
}

/// Asserts that an open of a file whose bytes the preparation is reading (the span request the
/// mount makes for it) joins that read: the cache counts a join, and the upstream is read once for
/// the file, not once more for the open. The tree is of that one file, so its read is of its span
/// alone, the read an open makes.
#[tokio::test]
async fn an_open_of_a_file_being_read_joins_that_read() {
    let rig = Rig::new("prov-join").await;
    let pack = rig.bench.pack.key.clone();
    let list = TreeList {
        entries: vec![member(&rig.bench.pack, "bin/tool", 0, 0o755)],
    };
    let key = rig.put_list_v2(&list).await;
    let gate = rig.bench.rig.tamper.hold_key(&pack);
    rig.provider
        .prepare(source("js.1", 0), &key, READ_ONLY)
        .unwrap();
    rig.provider.mount(source("js.1", 0)).await.unwrap();
    until("the read is at the upstream", || gets(&rig, &pack) == 1).await;
    let store = rig.bench.store.clone();
    let (sha, offset, size) = rig.bench.pack.blobs[0].clone();
    let span = FileSpan {
        pack: pack.clone(),
        offset,
    };
    let open = tokio::spawn(async move {
        let sha = TreeDigest::from_hex(&sha).unwrap();
        store.fetch_span(&span, size, &sha).await.map(|_| ())
    });
    until("the open joined the read", || {
        rig.bench
            .rig
            .cache
            .metrics()
            .contains("tierstore_cache_extent_joins_total 1\n")
    })
    .await;
    gate.add_permits(1);
    assert_eq!(open.await.unwrap(), Ok(()));
    until("the file was read", || rig.holds(0)).await;
    assert_eq!(gets(&rig, &pack), 1);
}

/// Asserts that a `Mount` asked while the tree list is being read waits, and answers the directory
/// once the tree is placed.
#[tokio::test]
async fn mount_waits_for_the_tree_and_answers_its_directory() {
    let rig = Rig::new("prov-wait").await;
    let gate = rig.bench.rig.tamper.hold_key(&sidecar(&rig));
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let provider = rig.provider.clone();
    let mount = tokio::spawn(async move { provider.mount(source("js.1", 0)).await });
    until("the sidecar is being read", || {
        gets(&rig, &sidecar(&rig)) == 1
    })
    .await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!mount.is_finished());
    gate.add_permits(1);
    assert_eq!(mount.await.unwrap(), Ok(Rig::path_of("js.1", 0)));
}

/// Asserts that a `Mount` whose tree is not placed within its wait is "not ready", unavailable, so
/// that the agent asks again.
#[tokio::test]
async fn mount_is_not_ready_past_its_wait() {
    let rig = Rig::with("prov-not-ready", TreesConfig::default(), |settings| {
        settings.mount_wait = Duration::ZERO;
    })
    .await;
    let _gate = rig.bench.rig.tamper.hold_key(&sidecar(&rig));
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let answer = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(answer, Err(ProviderError::Unavailable("not ready".into())));
}

/// Prepares the source `index` of `run` with `key` and mounts it: the refusal.
async fn refusal_of(rig: &Rig, run: &str, key: &str) -> String {
    rig.provider
        .prepare(source(run, 0), key, READ_ONLY)
        .unwrap();
    refused(rig.provider.mount(source(run, 0)).await.unwrap_err())
}

/// A tree list of empty files named `paths`, in that order.
fn empty_files(paths: &[&str]) -> TreeList {
    TreeList {
        entries: paths
            .iter()
            .map(|path| ListEntry {
                path: (*path).to_owned(),
                kind: ListKind::File {
                    mode: Mode::FILE,
                    size: 0,
                    source: None,
                    sha256: None,
                },
            })
            .collect(),
    }
}

/// Asserts that a ref the provider cannot use is refused at `Mount` with the reason, each by name:
/// a tree with no sidecar whose list is not there either, whose list (version 1 here, read for
/// want of a sidecar) has a malformed line or a file without its digest or is a tree the builder
/// refuses; a sidecar over the bound, and a tree over the store's limit. Each tree with no sidecar
/// counts once as a missing sidecar, none as a refusal of `missing_sidecar` or as a tree placed
/// from its list.
#[tokio::test]
async fn a_ref_the_provider_cannot_use_is_refused_with_its_reason() {
    let config = TreesConfig {
        max_bytes: 5000,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-refused", config, |settings| {
        settings.max_list_bytes = 2000;
    })
    .await;
    let missing = tree_list_key(b"never written");
    assert_eq!(
        refusal_of(&rig, "js.missing", &missing).await,
        format!("no tree list at {missing}")
    );

    let malformed = b"{\"format\":\"tierstore.tree-list\",\"version\":1}\n{}\n";
    let key = rig.put_bytes(malformed).await;
    assert!(
        refusal_of(&rig, "js.malformed", &key)
            .await
            .starts_with("tree list line 2: malformed: ")
    );

    let unhashed = TreeList {
        entries: vec![ListEntry {
            path: "a".into(),
            kind: ListKind::File {
                mode: Mode::FILE,
                size: 5,
                source: Some(BlobSource {
                    key: "k".into(),
                    offset: 0,
                }),
                sha256: None,
            },
        }],
    };
    let key = rig.put_list(&unhashed).await;
    assert_eq!(
        refusal_of(&rig, "js.unhashed", &key).await,
        "tree list line 2: file without sha256"
    );

    let key = rig.put_list(&empty_files(&["a", "a/b"])).await;
    assert_eq!(
        refusal_of(&rig, "js.below", &key).await,
        r#"the tree is refused: file and directory at one path "a/b""#
    );

    let names: Vec<String> = (0..40).map(|n| format!("file-{n:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let key = rig.put_list_v2(&empty_files(&names)).await;
    assert_eq!(
        refusal_of(&rig, "js.long", &key).await,
        format!("the L2 sidecar {} is over 2000 bytes", sidecar_key_of(&key))
    );

    assert_eq!(
        refusal_of(&rig, "js.large", &rig.key.clone()).await,
        "the tree is refused: tree too large: 6000 bytes of files, at most 5000"
    );
    assert!(rig.bench.store.mount_paths().is_empty());
    let metricz = rig.bench.rig.cache.metrics();
    for line in [
        "tierstore_l2_sidecar_missing_total 4\n",
        "tierstore_l2_sidecar_fallback_total 0\n",
        "tierstore_list_refused_total{reason=\"missing_sidecar\"} 0\n",
    ] {
        assert!(metricz.contains(line), "{line} in {metricz}");
    }
}

/// Asserts that a tree with no sidecar whose list is over the bound of a list is refused, naming
/// the bound, and is not placed.
#[tokio::test]
async fn a_list_read_for_want_of_a_sidecar_over_the_bound_is_refused() {
    let rig = Rig::with("prov-list-over", TreesConfig::default(), |settings| {
        settings.max_list_bytes = 2000;
    })
    .await;
    let names: Vec<String> = (0..40).map(|n| format!("file-{n:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let key = rig.put_list(&empty_files(&names)).await;
    assert_eq!(
        refusal_of(&rig, "js.long", &key).await,
        "the tree list is over 2000 bytes"
    );
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a preparation that failed for a reason that may pass (the upstream failed the read
/// of the list) is unavailable to `Mount`, and that the next `Mount` begins it again and answers
/// the directory.
#[tokio::test]
async fn a_failed_preparation_is_begun_again_by_the_next_mount() {
    let rig = Rig::new("prov-again").await;
    rig.bench
        .rig
        .tamper
        .fail_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let first = unavailable(rig.provider.mount(source("js.1", 0)).await.unwrap_err());
    let not_read = format!("the L2 sidecar {} was not read: ", sidecar(&rig));
    assert!(first.starts_with(&not_read), "{first}");
    let again = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(again, Ok(Rig::path_of("js.1", 0)));
}

/// Asserts that a `Prepare` of a source whose preparation failed begins it again too.
#[tokio::test]
async fn a_failed_preparation_is_begun_again_by_the_next_prepare() {
    let rig = Rig::new("prov-again-prepare").await;
    rig.bench
        .rig
        .tamper
        .fail_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    assert!(rig.provider.mount(source("js.1", 0)).await.is_err());
    // The stage is failed: this prepare begins it again, and the mount waits for it.
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    until("the sidecar was read again", || {
        gets(&rig, &sidecar(&rig)) == 2
    })
    .await;
    let again = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(again, Ok(Rig::path_of("js.1", 0)));
}

/// Asserts that a tree the node cannot make now (it has as many placements as it may) is
/// unavailable, not refused: it is the host's, not the ref's.
#[tokio::test]
async fn a_tree_the_node_cannot_make_now_is_unavailable() {
    let config = TreesConfig {
        max_placements: 0,
        ..TreesConfig::default()
    };
    let rig = Rig::with("prov-too-many", config, |_| {}).await;
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let answer = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(
        unavailable(answer),
        "the tree was not made: too many placements: 0 are alive"
    );
}

/// Asserts that a tree's sidecar not read within the list's bound is unavailable, naming it and the
/// bound.
#[tokio::test]
async fn a_list_not_read_within_its_bound_is_unavailable() {
    let rig = Rig::with("prov-list-bound", TreesConfig::default(), |settings| {
        settings.list_timeout = Duration::ZERO;
    })
    .await;
    let _gate = rig.bench.rig.tamper.hold_key(&sidecar(&rig));
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let answer = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(
        unavailable(answer),
        format!("the L2 sidecar {} was not read within 0ns", sidecar(&rig))
    );
}

/// The answer `INVALID_ARGUMENT` with `why`.
fn invalid<T>(why: &str) -> Result<T, ProviderError> {
    Err(ProviderError::Invalid(why.into()))
}

/// Asserts that `Prepare` refuses, by name, a source that is absent or whose agent or run is empty
/// or too long, a ref that is not the key of a tree list, and every mode but read-only; that the
/// other calls refuse a source or an agent the same way; and that an unspecified mode is taken as
/// read-only.
#[tokio::test]
async fn bad_sources_refs_and_modes_are_refused_by_name() {
    let rig = Rig::new("prov-bad").await;
    let provider = &rig.provider;
    assert_eq!(
        provider.prepare(None, &rig.key, READ_ONLY),
        invalid("no source")
    );
    let with = |agent: &str, run: &str| {
        Some(Source {
            agent: agent.into(),
            run: run.into(),
            index: 0,
            attempt: 0,
        })
    };
    assert_eq!(
        provider.prepare(with("", "js.1"), &rig.key, READ_ONLY),
        invalid("the agent is 1 to 120 bytes")
    );
    let long = "r".repeat(121);
    assert_eq!(
        provider.prepare(with(AGENT, &long), &rig.key, READ_ONLY),
        invalid("the run is 1 to 120 bytes")
    );
    let longest = "r".repeat(120);
    assert_eq!(
        provider.prepare(with(AGENT, &longest), &rig.key, READ_ONLY),
        Ok(())
    );
    let not_a_list = rig.key.replacen("cas/l/", "cas/t/", 1);
    assert_eq!(
        provider.prepare(source("js.1", 0), &not_a_list, READ_ONLY),
        invalid("the ref is not the key of a tree list: cas/l/ and 64 lowercase hex digits")
    );
    let read_only = Err(ProviderError::Refused(
        "tierstore makes read-only trees: the mode must be READ_ONLY".into(),
    ));
    for mode in [
        MountMode::ReadWrite as i32,
        MountMode::CopyOnWrite as i32,
        99,
    ] {
        assert_eq!(
            provider.prepare(source("js.1", 0), &rig.key, mode),
            read_only
        );
    }
    let unspecified = MountMode::Unspecified as i32;
    assert_eq!(
        provider.prepare(source("js.1", 0), &rig.key, unspecified),
        Ok(())
    );
    assert_eq!(provider.mount(None).await, invalid("no source"));
    assert_eq!(provider.release(None), invalid("no source"));
    assert_eq!(
        provider.sweep("", &[], &[]),
        invalid("the agent is 1 to 120 bytes")
    );
}

/// Asserts that a `Prepare` of a source prepared before with the same ref is a success that reads
/// nothing again, and that one with another ref is refused naming the first.
#[tokio::test]
async fn prepare_is_idempotent_by_source_and_ref() {
    let rig = Rig::new("prov-idem").await;
    rig.mounted("js.1", 0).await;
    let again = rig.provider.prepare(source("js.1", 0), &rig.key, READ_ONLY);
    assert_eq!(again, Ok(()));
    assert_eq!(
        rig.provider.mount(source("js.1", 0)).await,
        Ok(Rig::path_of("js.1", 0))
    );
    assert_eq!(gets(&rig, &sidecar(&rig)), 1);
    let other = rig.put_list_v2(&empty_files(&["x"])).await;
    let refused = rig.provider.prepare(source("js.1", 0), &other, READ_ONLY);
    assert_eq!(
        refused,
        Err(ProviderError::Refused(format!(
            "the source was prepared with another ref, {}",
            rig.key
        )))
    );
}

/// Asserts that a release removes the source's placement at once, that releasing it again or
/// releasing a source never prepared is a success, that a mount after it is not prepared here, and
/// that the source may be prepared again.
#[tokio::test]
async fn release_removes_the_placement_and_is_idempotent() {
    let rig = Rig::new("prov-release").await;
    rig.mounted("js.1", 0).await;
    let place = Rig::place_of("js.1", 0);
    assert!(rig.bench.store.locate_fs(&place).is_some());
    assert_eq!(rig.provider.release(source("js.1", 0)), Ok(()));
    assert!(rig.bench.store.locate_fs(&place).is_none());
    assert_eq!(rig.provider.release(source("js.1", 0)), Ok(()));
    assert_eq!(rig.provider.release(source("js.never", 3)), Ok(()));
    let after = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(unavailable(after), "the source is not prepared here");
    assert_eq!(rig.mounted("js.1", 0).await, Rig::path_of("js.1", 0));
}

/// Asserts that a `Mount` waiting for a tree when its source is released is refused: the source is
/// gone.
#[tokio::test]
async fn a_mount_waiting_when_the_source_is_released_is_refused() {
    let rig = Rig::new("prov-release-wait").await;
    let gate = rig.bench.rig.tamper.hold_key(&sidecar(&rig));
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    let provider = rig.provider.clone();
    let mount = tokio::spawn(async move { provider.mount(source("js.1", 0)).await });
    until("the sidecar is being read", || {
        gets(&rig, &sidecar(&rig)) == 1
    })
    .await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    rig.provider.release(source("js.1", 0)).unwrap();
    let answer = mount.await.unwrap().unwrap_err();
    assert_eq!(refused(answer), "the source was released");
    gate.add_permits(1);
}

/// Asserts that a sweep releases the sources of the agent whose runs are not kept, leaves the kept
/// runs and every source of another agent, and says how many it released.
#[tokio::test]
async fn a_sweep_releases_the_runs_not_kept_and_leaves_other_agents() {
    let rig = Rig::new("prov-sweep").await;
    for (run, index) in [("js.1", 0), ("js.2", 0), ("js.2", 1)] {
        rig.mounted(run, index).await;
    }
    let other = Some(Source {
        agent: "agent-2".into(),
        run: "js.1".into(),
        index: 0,
        attempt: 0,
    });
    rig.provider
        .prepare(other.clone(), &rig.key, READ_ONLY)
        .unwrap();
    rig.provider.mount(other).await.unwrap();
    let keep = ["js.2".to_owned()];
    assert_eq!(rig.provider.sweep(AGENT, &keep, &[]), Ok(1));
    let store = &rig.bench.store;
    assert!(store.locate_fs(&Rig::place_of("js.1", 0)).is_none());
    assert!(store.locate_fs(&Rig::place_of("js.2", 0)).is_some());
    assert!(store.locate_fs(&Rig::place_of("js.2", 1)).is_some());
    assert_eq!(
        rig.provider.sweep("agent-2", &["js.1".to_owned()], &[]),
        Ok(0)
    );
    assert_eq!(rig.provider.sweep(AGENT, &[], &[]), Ok(2));
    assert_eq!(store.mount_paths().len(), 1);
}

/// Asserts that a provider that starts over a store with placements it made before (a restart of
/// the process empties its table) mounts a source by its placement, says a source it never placed
/// is not prepared here, releases one, and sweeps those it finds under the agent's path.
#[tokio::test]
async fn after_a_restart_the_placements_are_found_by_their_paths() {
    let rig = Rig::new("prov-restart").await;
    rig.mounted("js.1", 0).await;
    rig.mounted("js.2", 0).await;
    let restarted = Provider::with(
        rig.bench.store.clone(),
        rig.bench.rig.cache.clone(),
        settings(),
    );
    assert_eq!(
        restarted.mount(source("js.1", 0)).await,
        Ok(Rig::path_of("js.1", 0))
    );
    let never = restarted.mount(source("js.9", 0)).await.unwrap_err();
    assert_eq!(unavailable(never), "the source is not prepared here");
    assert_eq!(restarted.release(source("js.1", 0)), Ok(()));
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_none()
    );
    assert_eq!(restarted.sweep(AGENT, &[], &[]), Ok(1));
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a placement at a path above a source's (made by another client of the tree store)
/// is not the source's: a mount of the source is not answered by it, and neither a release nor a
/// sweep of the agent removes it.
#[tokio::test]
async fn a_placement_above_a_sources_path_is_not_the_sources() {
    let bench = Bench::new("prov-above").await;
    let parent = Rig::place_of("js.1", 0);
    let parent = parent.trim_end_matches("/0");
    bench
        .store
        .create(bench.request(vec![fs_at(parent)]))
        .unwrap();
    let provider = Provider::with(bench.store.clone(), bench.rig.cache.clone(), settings());
    let answer = provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(unavailable(answer), "the source is not prepared here");
    assert_eq!(provider.release(source("js.1", 0)), Ok(()));
    assert_eq!(provider.sweep(AGENT, &[], &[]), Ok(0));
    assert!(bench.store.locate_fs(parent).is_some());
}

/// Asserts that a release the store cannot make (the cache does not hold the lock of its directory,
/// as an instance during a hand-over) is unavailable, so that the agent asks again.
#[tokio::test]
async fn a_release_the_store_cannot_make_is_unavailable() {
    let rig = Rig::new("prov-release-ro").await;
    rig.mounted("js.1", 0).await;
    let holder = rig.bench.rig.locked();
    let waiting = rig.bench.rig.locked();
    let provider = Provider::with(Bench::reopen(waiting.clone()), waiting, settings());
    let answer = provider.release(source("js.1", 0)).unwrap_err();
    assert_eq!(
        unavailable(answer),
        "the placement was not released: this node does not hold the lock of its cache directory"
    );
    drop(holder);
}

/// Asserts that a list read for want of a sidecar whose bytes do not hash to its key is
/// unavailable, not refused: the cache checks the bytes of a key of `cas/l/` as it fills them and
/// fails the read, as it fails a read of a store that gave bad bytes, which a retry may mend.
#[tokio::test]
async fn a_list_that_does_not_hash_to_its_key_is_unavailable() {
    let rig = Rig::new("prov-mismatch").await;
    let other = tree_list_key(b"other bytes");
    rig.bench
        .rig
        .put(&other, &TreeList::default().encode())
        .await;
    rig.provider
        .prepare(source("js.1", 0), &other, READ_ONLY)
        .unwrap();
    let answer = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    let why = unavailable(answer);
    assert!(why.starts_with("the tree list was not read: "), "{why}");
    assert!(rig.bench.store.mount_paths().is_empty());
}

/// Asserts that a source whose placement's lease ended (the run outlived `trees.ttl_max`) is made
/// again: the next `Mount` is unavailable while it is, and the one after answers the directory.
#[tokio::test]
async fn a_placement_whose_lease_ended_is_made_again() {
    let rig = Rig::new("prov-expired").await;
    rig.mounted("js.1", 0).await;
    rig.bench.rig.clock.advance(2 * 3600 * 1000);
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_none()
    );
    let gone = rig.provider.mount(source("js.1", 0)).await.unwrap_err();
    assert_eq!(
        unavailable(gone),
        "the tree is gone: it is being prepared again"
    );
    let again = rig.provider.mount(source("js.1", 0)).await;
    assert_eq!(again, Ok(Rig::path_of("js.1", 0)));
    assert!(
        rig.bench
            .store
            .locate_fs(&Rig::place_of("js.1", 0))
            .is_some()
    );
}

/// Asserts that a sweep the store cannot make (the cache does not hold the lock of its directory)
/// is unavailable, as a release is.
#[tokio::test]
async fn a_sweep_the_store_cannot_make_is_unavailable() {
    let rig = Rig::new("prov-sweep-ro").await;
    rig.mounted("js.1", 0).await;
    let holder = rig.bench.rig.locked();
    let waiting = rig.bench.rig.locked();
    let provider = Provider::with(Bench::reopen(waiting.clone()), waiting, settings());
    let answer = provider.sweep(AGENT, &[], &[]).unwrap_err();
    assert!(unavailable(answer).starts_with("the placement was not released: "));
    drop(holder);
}

/// Asserts that a source prepared again after a restart of the process (the agent sends the start
/// again) is the same tree at the same path: the prepare is a success and the mount answers it.
#[tokio::test]
async fn a_source_prepared_again_after_a_restart_is_mounted() {
    let rig = Rig::new("prov-restart-prepare").await;
    rig.mounted("js.1", 0).await;
    let restarted = Provider::with(
        rig.bench.store.clone(),
        rig.bench.rig.cache.clone(),
        settings(),
    );
    restarted
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .unwrap();
    assert_eq!(
        restarted.mount(source("js.1", 0)).await,
        Ok(Rig::path_of("js.1", 0))
    );
}
