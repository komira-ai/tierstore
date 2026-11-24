//! The requests of the core made in process: answered as the fill socket answers them, waiting for
//! the write role of the cache, ending by their end, the put of a staged file, and the reports.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tokio::runtime::Handle;
use tokio::sync::Notify;

use super::InProcess;
use crate::server::fill_socket;
use crate::server::local_cache::attaches::{Attach, Fetches, Outcome};
use crate::server::local_cache::descriptors::Descriptors;
use crate::server::local_cache::pack_testing::{Pack, blob_hex, fills, recency};
use crate::server::local_cache::testing::{Rig, until};
use crate::server::local_cache::trees::TreeStore;
use crate::server::local_cache::trees::testing::{Bench, fs_at};
use crate::server::local_cache::{LocalCache, Mode, Tick};
use crate::server::treefs::fill_client::{AskError, FillClient, PAUSE, Place, Until};
use crate::server::treefs::fills::{Fills, Report};
use crate::{Clock, Op, TreeDigest};

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

/// Runs `work` on a thread that is not the runtime's, as the core is called, and gives its answer.
async fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sent, got) = tokio::sync::oneshot::channel();
    let _thread = std::thread::spawn(move || {
        let _ = sent.send(work());
    });
    got.await.expect("the work ends")
}

/// The fill socket of `cache` and `trees`, served on a scratch path until dropped.
struct Served {
    socket: PathBuf,
    halt: Arc<Notify>,
    _dir: TestDir,
}

impl Served {
    async fn start(name: &str, (cache, trees): (&LocalCache, Option<&TreeStore>)) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let socket = dir.path().join("fill.sock");
        let halt = Arc::new(Notify::new());
        let stores = (cache.clone(), trees.cloned());
        let (path, notify) = (socket.clone(), Arc::clone(&halt));
        drop(tokio::spawn(async move {
            fill_socket::serve(stores, &path, notify).await;
        }));
        until("the fill socket is bound", || socket.exists()).await;
        Self {
            socket,
            halt,
            _dir: dir,
        }
    }

    /// A client of the socket that does not pause and waits [`LONG`].
    fn client(&self) -> FillClient {
        FillClient::pausing(self.socket.clone(), 0, LONG, Arc::new(|_| {}))
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.halt.notify_one();
    }
}

/// What a request answered, each kind of answer as text of its `Debug`.
type Answered = Vec<String>;

/// Every request of the core about the placements, the pins and the fills, with words the socket
/// takes and words it refuses, for the placement `id` of the tree `tree` at `jobs/1f3a/root` and
/// the blob `blob` of the pack `pack`.
fn ask_everything(
    fills: &dyn Fills,
    (id, tree): (&str, &str),
    ((sha256, offset, size), pack): ((&str, u64, u64), &str),
) -> Answered {
    let other = "0".repeat(64);
    let stage = Path::new("/nowhere/staged");
    vec![
        format!("{:?}", fills.place(MOUNT, "jobs/1f3a/root")),
        format!("{:?}", fills.place(MOUNT, "jobs")),
        format!("{:?}", fills.place(MOUNT, "nowhere")),
        format!("{:?}", fills.mounted()),
        format!("{:?}", fills.image(id)),
        format!("{:?}", fills.image(NO_PLACEMENT)),
        format!("{:?}", fills.image("not-an-id")),
        format!("{:?}", fills.span((sha256, size), (pack, offset), None)),
        format!("{:?}", fills.span((sha256, 0), (pack, offset), None)),
        format!(
            "{:?}",
            fills.span((sha256, size), ("_journal/x", offset), None)
        ),
        format!("{:?}", fills.span(("xyz", size), (pack, offset), None)),
        format!(
            "{:?}",
            fills.fill_blob((sha256, size), (pack, offset), None)
        ),
        format!(
            "{:?}",
            fills.fill_blob((sha256, size), (pack, u64::MAX), None)
        ),
        format!(
            "{:?}",
            fills.fill_blob((other.as_str(), 5), ("cas/none", 0), None)
        ),
        format!("{:?}", fills.fill_blob(("xyz", size), (pack, offset), None)),
        format!("{:?}", fills.fill_manifest("nothex", after(u64::MAX))),
        format!("{:?}", fills.pin(MOUNT, tree, &[sha256])),
        format!("{:?}", fills.pin("hold", tree, &[sha256])),
        format!("{:?}", fills.unpin(MOUNT, tree)),
        format!("{:?}", fills.pins_live(MOUNT, &[])),
        format!("{:?}", fills.places_live(MOUNT, &[id.to_owned()])),
        format!("{:?}", fills.places_live(MOUNT, &["bad".to_owned()])),
        format!(
            "{:?}",
            fills.put(("_journal/x", other.as_str(), 1), stage, LONG)
        ),
        format!(
            "{:?}",
            fills.put(("cas/b/k", other.as_str(), 1), Path::new("rel"), LONG)
        ),
        format!("{:?}", fills.put(("cas/b/k", "xyz", 1), stage, LONG)),
        format!(
            "{:?}",
            fills.put(("cas/b/k", other.as_str(), 1), stage, LONG)
        ),
    ]
}

/// Asserts that every request of the core made in process is answered as the fill socket answers
/// it over the same cache and tree store, including the words of what the socket refuses: the
/// three answers of a `place`, the paths of the placements, the tree of a placement and of one
/// that is gone, a placement id that is not one, a span and the spans of no bytes, of a reserved
/// key and of a digest that is not one, a fill, one past 2^64, one the upstream does not have and
/// one of a digest that is not one, a manifest of a digest that is not one, a pin, a pin of the
/// mount id of the holds, an unpin, the pins and holds kept, a placement id that is not one, and
/// the puts of a reserved key, of a path that is not absolute, of a digest that is not one and of
/// a staged file that is not there.
#[tokio::test(flavor = "multi_thread")]
async fn every_request_is_answered_as_the_fill_socket_answers_it() {
    let bench = Bench::new("tfs-in-same").await;
    let created = bench
        .store
        .create(bench.request(vec![fs_at("jobs/1f3a/root")]))
        .expect("a placement");
    let served = Served::start("tfs-in-same-sock", (&bench.rig.cache, Some(&bench.store))).await;
    let client = served.client();
    let over = in_process(&bench.rig.cache, Some(&bench.store), LONG);
    let (id, tree) = (created.placement.to_string(), created.tree.to_string());
    let (sha256, offset, size) = bench.pack.blobs[0].clone();
    let pack = bench.pack.key.clone();
    let (socket, called) = off_runtime(move || {
        let ask = |fills: &dyn Fills| {
            ask_everything(
                fills,
                (&id, &tree),
                ((sha256.as_str(), offset, size), &pack),
            )
        };
        (ask(&client), ask(&over))
    })
    .await;
    assert_eq!(called, socket);
    assert_eq!(
        called[0],
        format!("Ok(Tree({:?}))", created.placement.to_string())
    );
    assert_eq!(called[1], "Ok(Dir)");
    assert_eq!(called[2], "Ok(Nothing)");
    assert_eq!(called[3], r#"Ok(["jobs/1f3a/root"])"#);
    assert_eq!(called[5], r#"Err("none")"#);
    assert_eq!(called[6], r#"Err("refused not a placement id")"#);
    assert_eq!(called[11], "Ok(())");
    assert_eq!(called[13], r#"Err("upstream 404")"#);
    assert_eq!(called[16], r#"Ok("ok 1 0 0")"#);
    assert_eq!(called[22], r#"Ok("refused reserved key")"#);
    assert_eq!(called[25], r#"Ok("{\"ok\":false,\"error\":\"stage\"}")"#);
}

/// Asserts that a node with no tree store is answered in process as the fill socket answers it:
/// no placement at any path, no paths, `none` for the tree of a placement and for a span, and no
/// hold ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_no_tree_store_is_answered_as_the_fill_socket_answers_it() {
    let rig = Rig::new("tfs-in-nostore");
    let served = Served::start("tfs-in-nostore-sock", (&rig.cache, None)).await;
    let client = served.client();
    let over = in_process(&rig.cache, None, LONG);
    let sha256 = "1".repeat(64);
    let (socket, called) = off_runtime(move || {
        let ask = |fills: &dyn Fills| {
            fills.unplace(MOUNT, NO_PLACEMENT);
            vec![
                format!("{:?}", fills.place(MOUNT, "jobs/1f3a/root")),
                format!("{:?}", fills.mounted()),
                format!("{:?}", fills.image(NO_PLACEMENT)),
                format!(
                    "{:?}",
                    fills.span((sha256.as_str(), 1), ("cas/pack/one", 0), None)
                ),
                format!("{:?}", fills.places_live(MOUNT, &[NO_PLACEMENT.to_owned()])),
            ]
        };
        (ask(&client), ask(&over))
    })
    .await;
    assert_eq!(called, socket);
    assert_eq!(
        called,
        [
            "Ok(Nothing)",
            "Ok([])",
            r#"Err("none")"#,
            r#"Err("none")"#,
            r#"Ok("ok 0")"#
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
/// manifest and a span; the read-only error for the others), that a hold is not ended then, and
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
/// each fails in the client's words for a request out of time, and the fills they started go on
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
/// as a create-only object named by its SHA-256, answered as the fill socket answers a put: the
/// first makes the object (one put of the upstream, of the file's bytes), the same put again finds
/// the same bytes and makes none, and the staged file is left as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_put_reads_the_staged_file_once_and_stores_it() {
    let rig = Rig::writing("tfs-in-put");
    let staged = rig.dir.path().join("pack.part");
    std::fs::write(&staged, b"pack bytes").unwrap();
    let over = in_process(&rig.cache, None, LONG);
    let sha256 = TreeDigest::of(b"pack bytes").to_string();
    let path = staged.clone();
    let (made, again) = off_runtime(move || {
        let put = || over.put(("cas/b/1/n/pack", sha256.as_str(), 10), &path, LONG);
        (put(), put())
    })
    .await;
    assert_eq!(made, Ok(r#"{"ok":true,"created":true}"#.to_owned()));
    assert_eq!(again, Ok(r#"{"ok":true,"created":false}"#.to_owned()));
    assert_eq!(rig.calls(Op::Put), 1);
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
/// with no thread or queue between: the metrics after `started`, `attached`, `fetched` and
/// `descriptors` are those of a cache told the same by its own calls, and differ from a cache told
/// nothing.
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
    let over = in_process(&told.cache, None, LONG);
    let started = format!("{RUN}/0");
    over.report(Report::Started(started.clone()));
    over.report(Report::Attached(attach));
    over.report(Report::Fetched(fetches));
    over.report(Report::Descriptors(descriptors.clone()));
    let after = reported(&told.cache);
    let _ = direct.cache.run_started(&started);
    direct.cache.tree_attached(attach);
    direct.cache.manifests_fetched(&fetches);
    direct.cache.descriptors_reported(descriptors);
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

/// Asserts that the requests of a runtime that is gone get no answer, as a socket that answers
/// nothing: a fill fails as one with no answer in time, and a pin with that error.
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

/// Asserts that a request in process is in the place answer of the socket: a lookup of a placed
/// location holds the placement for the mount, and an `unplace` gives the hold back.
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
