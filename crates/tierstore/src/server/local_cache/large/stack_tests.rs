//! The read path of `[cache]` ([`stack::start`]) over the fake upstream of `tierstore-fetch-api`,
//! attached to a cache and read through the real S3 front: what it builds on disk, the overlap
//! budget, what it leaves alone, the evictor lock, the restart, the entries an earlier process
//! left, and its families on `/metricz`. The evictor's and the committer's clock is injected and
//! moved by hand; the engine's timers never fire in these tests.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};
use tierstore_entry::inproc::InProcStore;
use tierstore_evict::EvictConfig;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::{EntrySpec, EntryStore as _};
use tierstore_fetch_api::fake::{FakeObject, FakeUpstream};
use tierstore_fetch_api::ids::PackKey;
use tierstore_fetch_api::upstream::Location;
use tierstore_format::ids::Generation;
use tierstore_journal::{BootId, FileKind};
use tierstore_testkit::dir::TestDir;

use super::super::testing::{Rig, wait_until};
use super::stack::{self, Stack, StackParts, overlap_budget};
use super::testing::{
    LIMIT, MIB, build_node_file, cache_settings, generation, hot, object, pack_id,
};
use crate::Backend;
use crate::server::auth::sign;
use crate::server::testing::{BUCKET, KEY_ID, NOW, SECRET};
use crate::server::{Server, ServerConfig};

/// The boot every stack of these tests runs in.
const BOOT: BootId = BootId([9; 16]);

/// A cache read through the S3 front, with the read path of a stack attached.
struct Front {
    rig: Rig,
    server: Arc<Server>,
    config: ServerConfig,
}

impl Front {
    /// A cache in a directory called `name`, whose `whole_fill_limit` is one cell, with `stack`
    /// attached.
    fn over(name: &str, stack: &Stack) -> Self {
        let rig = Rig::with(name, |cache| cache.whole_limit = LIMIT);
        rig.clock.set(NOW);
        assert!(rig.cache.route_large(stack.large()));
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, rig.clock.clone());
        config.rate_clock = rig.clock.clone();
        config.cache = Some(rig.cache.clone());
        let backend: Arc<dyn Backend> = Arc::new(rig.cache.clone());
        let server = Server::new(backend, config.clone());
        Self {
            rig,
            server,
            config,
        }
    }

    /// A signed GET of `key`: the status and the body.
    async fn get(&self, key: &str) -> (StatusCode, Vec<u8>) {
        let uri = format!("/{BUCKET}/{key}");
        let request: Request<Full<Bytes>> = sign(&self.config, "GET", &uri, &[], b"");
        let response = self.server.handle(request).await;
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        (parts.status, body)
    }
}

/// The fake upstream with `key` stored as `bytes` at `g`, with a hot copy.
fn holding(objects: &[(&str, &Bytes, Generation)]) -> Arc<FakeUpstream> {
    let upstream = Arc::new(FakeUpstream::new());
    for (key, bytes, g) in objects {
        let size = bytes.len() as u64;
        let location = Location {
            generation: *g,
            size: Some(size),
            sources: vec![hot(size)],
            conflicts: Vec::new(),
        };
        let object = FakeObject {
            generation: *g,
            bytes: (*bytes).clone(),
            location,
        };
        upstream.insert(PackKey((*key).to_owned()), object);
    }
    upstream
}

/// The parts of a stack at `root` with `budget`, the layout of the release before in `legacy`.
fn parts(root: &Path, budget: u64, legacy: &Path, upstream: &Arc<FakeUpstream>) -> StackParts {
    StackParts {
        root: root.to_owned(),
        budget,
        legacy: legacy.to_owned(),
        upstream: upstream.clone(),
        boot: Some(BOOT),
        clock: Arc::new(ManualClock::new()),
        // The tests run the passes they look at themselves: the loop's first is an hour away.
        evict_every: Duration::from_secs(3600),
    }
}

/// Waits until nothing holds the store any more: the stack, the engine and the cache are gone,
/// and with them every lock of the store's data files.
async fn gone(store: Weak<InProcStore>) {
    wait_until("the store is dropped", || store.upgrade().is_none()).await;
}

/// Waits until no entry of `stack` is held (the engine's fills and the front's reads are done).
async fn settled(stack: &Stack) {
    let free = || stack.store().entries().iter().all(|info| info.holders == 0);
    wait_until("no entry is held", free).await;
}

/// The relative path of `path` under `root`, as text.
fn under(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap().display().to_string()
}

/// Whether `text` is `n` lower-case hex digits.
fn is_hex(text: &str, n: usize) -> bool {
    text.len() == n && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Asserts that a file with a `[cache]` root and budget starts a stack that serves a large GET
/// through the engine: the bytes are the object's, every part the upstream answered carried the
/// object's generation, and the object is a sparse pack file
/// `packs-v2/<aa>/<bb>/<hash>.pack` of its size under the configured root.
#[tokio::test]
async fn a_cache_section_builds_a_stack_that_serves_a_large_get_through_the_engine() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-serve");
    let root = dir.path().join("new");
    let table = format!("root = \"{}\"\nbudget = \"64MiB\"", root.display());
    let settings = cache_settings(&build_node_file(
        dir.path(),
        "http://192.0.2.1:9000",
        &table,
    ));
    let read = settings.read_path.clone().unwrap();
    assert_eq!(
        (read.root.as_path(), read.budget),
        (root.as_path(), 64 * MIB)
    );
    let bytes = object(3 * MIB, 1);
    let g = generation(0x21);
    let upstream = holding(&[("cas/big", &bytes, g)]);
    let stack = stack::start(parts(
        &read.root,
        read.budget,
        &settings.cache.path,
        &upstream,
    ));
    let stack = stack.unwrap();
    let front = Front::over("tsstack-serve-front", &stack);
    let (status, got) = front.get("cas/big").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, bytes.to_vec());
    let gets = upstream.gets();
    assert!(!gets.is_empty());
    assert!(gets.iter().all(|get| get.generation == g));
    let entry = stack.store().entry(&pack_id("cas/big", g)).unwrap();
    let data = entry.path(FileKind::Data);
    assert_eq!(std::fs::metadata(&data).unwrap().len(), 3 * MIB);
    let shown = under(&data, &root);
    let parts: Vec<&str> = shown.split('/').collect();
    assert_eq!(parts.len(), 4, "{shown}");
    assert_eq!(parts[0], "packs-v2");
    assert!(is_hex(parts[1], 2) && is_hex(parts[2], 2), "{shown}");
    let name = parts[3].strip_suffix(".pack").unwrap();
    assert!(is_hex(name, 64), "{shown}");
    assert_eq!((&name[..2], &name[2..4]), (parts[1], parts[2]));
}

/// Asserts that the budget of `packs-v2/` is half of `cache.budget` while a file of the layout of
/// the release before remains under `obj/` or `extents-v1/` (at any depth), and the whole of it
/// when neither holds one (empty directories, or none at all).
#[test]
fn the_budget_is_halved_while_digest_v1_files_exist() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-overlap");
    let legacy = dir.path().join("cache");
    assert_eq!(overlap_budget(10 * MIB, &legacy), 10 * MIB);
    std::fs::create_dir_all(legacy.join("obj/ab/cd")).unwrap();
    std::fs::create_dir_all(legacy.join("extents-v1/ab")).unwrap();
    std::fs::create_dir_all(legacy.join("packs-v2/ab")).unwrap();
    std::fs::write(legacy.join("packs-v2/ab/x.pack"), b"new").unwrap();
    assert_eq!(overlap_budget(10 * MIB, &legacy), 10 * MIB);
    std::fs::write(legacy.join("obj/ab/cd/x"), b"old").unwrap();
    assert_eq!(overlap_budget(10 * MIB, &legacy), 5 * MIB);
    std::fs::remove_file(legacy.join("obj/ab/cd/x")).unwrap();
    std::fs::write(legacy.join("extents-v1/ab/y.ext"), b"old").unwrap();
    assert_eq!(overlap_budget(10 * MIB + 1, &legacy), 5 * MIB);
}

/// Asserts what the halved budget does: with `cache.budget` of 8 MiB and a digest-v1 file left,
/// a pass of the evictor after the grace window, with two objects of 3 MiB read, removes one
/// whole entry and leaves 3 MiB; with no digest-v1 file the same reads fit the budget and the pass
/// removes nothing. No pass evicts before the grace window.
#[tokio::test]
async fn a_pass_after_the_grace_window_evicts_down_to_the_halved_budget() {
    for (legacy_file, budget, removed, left) in
        [(true, 4 * MIB, 1, 3 * MIB), (false, 8 * MIB, 0, 6 * MIB)]
    {
        let name = format!("tsstack-evict-{legacy_file}");
        let dir = TestDir::under(std::env::temp_dir(), &name);
        let legacy = dir.path().join("cache");
        if legacy_file {
            std::fs::create_dir_all(legacy.join("obj/aa/bb")).unwrap();
            std::fs::write(legacy.join("obj/aa/bb/old"), b"old").unwrap();
        }
        let (a, b) = (object(3 * MIB, 2), object(3 * MIB, 3));
        let upstream = holding(&[("cas/a", &a, generation(2)), ("cas/b", &b, generation(3))]);
        let clock = ManualClock::new();
        let mut stack_parts = parts(&dir.path().join("new"), 8 * MIB, &legacy, &upstream);
        stack_parts.clock = Arc::new(clock.clone());
        let stack = stack::start(stack_parts).unwrap();
        assert_eq!(stack.budget(), budget);
        let front = Front::over(&format!("{name}-front"), &stack);
        assert_eq!(front.get("cas/a").await, (StatusCode::OK, a.to_vec()));
        assert_eq!(front.get("cas/b").await, (StatusCode::OK, b.to_vec()));
        settled(&stack).await;
        assert!(stack.pass().is_none());
        clock.advance(EvictConfig::GRACE_MS);
        let report = stack.pass().unwrap();
        assert_eq!(report.used_before, 6 * MIB);
        assert_eq!((report.entries_removed, report.used_after), (removed, left));
    }
}

/// Asserts that the stack ignores the layout of the release before: with `cache.root` the
/// directory of `local_cache.path`, and an `obj/` object and an `extents-v1/` entry of the key
/// there holding other bytes, the GET is served from `packs-v2/` through the engine with the
/// upstream's bytes, and the old files are left byte for byte.
#[tokio::test]
async fn old_entries_are_ignored_and_packs_v2_is_served() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-old");
    let root = dir.path().join("cache");
    let old: [(&str, &[u8]); 4] = [
        ("obj/aa/bb/0123", b"stale whole object"),
        ("obj/aa/bb/0123.meta", b"{\"key\":\"cas/big\"}"),
        ("extents-v1/aa/bb/0123", b"stale extent"),
        ("extents-v1/aa/bb/0123.ext", b"{\"key\":\"cas/big\"}"),
    ];
    for (path, bytes) in old {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let bytes = object(2 * MIB + 5, 4);
    let g = generation(4);
    let upstream = holding(&[("cas/big", &bytes, g)]);
    let stack = stack::start(parts(&root, 64 * MIB, &root, &upstream)).unwrap();
    assert_eq!(stack.budget(), 32 * MIB);
    let front = Front::over("tsstack-old-front", &stack);
    assert_eq!(front.get("cas/big").await, (StatusCode::OK, bytes.to_vec()));
    assert!(!upstream.gets().is_empty());
    let entry = stack.store().entry(&pack_id("cas/big", g)).unwrap();
    assert!(
        entry
            .path(FileKind::Data)
            .starts_with(root.join("packs-v2"))
    );
    for (path, bytes) in old {
        assert_eq!(std::fs::read(root.join(path)).unwrap(), bytes, "{path}");
    }
}

/// Asserts that one process at a time is the evictor of a root: the first stack takes
/// `packs-v2/.lock`, a second over the same root runs without it and its pass does nothing; once
/// the first is gone, the second's next pass takes the lock.
#[tokio::test]
async fn a_second_process_cannot_become_the_evictor() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-lock");
    let (root, legacy) = (dir.path().join("new"), dir.path().join("cache"));
    let upstream = holding(&[]);
    let first = stack::start(parts(&root, 8 * MIB, &legacy, &upstream)).unwrap();
    assert!(first.is_evictor());
    assert!(root.join("packs-v2/.lock").is_file());
    let second = stack::start(parts(&root, 8 * MIB, &legacy, &upstream)).unwrap();
    assert!(!second.is_evictor());
    assert!(second.pass().is_none());
    assert!(!second.is_evictor());
    drop(first);
    assert!(
        second.pass().is_none(),
        "the grace window of the new evictor"
    );
    assert!(second.is_evictor());
}

/// Asserts that a stack whose root cannot be made (a file is in its place) is refused, saying
/// that it is the new cache's.
#[tokio::test]
async fn a_root_that_cannot_be_made_refuses_the_start() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-badroot");
    let root = dir.path().join("file");
    std::fs::write(&root, b"not a directory").unwrap();
    let refused = stack::start(parts(&root, MIB, &dir.path().join("cache"), &holding(&[])));
    let why = refused.err().unwrap();
    assert!(why.starts_with("the new cache: "), "{why}");
}

/// Starts a stack at `root` over `upstream`, reads `key` through a front once, then stops the
/// stack and waits until every file of the store is closed: what was read, and the path of the
/// entry's data file.
async fn read_once(root: &Path, upstream: &Arc<FakeUpstream>, key: &str, g: Generation) -> PathBuf {
    let stack = stack::start(parts(root, 64 * MIB, &root.join("cache"), upstream)).unwrap();
    let store = Arc::downgrade(stack.store());
    let front = Front::over(&format!("tsstack-once-{}", key.replace('/', "-")), &stack);
    let (status, _) = front.get(key).await;
    assert_eq!(status, StatusCode::OK);
    settled(&stack).await;
    let data = stack
        .store()
        .entry(&pack_id(key, g))
        .unwrap()
        .path(FileKind::Data);
    drop(front);
    drop(stack);
    gone(store).await;
    data
}

/// Asserts that a stack started again over the same root in the same boot serves an object read
/// before from the cells its journal kept, with the same bytes and no part fetched again.
#[tokio::test]
async fn restart_replays_the_journal_and_serves_journaled_cells_without_refetch() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-restart");
    let root = dir.path().join("new");
    let bytes = object(3 * MIB + 7, 5);
    let g = generation(5);
    let before = holding(&[("cas/big", &bytes, g)]);
    read_once(&root, &before, "cas/big", g).await;
    assert!(!before.gets().is_empty());
    let after = holding(&[("cas/big", &bytes, g)]);
    let stack = stack::start(parts(&root, 64 * MIB, &dir.path().join("cache"), &after)).unwrap();
    let front = Front::over("tsstack-restart-front", &stack);
    assert_eq!(front.get("cas/big").await, (StatusCode::OK, bytes.to_vec()));
    assert!(after.gets().is_empty(), "{:?}", after.gets());
}

/// Asserts that the entries an earlier process left are counted against the budget when the
/// lock is taken: the oldest of them is removed (its data file, journal and snapshot) until the
/// rest fit, and an entry that another process holds open is never removed. A directory of
/// `packs-v2/` that cannot be read is passed over, and the sweep goes on with the others.
#[tokio::test]
async fn entries_left_by_an_earlier_process_are_removed_oldest_first_past_the_budget() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-sweep");
    let root = dir.path().join("new");
    let (a, b, c) = (object(3 * MIB, 6), object(3 * MIB, 7), object(3 * MIB, 8));
    let (ga, gb, gc) = (generation(6), generation(7), generation(8));
    let upstream = holding(&[("cas/a", &a, ga), ("cas/b", &b, gb), ("cas/c", &c, gc)]);
    let paths = [
        read_once(&root, &upstream, "cas/a", ga).await,
        read_once(&root, &upstream, "cas/b", gb).await,
        read_once(&root, &upstream, "cas/c", gc).await,
    ];
    for (path, seconds) in paths.iter().zip([10, 20, 30]) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }
    // Another process holds the oldest entry open.
    let other = InProcStore::new(&root, Some(BOOT)).unwrap();
    let held = EntrySpec {
        pack: pack_id("cas/a", ga),
        size: 3 * MIB,
    };
    let _open = other.open(&held).unwrap();
    let unreadable = root.join("packs-v2/zz");
    std::fs::create_dir_all(&unreadable).unwrap();
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&unreadable, mode(0o000)).unwrap();
    let stack = stack::start(parts(&root, 4 * MIB, &dir.path().join("cache"), &upstream)).unwrap();
    std::fs::set_permissions(&unreadable, mode(0o755)).unwrap();
    assert!(stack.is_evictor());
    let stem = |path: &PathBuf, suffix: &str| {
        let mut name = path.with_extension("").into_os_string();
        name.push(suffix);
        PathBuf::from(name)
    };
    assert!(paths[0].exists(), "the held entry is kept");
    for suffix in [".pack", ".jnl", ".meta"] {
        assert!(
            !stem(&paths[1], suffix).exists(),
            "{suffix} of the oldest unheld entry"
        );
    }
    assert!(paths[2].exists(), "the newest entry fits the budget");
}

/// Asserts that `/metricz` shows, after the cache's own counters, the families of the read path
/// with the engine's counts: the bytes the engine fetched for one read of 3 MiB, the same when the
/// page is shown again (the counts are copied once), the budget's ceiling, and no second
/// `tierstore_build_info`.
#[tokio::test]
async fn the_families_of_the_read_path_are_on_the_metrics_page() {
    let dir = TestDir::under(std::env::temp_dir(), "tsstack-metrics");
    let bytes = object(3 * MIB, 9);
    let upstream = holding(&[("cas/big", &bytes, generation(9))]);
    let root = dir.path().join("new");
    let stack = stack::start(parts(&root, 64 * MIB, &dir.path().join("cache"), &upstream)).unwrap();
    let front = Front::over("tsstack-metrics-front", &stack);
    assert_eq!(front.get("cas/big").await.0, StatusCode::OK);
    let fetched = format!("tierstore_bytes_fetched_total {}\n", 3 * MIB);
    for _ in 0..2 {
        let page = front.rig.cache.metrics();
        assert!(page.contains(&fetched), "{page}");
        assert!(
            page.contains("# TYPE tierstore_gets_total counter\n"),
            "{page}"
        );
        assert!(
            page.contains("# TYPE tierstore_budget_ceiling_parts gauge\n"),
            "{page}"
        );
        assert!(
            page.contains("# TYPE tierstore_evictions_total counter\n"),
            "{page}"
        );
        assert!(!page.contains("tierstore_build_info"), "{page}");
    }
}
