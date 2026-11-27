//! The requests of the core made in process: answered in the words of the cache, waiting for the
//! write role of the cache, ending by their end, the put of a staged file, the reports, the
//! bound on the fills, and the core's calls with no socket in the process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;

use super::InProcess;
use crate::server::local_cache::attaches::runs::{RunAttach, RunDetach, RunOutcome};
use crate::server::local_cache::attaches::{Attach, Fetches, Levels, Outcome};
use crate::server::local_cache::descriptors::Descriptors;
use crate::server::local_cache::opens::requests::{RequestKind, Requests};
use crate::server::local_cache::opens::{Opens, Spans};
use crate::server::local_cache::pack_testing::{Pack, blob_hex, fills, recency};
use crate::server::local_cache::testing::{Rig, until};
use crate::server::local_cache::tree_lists::{Totals, TreeLists};
use crate::server::local_cache::trees::testing::{Bench, flat_of, fs_at};
use crate::server::local_cache::trees::{FlatTree, TreeStore};
use crate::server::local_cache::{FillError, LocalCache, Mode, Tick};
use crate::server::treefs::fills::{AskError, PAUSE, Place, PlacedTree, Report, TreeAnswer, Until};
use crate::{Backend as _, Clock, Op, TreeDigest};

#[path = "in_process_fill_tests.rs"]
mod fill_words;
#[path = "in_process_mount_tests.rs"]
mod mount;

/// A wait longer than any test: what the requests that must not time out are given.
const LONG: Duration = Duration::from_secs(30);

/// The mount the requests are made for.
const MOUNT: &str = "mount-1";

/// A placement id no placement has.
const NO_PLACEMENT: &str = "00000000000000000000000000000000";

/// A clock that stays at zero: an end on it is that many milliseconds away.
struct Still;

impl Clock for Still {
    fn now(&self) -> u64 {
        0
    }
}

/// The end `ms` milliseconds away, on a clock that does not move.
fn after(ms: u64) -> Until<'static> {
    Until {
        clock: &Still,
        end_ms: ms,
    }
}

/// The requests of a core to `cache` and `trees`, on the runtime of the test, each waiting at most
/// `fetch_timeout`, with no pause between the core's tries.
fn in_process(cache: &LocalCache, trees: Option<&TreeStore>, fetch_timeout: Duration) -> InProcess {
    InProcess::new(
        (cache.clone(), trees.cloned()),
        Handle::current(),
        fetch_timeout,
        Arc::new(|_| {}),
    )
}

/// [`in_process`] waiting [`LONG`], shared with the threads that make the requests.
fn shared(cache: &LocalCache, trees: Option<&TreeStore>) -> Arc<InProcess> {
    Arc::new(in_process(cache, trees, LONG))
}

/// Asserts that a node with no tree store holds no span: asked without waiting, the answer is
/// none, and nothing is waited for.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_without_a_tree_store_holds_no_span() {
    let rig = Rig::new("tfs-in-no-trees-span");
    let over = in_process(&rig.cache, None, Duration::from_secs(30));
    let sha256 = "1".repeat(64);
    assert_eq!(over.span_held((&sha256, 10), ("cas/pack/one", 0)), None);
}

/// Runs `work` on a thread that is not the runtime's, as the core is called, and gives its answer.
async fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sent, got) = tokio::sync::oneshot::channel();
    let _thread = std::thread::spawn(move || {
        let _ = sent.send(work());
    });
    got.await.expect("the work ends")
}

/// Makes `request` of `over` on a thread that is not the runtime's.
async fn ask<T: Send + 'static>(
    over: &Arc<InProcess>,
    request: impl FnOnce(&InProcess) -> T + Send + 'static,
) -> T {
    let over = Arc::clone(over);
    off_runtime(move || request(&over)).await
}

/// What a request answered, each kind of answer as text of its `Debug`.
type Answered = Vec<String>;

/// Every request of the core about the placements, the pins and the fills, with words the cache
/// takes and words it refuses, for the placement `id` of the tree `tree` at `jobs/1f3a/root`,
/// whose flattened tree is `flat`, and the blob `blob` of the pack `pack`.
fn ask_everything(
    over: &InProcess,
    (id, tree, flat): (&str, &str, &FlatTree),
    ((sha256, offset, size), pack): ((&str, u64, u64), &str),
) -> Answered {
    let other = "0".repeat(64);
    let stage = Path::new("/nowhere/staged");
    vec![
        format!("{:?}", over.place(MOUNT, "jobs/1f3a/root")),
        format!("{:?}", over.place(MOUNT, "jobs")),
        format!("{:?}", over.place(MOUNT, "nowhere")),
        format!("{:?}", over.mounted()),
        format!(
            "{:?}",
            over.image(id)
                .map(|placed| placed == PlacedTree::Flat(flat.clone()))
        ),
        format!("{:?}", over.image(NO_PLACEMENT)),
        format!("{:?}", over.image("not-an-id")),
        format!(
            "{:?}",
            over.span((sha256, size), (pack, offset), None).map(drop)
        ),
        format!("{:?}", over.span((sha256, 0), (pack, offset), None)),
        format!(
            "{:?}",
            over.span((sha256, size), ("_journal/x", offset), None)
        ),
        format!("{:?}", over.span(("xyz", size), (pack, offset), None)),
        format!("{:?}", over.fill_blob((sha256, size), (pack, offset), None)),
        format!(
            "{:?}",
            over.fill_blob((sha256, size), (pack, u64::MAX), None)
        ),
        format!(
            "{:?}",
            over.fill_blob((other.as_str(), 5), ("cas/none", 0), None)
        ),
        format!("{:?}", over.fill_blob(("xyz", size), (pack, offset), None)),
        format!("{:?}", over.fill_manifest("nothex", after(u64::MAX))),
        format!("{:?}", over.pin(MOUNT, tree, &[sha256])),
        format!("{:?}", over.pin("hold", tree, &[sha256])),
        format!("{:?}", over.unpin(MOUNT, tree)),
        format!("{:?}", over.pins_live(MOUNT, &[])),
        format!("{:?}", over.places_live(MOUNT, &[id.to_owned()])),
        format!("{:?}", over.places_live(MOUNT, &["bad".to_owned()])),
        format!(
            "{:?}",
            over.put(("_journal/x", other.as_str(), 1), stage, LONG)
        ),
        format!(
            "{:?}",
            over.put(("cas/b/k", other.as_str(), 1), Path::new("rel"), LONG)
        ),
        format!("{:?}", over.put(("cas/b/k", "xyz", 1), stage, LONG)),
        format!(
            "{:?}",
            over.put(("cas/b/k", other.as_str(), 1), stage, LONG)
        ),
    ]
}

/// Asserts the exact answer of every request of the core made in process over a cache and a tree
/// store, including the words of what it refuses: the three answers of a `place`, the paths of the
/// placements, the tree of a placement and of one that is gone, a placement id that is not one, a
/// span and the spans of no bytes, of a reserved key and of a digest that is not one, a fill, one
/// past 2^64, one the upstream does not have and one of a digest that is not one, a manifest of a
/// digest that is not one, a pin, a pin of the mount id of the holds, an unpin, the pins and holds
/// kept, a placement id that is not one, and the puts of a reserved key, of a path that is not
/// absolute, of a digest that is not one and of a staged file that is not there.
#[tokio::test(flavor = "multi_thread")]
async fn every_request_is_answered_in_the_words_of_the_cache() {
    let bench = Bench::new("tfs-in-same").await;
    let created = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1f3a/root")]))
        .expect("a placement");
    let over = in_process(&bench.rig.cache, Some(&bench.store), LONG);
    let (id, tree) = (created.placement.to_string(), created.tree.to_string());
    let flat = flat_of(&bench.store, &created.placement).unwrap();
    let (sha256, offset, size) = bench.pack.blobs[0].clone();
    let pack = bench.pack.key.clone();
    let called = off_runtime(move || {
        ask_everything(
            &over,
            (&id, &tree, &flat),
            ((sha256.as_str(), offset, size), &pack),
        )
    })
    .await;
    let want = [
        format!("Ok(Tree({:?}))", created.placement.to_string()),
        "Ok(Dir)".to_owned(),
        "Ok(Nothing)".to_owned(),
        r#"Ok(["jobs/1f3a/root"])"#.to_owned(),
        "Ok(true)".to_owned(),
        r#"Err("none")"#.to_owned(),
        r#"Err("refused not a placement id")"#.to_owned(),
        "Ok(())".to_owned(),
        r#"Err("refused size is zero")"#.to_owned(),
        r#"Err("refused reserved key")"#.to_owned(),
        r#"Err("refused not a sha256 digest")"#.to_owned(),
        "Ok(())".to_owned(),
        r#"Err("refused offset and size pass 2^64")"#.to_owned(),
        r#"Err("upstream 404")"#.to_owned(),
        r#"Err("refused not a sha256 digest")"#.to_owned(),
        r#"Err("refused not a sha256 digest")"#.to_owned(),
        r#"Ok("ok 1 0 0")"#.to_owned(),
        r#"Ok("the mount id \"hold\" is reserved for holds")"#.to_owned(),
        r#"Ok("ok 1")"#.to_owned(),
        r#"Ok("ok 0")"#.to_owned(),
        r#"Ok("ok 0")"#.to_owned(),
        r#"Ok("refused not a placement id")"#.to_owned(),
        r#"Ok("refused reserved key")"#.to_owned(),
        r#"Ok("refused not an absolute path")"#.to_owned(),
        r#"Ok("refused not a sha256 digest")"#.to_owned(),
        r#"Ok("{\"ok\":false,\"error\":\"stage\"}")"#.to_owned(),
    ];
    assert_eq!(called, want);
}

/// Asserts that a node with no tree store is answered in process with no placement at any path, no
/// paths, `none` for the tree of a placement and for a span, and no hold ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_no_tree_store_is_answered_none_in_process() {
    let rig = Rig::new("tfs-in-nostore");
    let over = in_process(&rig.cache, None, LONG);
    let sha256 = "1".repeat(64);
    let called = off_runtime(move || {
        over.unplace(MOUNT, NO_PLACEMENT);
        vec![
            format!("{:?}", over.place(MOUNT, "jobs/1f3a/root")),
            format!("{:?}", over.mounted()),
            format!("{:?}", over.image(NO_PLACEMENT)),
            format!(
                "{:?}",
                over.span((sha256.as_str(), 1), ("cas/pack/one", 0), None)
            ),
            format!("{:?}", over.places_live(MOUNT, &[NO_PLACEMENT.to_owned()])),
            format!("{:?}", over.lists().is_none()),
        ]
    })
    .await;
    assert_eq!(
        called,
        [
            "Ok(Nothing)",
            "Ok([])",
            r#"Err("none")"#,
            r#"Err("none")"#,
            r#"Ok("ok 0")"#,
            "true",
        ]
    );
}

/// Asserts that a fill asked while the cache is read-only (another process holds the lock of its
/// directory) waits for the write role and then fills: it has not ended and nothing was filled
/// while the lock was held, and once the cache takes the lock, the blob is filled and the fill
/// answered `ok`.
#[tokio::test]
async fn a_fill_waits_for_the_write_role_then_fills() {
    let rig = Rig::new("tfs-in-wait");
    let pack = Pack::put(&rig, "packs/one", &[300]).await;
    let holder = rig.locked();
    let waiting = rig.locked();
    assert_eq!(waiting.mode(), Mode::ReadOnly);
    let over = in_process(&waiting, None, LONG);
    let watchers = waiting.mode_watchers();
    let (sha256, offset, size) = pack.blobs[0].clone();
    let key = pack.key.clone();
    let filled = tokio::spawn(off_runtime(move || {
        over.fill_blob((sha256.as_str(), size), (key.as_str(), offset), None)
    }));
    until("the fill watches the mode of the cache", || {
        waiting.mode_watchers() > watchers
    })
    .await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(!filled.is_finished());
    assert_eq!(fills(&waiting), 0);
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    assert_eq!(filled.await.unwrap(), Ok(()));
    assert_eq!(fills(&waiting), 1);
}

/// Asserts that every request that waits for the write role fails once `fetch_timeout` has passed
/// with the cache still read-only, each in the words of its kind (`read-only` for a fill, a
/// manifest, a span and a fill of a tree; the read-only error for the others), that a hold is not
/// ended then, and
/// that none of the fills, spans and puts asked starts once the cache takes the lock. The clock is
/// paused and moves when every task waits.
#[tokio::test(start_paused = true)]
async fn a_request_still_read_only_after_fetch_timeout_fails_and_starts_nothing() {
    let bench = Bench::new("tfs-in-readonly").await;
    let created = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1f3a/root")]))
        .expect("a placement");
    let id = created.placement.to_string();
    assert!(bench.store.attach(&created.placement, MOUNT).is_ok());
    let holder = bench.rig.locked();
    let waiting = bench.rig.locked();
    let over = in_process(&waiting, Some(&bench.store), LONG);
    let (sha256, offset, size) = bench.pack.blobs[0].clone();
    let pack = bench.pack.key.clone();
    let staged = bench.rig.dir.path().join("staged");
    std::fs::write(&staged, b"x").unwrap();
    let (hold, kept) = (id.clone(), vec![id.clone()]);
    let words = off_runtime(move || {
        over.unplace(MOUNT, &hold);
        vec![
            format!(
                "{:?}",
                over.fill_blob((sha256.as_str(), size), (pack.as_str(), offset), None)
            ),
            format!("{:?}", over.fill_manifest(&sha256, after(u64::MAX))),
            format!(
                "{:?}",
                over.span((sha256.as_str(), size), (pack.as_str(), offset), None)
            ),
            format!("{:?}", over.pin(MOUNT, "tree", &[sha256.as_str()])),
            format!("{:?}", over.unpin(MOUNT, "tree")),
            format!("{:?}", over.pins_live(MOUNT, &[])),
            format!("{:?}", over.places_live(MOUNT, &kept)),
            format!("{:?}", over.place(MOUNT, "jobs")),
            format!("{:?}", over.mounted()),
            format!("{:?}", over.image(&hold)),
            format!(
                "{:?}",
                over.put(("cas/b/k", sha256.as_str(), 1), &staged, LONG)
            ),
            format!(
                "{:?}",
                over.fill_tree(&TreeDigest::of(b"a tree"), after(u64::MAX))
            ),
        ]
    })
    .await;
    let failed = format!("{:?}", Err::<(), _>(AskError::ReadOnly));
    let said = format!("{:?}", Err::<(), _>(AskError::ReadOnly.to_string()));
    let unfilled = r#"Err("read-only")"#.to_owned();
    assert_eq!(
        words,
        [
            unfilled.clone(),
            unfilled.clone(),
            unfilled,
            failed.clone(),
            failed.clone(),
            failed.clone(),
            failed.clone(),
            said.clone(),
            said.clone(),
            said,
            failed,
            format!("{:?}", TreeAnswer::Failed("read-only".to_owned())),
        ]
    );
    let info = bench
        .store
        .placement(&created.placement)
        .expect("a placement");
    assert_eq!(info.attachments, 1);
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fills(&waiting), 0);
    assert_eq!(bench.rig.calls(Op::Put), 0);
}

/// Asserts that a request whose end has passed is not asked, and that one whose end comes before
/// `fetch_timeout` is answered by that end, while one with no end is answered by `fetch_timeout`:
/// each fails in the words of a request out of time, and the fills they started go on
/// and fill once the upstream answers. The clock is paused and moves when every task waits.
#[tokio::test(start_paused = true)]
async fn a_request_ends_by_its_own_end_or_by_fetch_timeout_and_its_fill_goes_on() {
    let bench = Bench::new("tfs-in-ends").await;
    let over = in_process(&bench.rig.cache, Some(&bench.store), LONG);
    let gate = bench.rig.tamper.hold();
    let blobs = bench.pack.blobs.clone();
    let pack = bench.pack.key.clone();
    let words = off_runtime(move || {
        let blob = |n: usize| (blobs[n].0.as_str(), blobs[n].2);
        let at = |n: usize| (pack.as_str(), blobs[n].1);
        vec![
            over.fill_blob(blob(0), at(0), Some(after(0))),
            over.fill_blob(blob(0), at(0), Some(after(50))),
            over.fill_blob(blob(1), at(1), None),
            over.span(blob(2), at(2), Some(after(50))).map(drop),
        ]
    })
    .await;
    let spent = Err(AskError::Spent.to_string());
    let late = Err(AskError::TimedOut.to_string());
    assert_eq!(words, [spent.clone(), spent.clone(), late, spent]);
    gate.add_permits(1_000);
    until("the fills that were not waited for end", || {
        fills(&bench.rig.cache) >= 2
    })
    .await;
}

/// Asserts that a put made in process opens the staged file once and streams it to the upstream
/// as a create-only object named by its SHA-256, answered as the cache answers a put: the
/// first makes the object (one put of the upstream, of the file's bytes), the same put again finds
/// the same bytes and leaves the object as it was, and the staged file is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_put_reads_the_staged_file_once_and_stores_it() {
    let rig = Rig::writing("tfs-in-put");
    let staged = rig.dir.path().join("pack.part");
    std::fs::write(&staged, b"pack bytes").unwrap();
    let over = in_process(&rig.cache, None, LONG);
    let sha256 = TreeDigest::of(b"pack bytes").to_string();
    let path = staged.clone();
    let over = Arc::new(over);
    let put = move || {
        let over = Arc::clone(&over);
        let (sha256, path) = (sha256.clone(), path.clone());
        off_runtime(move || over.put(("cas/b/1/n/pack", sha256.as_str(), 10), &path, LONG))
    };
    let made = put().await;
    assert_eq!(made, Ok(r#"{"ok":true,"created":true}"#.to_owned()));
    assert_eq!(rig.calls(Op::Put), 1);
    let stored = rig.tamper.get("cas/b/1/n/pack", None).await.unwrap();
    assert_eq!(stored, "pack bytes");
    let again = put().await;
    assert_eq!(again, Ok(r#"{"ok":true,"created":false}"#.to_owned()));
    let stored = rig.tamper.get("cas/b/1/n/pack", None).await.unwrap();
    assert_eq!(stored, "pack bytes");
    assert_eq!(std::fs::read(&staged).unwrap(), b"pack bytes");
}

/// The lines of the families the reports of a mount count, in the `/metricz` text of `cache`.
fn reported(cache: &LocalCache) -> Vec<String> {
    cache
        .metrics()
        .lines()
        .filter(|line| line.contains("tierstore_fs_") || line.contains("request_to_start"))
        .map(str::to_owned)
        .collect()
}

/// Asserts that each report made in process is in the cache's metrics when the call returns,
/// with no thread or queue between: the metrics after `started`, `attached`, `fetched`, `levels`,
/// `descriptors`, `opens`, `treelists`, `requests`, `run-attached` and `run-detached` are those of
/// a cache told the same by its own calls, and differ from a cache told nothing.
#[tokio::test]
async fn the_reports_reach_the_cache_without_a_queue() {
    const RUN: &str = "mount-provider/6167656e74/6a732e31";
    let (told, direct) = (Rig::new("tfs-in-told"), Rig::new("tfs-in-direct"));
    let before = reported(&told.cache);
    for rig in [&told, &direct] {
        rig.clock.set(10_000);
        rig.cache.run_requested(RUN);
        rig.clock.advance(180);
    }
    let attach = Attach {
        outcome: Outcome::Ok,
        ms: 3,
    };
    let fetches = Fetches {
        cache: 1,
        filled: 2,
        fill_ms: 9,
        ..Fetches::default()
    };
    let descriptors = Descriptors {
        mount: MOUNT.to_owned(),
        process: 4242,
        held: 12,
        short: 3,
    };
    let mut spans = [Spans::default(); 4];
    spans[1].ms = 30;
    spans[1].buckets[5] = 1;
    let opens = Opens {
        mount: MOUNT.to_owned(),
        process: 4242,
        waiting: 2,
        spans,
    };
    let levels = Levels {
        levels: 4,
        rounds: 1,
    };
    let lists = TreeLists {
        mount: MOUNT.to_owned(),
        process: 4242,
        mapped: 2,
        bytes: 8192,
        cap: 1 << 29,
        totals: Totals {
            cap: 1,
            idle: 2,
            over_cap: 3,
        },
    };
    let mut asked = [Spans::default(); 8];
    asked[RequestKind::Readdir.index()].ms = 12;
    asked[RequestKind::Readdir.index()].buckets[4] = 1;
    let requests = Requests {
        mount: MOUNT.to_owned(),
        process: 4242,
        spans: asked,
    };
    let run_attach = RunAttach {
        outcome: RunOutcome::Ok,
        ms: 35,
        parts: [25, 7, 3],
    };
    let run_detach = RunDetach {
        outcome: RunOutcome::Failed,
        ms: 4,
    };
    let over = in_process(&told.cache, None, LONG);
    let started = format!("{RUN}/0");
    over.report(Report::Started(started.clone()));
    over.report(Report::Attached(attach));
    over.report(Report::Fetched(fetches));
    over.report(Report::Levels(levels));
    over.report(Report::Descriptors(descriptors.clone()));
    over.report(Report::Opens(Box::new(opens.clone())));
    over.report(Report::TreeLists(lists.clone()));
    over.report(Report::Requests(Box::new(requests.clone())));
    over.report(Report::RunAttached(run_attach));
    over.report(Report::RunDetached(run_detach));
    let after = reported(&told.cache);
    let _ = direct.cache.run_started(&started);
    direct.cache.tree_attached(attach);
    direct.cache.manifests_fetched(&fetches);
    direct.cache.tree_levels(levels);
    direct.cache.descriptors_reported(descriptors);
    direct.cache.opens_reported(opens);
    direct.cache.tree_lists_reported(lists);
    direct.cache.requests_reported(requests);
    direct.cache.run_attached(run_attach);
    direct.cache.run_detached(run_detach);
    assert_eq!(after, reported(&direct.cache));
    assert_ne!(after, before);
    assert_eq!(told.cache.run_started(&started), None);
}

/// Asserts that a touch made in process counts its blob as used now, at once: the blob filled
/// first, least recently used before, is the most recently used after.
#[tokio::test]
async fn a_touch_counts_the_blob_as_used_now() {
    let rig = Rig::new("tfs-in-touch");
    let pack = Pack::put(&rig, "packs/two", &[300, 400]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    rig.clock.advance(10);
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    let (first, second) = (blob_hex(&pack.blobs[0].0), blob_hex(&pack.blobs[1].0));
    assert_eq!(recency(&rig.cache), [first.clone(), second.clone()]);
    rig.clock.advance(10);
    in_process(&rig.cache, None, LONG).touch(&[pack.blobs[0].0.as_str()]);
    assert_eq!(recency(&rig.cache), [second, first]);
}

/// Asserts that the requests of a runtime that is gone get no answer: a fill fails as one with no
/// answer in time, and a pin with that error.
#[test]
fn a_request_on_a_runtime_that_is_gone_gets_no_answer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.handle().clone();
    let entered = runtime.enter();
    let rig = Rig::new("tfs-in-gone");
    drop(entered);
    drop(runtime);
    let over = InProcess::new((rig.cache.clone(), None), handle, LONG, Arc::new(|_| {}));
    let sha256 = "1".repeat(64);
    assert_eq!(
        over.fill_manifest(&sha256, after(u64::MAX)),
        Err(AskError::TimedOut.to_string())
    );
    assert_eq!(
        over.pin(MOUNT, "tree", &[sha256.as_str()]),
        Err(AskError::TimedOut)
    );
}

/// Asserts that the core's rests are the pauses it was given: a rest is one [`PAUSE`], a rest
/// before an end is taken only with more than a pause left, and the time an answer is waited for
/// is `fetch_timeout`.
#[test]
fn the_rests_are_the_pauses_given_and_the_answer_timeout_is_fetch_timeout() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    let rig = Rig::new("tfs-in-rests");
    let paused = Arc::new(Mutex::new(Vec::new()));
    let taken = Arc::clone(&paused);
    let pause = Arc::new(move |pause: Duration| taken.lock().unwrap().push(pause));
    let over = InProcess::new(
        (rig.cache.clone(), None),
        runtime.handle().clone(),
        Duration::from_secs(7),
        pause,
    );
    assert_eq!(over.answer_timeout(), Duration::from_secs(7));
    over.rest();
    assert!(!over.rest_before(after(1_000)));
    assert!(over.rest_before(after(1_001)));
    assert_eq!(*paused.lock().unwrap(), [PAUSE, PAUSE]);
    drop(entered);
}

/// Asserts that a lookup of a placed location holds the placement for the mount, and an `unplace`
/// gives the hold back.
#[tokio::test(flavor = "multi_thread")]
async fn a_place_holds_the_placement_and_an_unplace_gives_it_back() {
    let bench = Bench::new("tfs-in-hold").await;
    let created = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1f3a/root")]))
        .expect("a placement");
    let over = Arc::new(in_process(&bench.rig.cache, Some(&bench.store), LONG));
    let asked = Arc::clone(&over);
    let placed = off_runtime(move || asked.place(MOUNT, "jobs/1f3a/root")).await;
    assert_eq!(placed, Ok(Place::Tree(created.placement.to_string())));
    let held = || {
        bench
            .store
            .placement(&created.placement)
            .expect("a placement")
            .attachments
    };
    assert_eq!(held(), 1);
    let id = created.placement.to_string();
    off_runtime(move || over.unplace(MOUNT, &id)).await;
    assert_eq!(held(), 0);
}

/// Asserts that a fill of a tree made in process fills its new manifests in the cache: a root over
/// one directory is filled at both levels, in no time of the cache's clock; a root the upstream
/// does not have fails with the words of its fill and its digest; and a fill whose end has passed
/// is not asked, and fails with the words of a request out of time.
#[tokio::test(flavor = "multi_thread")]
async fn a_fill_of_a_tree_fills_its_new_manifests_in_the_cache() {
    use crate::server::local_cache::TreeFilled;
    use crate::tree_manifest_tests::{dir, file, manifest};

    let rig = Rig::new("tfs-in-tree");
    let below = manifest(&[file("f", 1)]);
    let below_digest = TreeDigest::of(below.as_bytes());
    let root = manifest(&[dir("d", &below_digest)]);
    let root_digest = TreeDigest::of(root.as_bytes());
    for text in [&below, &root] {
        let key = format!("cas/l/{}", TreeDigest::of(text.as_bytes()));
        rig.put(&key, text.as_bytes()).await;
    }
    let absent = TreeDigest::of(b"no tree");
    let over = in_process(&rig.cache, None, LONG);
    let answers = off_runtime(move || {
        [
            over.fill_tree(&root_digest, after(60_000)),
            over.fill_tree(&absent, after(60_000)),
            over.fill_tree(&root_digest, after(0)),
        ]
    })
    .await;
    assert_eq!(
        answers,
        [
            TreeAnswer::Filled(TreeFilled {
                ms: 0,
                levels: vec![1, 1],
            }),
            TreeAnswer::Failed(format!("upstream 404 at {absent}")),
            TreeAnswer::Failed(AskError::Spent.to_string()),
        ]
    );
    assert!(rig.has_file(&format!("cas/l/{below_digest}")));
}

/// The value of the sample `name` (one without labels) in the text of the metrics of `cache` now.
fn sample(cache: &LocalCache, name: &str) -> u64 {
    let text = cache.metrics();
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no sample {name} in\n{text}"));
    line.parse().expect("a number")
}

const RUNNING: &str = "tierstore_cache_fill_socket_running";
const BUSY: &str = "tierstore_cache_fill_socket_busy_total";

/// The files under `dir` that are Unix sockets or are called `fill.sock`.
fn sockets_under(dir: &Path) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt as _;
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_socket() || entry.file_name() == "fill.sock" {
                found.push(entry.path());
            }
        }
    }
    found
}

/// Asserts that a core over the node's cache reaches it with no socket anywhere in the process: an
/// attach fills the tree's manifests in the cache; the first open of a file fills its blob, which
/// holds a permit of the pool of the cache's source fills while it runs (the gauge of the fills
/// running counts it, and no request is answered busy), is pinned for the tree, and reads as its
/// bytes; a put of a staged file answers `created` and the object is at the upstream; a report
/// reaches the cache's metrics; and no file called `fill.sock` and no Unix socket is under the
/// rig's directories, the runtime directory of the core included.
#[test]
fn the_core_fills_through_the_cache_with_no_socket_in_the_process() {
    use crate::server::local_cache::manifest_path;
    use crate::server::treefs::ino;
    use crate::server::treefs::testing::until as spin;
    use crate::server::treefs::testing::{Rig as CoreRig, open_cached, sample as sample_tree, sha};

    const RUN: &str = "mount-provider/61/6a73";
    let rig = CoreRig::new("tfs-in-nosock", sample_tree());
    let (root, manifests) = {
        let built = rig.upstream.built.lock().unwrap();
        (
            built.root.clone(),
            built.manifests.keys().cloned().collect::<Vec<_>>(),
        )
    };

    // A fill of the manifests, by an attach.
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let asked = rig.upstream.asked("fill");
    for digest in &manifests {
        assert!(
            asked.contains(&format!("fill {digest}")),
            "{digest}: {asked:?}"
        );
        assert!(manifest_path(&rig.cache, digest).exists(), "{digest}");
    }

    // A fill of a blob, by the first open of `bin/tool`, which holds a permit while it runs.
    let tool = ino::of(1, 4);
    let blob = sha(b"tool bytes");
    let fill = format!("fill {blob} 10 from cas/l/{blob} @ 0");
    assert!(!asked.contains(&fill));
    assert_eq!(sample(&rig.node, RUNNING), 0);
    rig.upstream.hold_fills();
    let core = Arc::clone(&rig.core);
    let reading = std::thread::spawn(move || {
        let fh = open_cached(&core, tool, false)?;
        let bytes = core.read(fh, 0, 100);
        core.release(fh);
        bytes
    });
    spin(|| sample(&rig.node, RUNNING) == 1);
    assert_eq!(sample(&rig.node, BUSY), 0);
    rig.upstream.release_fills();
    assert_eq!(reading.join().unwrap().unwrap(), b"tool bytes");
    let asked = rig.upstream.asked("fill");
    assert_eq!(asked.iter().filter(|line| **line == fill).count(), 1);
    spin(|| sample(&rig.node, RUNNING) == 0);
    assert_eq!(sample(&rig.node, BUSY), 0);

    // The pin of the blob, for the tree the run attached.
    assert_eq!(rig.node.pinned_bytes(), 10);

    // A put of a staged file.
    // The rig's cache is read-only to puts (`write_through` is off), so the put goes through a
    // cache of the node's kind that takes writes, over the same upstream.
    let staged = rig.dir.path().join("pack.part");
    std::fs::write(&staged, b"pack bytes").unwrap();
    let digest = TreeDigest::of(b"pack bytes").to_string();
    let writing = {
        use crate::TestClock;
        use crate::server::local_cache::CacheConfig;
        use crate::server::local_cache::testing::{FakeDisk, never_expires};
        let path = rig.dir.path().join("cache-writing");
        std::fs::create_dir_all(&path).unwrap();
        let mut config = CacheConfig::new(path, 1 << 30, vec![("cas/".to_owned(), true)]);
        never_expires(&mut config);
        config.write_through = true;
        let handle = rig.work.handle();
        let _entered = handle.enter();
        let upstream: Arc<dyn crate::Backend> = rig.upstream.clone();
        let cache = LocalCache::open(
            config,
            upstream,
            Arc::new(FakeDisk::new()),
            Arc::new(TestClock::new()),
        )
        .unwrap();
        InProcess::new((cache, None), handle.clone(), LONG, Arc::new(|_| {}))
    };
    let put = writing.put(("cas/b/1/n/pack", digest.as_str(), 10), &staged, LONG);
    assert_eq!(put, Ok(r#"{"ok":true,"created":true}"#.to_owned()));
    let objects = rig.work.handle().block_on(rig.upstream.put_objects());
    let stored = objects.iter().find(|(key, _)| key == "cas/b/1/n/pack");
    assert_eq!(
        stored.map(|(_, bytes)| &bytes[..]),
        Some(&b"pack bytes"[..])
    );

    // A report.
    rig.node.run_requested(RUN);
    rig.asks().report(Report::Started(format!("{RUN}/0")));
    let text = rig.node.metrics();
    assert!(
        text.contains("tierstore_request_to_start_seconds_count 1\n"),
        "{text}"
    );

    // No socket anywhere.
    assert_eq!(sockets_under(rig.dir.path()), Vec::<PathBuf>::new());
    assert!(!rig.runtime.join("fill.sock").exists());
}

/// A cache of `concurrency` permits for the source fills, which waits for one for 5 s, and a pack
/// of two blobs in its upstream.
async fn bounded(name: &str, concurrency: usize) -> (Rig, Pack) {
    let rig = Rig::with(name, |config| {
        config.fill_socket_concurrency = concurrency;
        config.fill_socket_wait = Duration::from_secs(5);
    });
    let pack = Pack::put(&rig, "packs/bounded", &[400, 500]).await;
    (rig, pack)
}

/// What a fill of the blob `i` of `pack` is asked with: its digest, offset and size, and the key
/// of the pack.
fn blob_of(pack: &Pack, i: usize) -> ((String, u64, u64), String) {
    (pack.blobs[i].clone(), pack.key.clone())
}

/// A fill of a blob (see [`blob_of`]), made in process.
async fn fill_blob_of(
    over: &Arc<InProcess>,
    ((sha, offset, size), key): ((String, u64, u64), String),
) -> Result<(), String> {
    ask(over, move |over| {
        over.fill_blob((sha.as_str(), size), (key.as_str(), offset), None)
    })
    .await
}

/// Asserts that a node whose file says `fill_socket_concurrency = 1` still bounds the fills the
/// core asks for in process: while the upstream holds the first fill (it holds the one permit,
/// which the gauge of the fills running shows), a fill of another blob finds no permit within
/// `fill_socket_wait` and is answered `busy`, which `tierstore_cache_fill_socket_busy_total`
/// counts once; the first fill then ends `ok`. With `fill_socket_concurrency = 2` the two fills
/// run together and none is busy. The clock is paused and moves when every task waits.
#[tokio::test(start_paused = true)]
async fn a_node_file_with_fill_socket_concurrency_still_bounds_the_fills() {
    let (rig, pack) = bounded("tfs-in-bound-one", 1).await;
    let over = shared(&rig.cache, None);
    let gate = rig.tamper.hold();
    let first = tokio::spawn({
        let (over, blob) = (Arc::clone(&over), blob_of(&pack, 0));
        async move { fill_blob_of(&over, blob).await }
    });
    until("the first fill holds the permit", || {
        sample(&rig.cache, RUNNING) == 1
    })
    .await;
    assert_eq!(
        fill_blob_of(&over, blob_of(&pack, 1)).await,
        Err(FillError::Busy.to_string())
    );
    assert_eq!(sample(&rig.cache, BUSY), 1);
    gate.add_permits(1_000);
    // Waited for by turns of the scheduler, not by an await: the fill's disk work runs on threads
    // while the paused clock would move to the request's end as soon as every task waits.
    until("the first fill ends", || first.is_finished()).await;
    assert_eq!(first.await.unwrap(), Ok(()));
    until("the permit is given back", || {
        sample(&rig.cache, RUNNING) == 0
    })
    .await;

    let (rig, pack) = bounded("tfs-in-bound-two", 2).await;
    let over = shared(&rig.cache, None);
    let gate = rig.tamper.hold();
    let both: Vec<_> = (0..2)
        .map(|i| {
            let (over, blob) = (Arc::clone(&over), blob_of(&pack, i));
            tokio::spawn(async move { fill_blob_of(&over, blob).await })
        })
        .collect();
    until("both fills hold a permit", || {
        sample(&rig.cache, RUNNING) == 2
    })
    .await;
    gate.add_permits(1_000);
    for fill in both {
        until("the fill ends", || fill.is_finished()).await;
        assert_eq!(fill.await.unwrap(), Ok(()));
    }
    assert_eq!(sample(&rig.cache, BUSY), 0);
}

/// Asserts the words of the requests that got no answer, which the core reads (`passes`), and which
/// errors are a lack of descriptors: `EMFILE` and `ENFILE`, not `EACCES` nor an error of no errno.
#[test]
fn the_words_of_a_request_with_no_answer_and_a_lack_of_descriptors_are_named() {
    use crate::server::treefs::fills::{NO_DESCRIPTOR, lacks_descriptors};
    assert_eq!(
        AskError::Unavailable.to_string(),
        "the cache is not answering this mount while its successor takes over"
    );
    assert_eq!(
        AskError::TimedOut.to_string(),
        "the cache gave no answer in time"
    );
    assert_eq!(
        AskError::Spent.to_string(),
        "the cache did not answer within the time the request was given"
    );
    assert_eq!(
        AskError::ReadOnly.to_string(),
        "the cache is read-only: another process holds the lock of its directory"
    );
    assert_eq!(NO_DESCRIPTOR, "no descriptor: ");
    let raw = std::io::Error::from_raw_os_error;
    assert_eq!(lacks_descriptors(&raw(24)), Some(24));
    assert_eq!(lacks_descriptors(&raw(23)), Some(23));
    assert_eq!(lacks_descriptors(&raw(13)), None);
    assert_eq!(lacks_descriptors(&std::io::Error::other("no errno")), None);
}
