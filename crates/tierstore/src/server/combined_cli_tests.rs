//! The start of a storage node with a local cache: what it serves, and the refusals of its start.

use std::fs::Permissions;
use std::net::SocketAddr;

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey};
use tierstore_testkit::dir::TestDir;
use tokio::sync::oneshot;

use super::Drained;
use super::cache_cli_tests::{client, probe};
use super::cli::{Error, checked, exit_status};
use super::combined_cli::serve_combined;
use super::config::{PerRun, Settings, settings_from_text};
use super::config_combined::CombinedSettings;
use super::local_cache::testing::{FakeDisk, Quirk};
use super::local_cache::{Disk, LocalCache, Mode, SystemDisk};
use crate::{Backend, Clock, DirBackend, Error as BackendError, MemoryBackend, SystemClock};
use objstore_s3::ObjectStore;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

const KEY_ID: &str = "AKIDCACHE";
const SECRET: &str = "cache-secret-0123456789";

/// The knobs of a run whose drain does not wait.
pub(super) const QUICK: PerRun = PerRun {
    drain_lb_wait: Some(Duration::from_millis(50)),
    drain_deadline: Some(Duration::from_secs(60)),
    state_dir: None,
};

/// A scratch directory with a key file, and the directories of a node's tiers and cache in it.
pub(super) struct Node {
    pub(super) dir: TestDir,
    pub(super) key: PathBuf,
}

impl Node {
    pub(super) fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let key = dir.path().join("key");
        std::fs::write(&key, format!("{KEY_ID}\n{SECRET}\n")).unwrap();
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
        Self { dir, key }
    }

    pub(super) fn hot(&self) -> PathBuf {
        self.dir.path().join("hot")
    }

    pub(super) fn cold(&self) -> PathBuf {
        self.dir.path().join("cold")
    }

    pub(super) fn cache_dir(&self) -> PathBuf {
        self.dir.path().join("cache")
    }

    /// The file of a node with a cache, with this text added to `[hot_store]`.
    fn text(&self, hot_extra: &str) -> String {
        self.text_with("", hot_extra, "")
    }

    /// The same file with `server_extra` added to `[server]`, `hot_extra` to `[hot_store]`, and
    /// `interfaces` (whole `[[interface]]` tables) after the `s3` one.
    pub(super) fn text_with(
        &self,
        server_extra: &str,
        hot_extra: &str,
        interfaces: &str,
    ) -> String {
        format!(
            r#"
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}
{interfaces}
[server]
proxy_id        = "n1"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server_extra}

[hot_store]
kind   = "dir"
path   = "{hot}"
budget = "1MiB"
{hot_extra}

[cold_store]
kind = "dir"
path = "{cold}"

[local_cache]
path   = "{cache}"
budget = "1MiB"

[[policy]]
prefix = "cas/"
cache  = true
"#,
            key = self.key.display(),
            hot = self.hot().display(),
            cold = self.cold().display(),
            cache = self.cache_dir().display(),
        )
    }

    pub(super) fn settings(&self, hot_extra: &str) -> CombinedSettings {
        let settings = settings_from_text(&self.text(hot_extra), QUICK).unwrap();
        settings.into_combined().unwrap()
    }

    /// Writes the active format into the hot directory, as `format-epoch --activate` does for a
    /// new store, so that the node takes writes.
    pub(super) async fn activate(&self) {
        let active = bytes::Bytes::from_static(br#"{"epoch":2,"phase":"active"}"#);
        let hot = Self::tier(self.hot());
        hot.put("_tiering/format", active).await.unwrap();
    }

    /// The tier directory `path` as a backend, to read what the node stored there.
    pub(super) fn tier(path: PathBuf) -> DirBackend {
        DirBackend::open(path).unwrap()
    }
}

/// Runs `serve_combined` on this disk, calls `work` with the address, then stops it.
pub(super) async fn serving<F, Fut>(
    settings: &CombinedSettings,
    disk: Arc<dyn Disk>,
    work: F,
) -> Result<Drained, Error>
where
    F: FnOnce(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = serve_combined(
        settings,
        disk,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound.s3);
        },
    );
    let driving = async {
        // A start that fails never tells its address.
        if let Ok(bound) = addr.await {
            work(bound).await;
        }
        let _ = stop.send(());
    };
    let (drained, ()) = tokio::join!(running, driving);
    drained
}

/// Asserts that a node with a cache takes a put through the front and answers the reads of it
/// from the cache: the put is in the hot directory, the object reads back (twice, as hits), `/readyz` says `ready read-write`, `/usagez` is the proxy's, and
/// `/metricz` has the proxy's block and the cache's. The start succeeding also shows that the
/// cache's write probe of an upstream is not made against the proxy, which would refuse it.
#[tokio::test]
async fn a_combined_node_serves_a_put_and_answers_the_next_read_from_its_cache() {
    let node = Node::new("tscombined-serve");
    node.activate().await;
    let settings = node.settings("");
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let hot = node.hot();
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        let key = ObjectKey::new("cas/obj").unwrap();
        store
            .put_new(&key, Bytes::from_static(b"hello"), None)
            .await
            .unwrap();
        let all = ByteRange::new(0, 5).unwrap();
        for _ in 0..2 {
            assert_eq!(&store.get_range(&key, all).await.unwrap()[..], b"hello");
        }
        let kept = Node::tier(hot).head("cas/obj").await.unwrap();
        assert_eq!(kept.size, 5);
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-write"));
        let (status, text) = probe(addr, "/usagez").await;
        let watched = text.starts_with("no usage yet") || text.starts_with("usage published");
        assert!(status == 200 && watched, "{status} {text}");
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        assert!(text.contains("tierstore_reads_total"), "{text}");
        assert!(text.contains("tierstore_cache_hits_total 2\n"), "{text}");
    };
    let drained = serving(&settings, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that a delete through the front removes the object from the cache and from the tier
/// it was in: a read after it is not found, and the hot directory no longer has the key.
#[tokio::test]
async fn a_delete_through_a_combined_node_removes_the_object_everywhere() {
    let node = Node::new("tscombined-delete");
    node.activate().await;
    let settings = node.settings("");
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let hot = node.hot();
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        let key = ObjectKey::new("cas/gone").unwrap();
        store
            .put_new(&key, Bytes::from_static(b"bytes"), None)
            .await
            .unwrap();
        let all = ByteRange::new(0, 5).unwrap();
        assert_eq!(&store.get_range(&key, all).await.unwrap()[..], b"bytes");
        store.delete(&key).await.unwrap();
        assert!(store.get_range(&key, all).await.is_err());
        let left = Node::tier(hot).head("cas/gone").await;
        assert_eq!(left, Err(BackendError::NotFound("cas/gone".to_owned())));
    };
    let drained = serving(&settings, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that a node over a cache directory that another cache holds is read-only and still
/// takes writes: `/readyz` says `ready read-only`, a put passes through to the tier, and the
/// object reads back.
#[tokio::test]
async fn a_combined_node_over_a_held_directory_is_read_only_and_still_writes_through() {
    let node = Node::new("tscombined-held");
    node.activate().await;
    let settings = node.settings("");
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let elsewhere: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let config = settings.cache.config.clone();
    let holder = LocalCache::open_locked(config, elsewhere, Arc::clone(&disk), clock).unwrap();
    assert_eq!(holder.mode(), Mode::ReadWrite);
    let hot = node.hot();
    let work = |addr: SocketAddr| async move {
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-only"));
        let store = client(addr);
        let key = ObjectKey::new("cas/held").unwrap();
        store
            .put_new(&key, Bytes::from_static(b"abc"), None)
            .await
            .unwrap();
        let all = ByteRange::new(0, 3).unwrap();
        assert_eq!(&store.get_range(&key, all).await.unwrap()[..], b"abc");
        assert_eq!(Node::tier(hot).head("cas/held").await.unwrap().size, 3);
    };
    let drained = serving(&settings, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that a node whose hot tier names a lock file starts and serves with the reconciler
/// gated on it: the lock file is made and the node answers.
#[tokio::test]
async fn a_combined_node_with_a_lock_file_serves() {
    let node = Node::new("tscombined-lock");
    node.activate().await;
    let lock = node.dir.path().join("instance.lock");
    let settings = node.settings(&format!("lock_file = \"{}\"", lock.display()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let work = |addr: SocketAddr| async move {
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-write"));
    };
    let drained = serving(&settings, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert!(lock.is_file());
}

/// Asserts the refusals of a start, each with the exit status of its reason: flags that cannot go
/// together (a usage staleness not above the lease), a tier that cannot be opened (1, and the
/// cache directory is not made), a cache directory that cannot be opened (1), a file system that
/// fails the check of the layout (3), and a budget over the file system (2); and that a budget
/// exactly the room there is starts.
#[tokio::test]
async fn a_combined_node_that_cannot_start_exits_with_the_status_of_the_reason() {
    let nothing = |_addr: SocketAddr| async {};
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);

    let node = Node::new("tscombined-refused");
    let late = node.settings("usage_stale_after = \"10s\"");
    let why = serving(&late, disk.clone(), nothing).await.unwrap_err();
    assert!(why.to_string().contains("--usage-stale-after"), "{why}");
    assert!(!node.cache_dir().exists());

    let node = Node::new("tscombined-tier");
    std::fs::write(node.hot(), b"a file, not a directory").unwrap();
    let settings = node.settings("");
    let why = serving(&settings, disk.clone(), nothing).await.unwrap_err();
    assert_eq!(exit_status(&why), 1, "{why}");
    assert!(!node.cache_dir().exists());

    let node = Node::new("tscombined-cache-path");
    let in_the_way = node.dir.path().join("in-the-way");
    std::fs::write(&in_the_way, b"a file").unwrap();
    let text = node.text("");
    let text = text.replace(
        &node.cache_dir().display().to_string(),
        &in_the_way.display().to_string(),
    );
    let blocked = settings_from_text(&text, QUICK).unwrap();
    let blocked = blocked.into_combined().unwrap();
    let why = serving(&blocked, disk, nothing).await.unwrap_err();
    assert!(why.to_string().contains("opening the local cache"), "{why}");
    assert_eq!(exit_status(&why), 1);

    let node = Node::new("tscombined-disk");
    let settings = node.settings("");
    let flawed = Arc::new(FakeDisk::new());
    flawed.set(Quirk::OneLink);
    let disk: Arc<dyn Disk> = flawed;
    let why = serving(&settings, disk, nothing).await.unwrap_err();
    assert!(why.to_string().contains("st_nlink"), "{why}");
    assert_eq!(exit_status(&why), 3);

    // Each start has a node of its own: a start that failed after it opened its proxy leaves the
    // heartbeat of the dropped proxy to end on its own, and it must not beat over the
    // registration of the next start.
    let node = Node::new("tscombined-small");
    let settings = node.settings("");
    let small = Arc::new(FakeDisk::new());
    small.set_space(1_165_083, 1_165_083);
    let disk: Arc<dyn Disk> = small;
    let why = serving(&settings, disk, nothing).await.unwrap_err();
    assert!(why.to_string().contains("local_cache.budget"), "{why}");
    assert_eq!(exit_status(&why), 2);

    let node = Node::new("tscombined-enough");
    let settings = node.settings("");
    let enough = Arc::new(FakeDisk::new());
    enough.set_space(1_165_084, 1_165_084);
    let disk: Arc<dyn Disk> = enough;
    let drained = serving(&settings, disk, nothing).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts what `--check-config` finds in the settings of a node with a cache: a good file
/// passes, and a key file that is not there is refused, as for a storage node.
#[test]
fn check_config_of_a_combined_node_reads_its_key_files() {
    let node = Node::new("tscombined-check");
    let good = settings_from_text(&node.text(""), QUICK).unwrap();
    assert!(matches!(good, Settings::Combined(_)));
    assert!(checked(&good).is_ok());
    let missing = node.dir.path().join("absent.key");
    assert!(!missing.exists());
    let gone = node.text("").replace(
        &node.key.display().to_string(),
        &missing.display().to_string(),
    );
    let gone = settings_from_text(&gone, QUICK).unwrap();
    let why = checked(&gone).unwrap_err();
    assert!(
        why.to_string().contains(&missing.display().to_string()),
        "{why}"
    );
    assert!(!node.cache_dir().exists());
}
