//! The `fs` interface of the one process: a build node's start that gets the mount (taken, adopted
//! or made, in that order) before the sockets of its fronts, and a stop that leaves it after they
//! drain. Over a stand-in for the kernel's end of `/dev/fuse` (`fakedev.rs`), and over a real
//! mount where one can be made.

use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use objstore_s3::{ByteRange as ObjectRange, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::Service;
use crate::server::Drained;
use crate::server::cache_cli::{Imaging, OnSystem, serve_cache_on};
use crate::server::cache_cli_tests::{Node, QUICK, client, probe};
use crate::server::cli::{Addresses, Error, exit_status};
use crate::server::config::settings_from_text;
use crate::server::config_cache::CacheSettings;
use crate::server::handoff::testing::within;
use crate::server::local_cache::{Disk, SystemDisk};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::fakedev::{self, FakeDevice};
use crate::server::treefs::fills::Reach;
use crate::server::treefs::handover::{HandoverError, Offer};
use crate::server::treefs::init_record::{self, Record};
use crate::server::treefs::ino;
use crate::server::treefs::launch::Platform;
use crate::server::treefs::run::{Kernel, Prepared, Running};
use crate::server::treefs::sd::{FUSE_NAME, Manager, Stored};
use crate::server::treefs::start::StartError;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{FakeManager, Rig, sample, until, with_fuse};
use crate::{Backend, ByteRange, Error as StoreError, Listed, MemoryBackend, Meta, Mono};

/// The connection number the tests' tables of mounts give the mount.
const CONNECTION: &str = "77";

/// A build node whose file has an `fs` interface, in a scratch directory.
struct FsNode {
    node: Node,
    dir: TestDir,
}

impl FsNode {
    fn new(name: &str) -> Self {
        Self {
            node: Node::new(name),
            dir: TestDir::under(std::env::temp_dir(), &format!("{name}-fs")),
        }
    }

    fn mountpoint(&self) -> PathBuf {
        self.dir.path().join("mnt")
    }

    fn runtime(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    /// The file of the node with `server` in `[server]` and `params` among the params of its `fs`
    /// interface; the S3 front listens on `listen`.
    fn text(&self, server: &str, params: &str, listen: &str) -> String {
        self.text_with(server, params, listen, "")
    }

    /// [`FsNode::text`] with `local` among the keys of `[local_cache]`.
    fn text_with(&self, server: &str, params: &str, listen: &str, local: &str) -> String {
        let (uid, gid) = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        let fs = format!(
            "[[interface]]\ntype = \"fs\"\n[interface.params]\nmountpoint = {:?}\nruntime_dir = {:?}\nclient_group = \"{gid}\"\nowner = \"{uid}:{gid}\"\n{params}\n\n",
            self.mountpoint().display().to_string(),
            self.runtime().display().to_string(),
        );
        format!("{fs}{}", self.node.text(server, local)).replace("127.0.0.1:0", listen)
    }

    /// The settings of [`FsNode::text`].
    fn settings(&self, server: &str, params: &str, listen: &str) -> CacheSettings {
        self.settings_with(server, params, listen, "")
    }

    /// The settings of [`FsNode::text_with`].
    fn settings_with(
        &self,
        server: &str,
        params: &str,
        listen: &str,
        local: &str,
    ) -> CacheSettings {
        let mut settings =
            settings_from_text(&self.text_with(server, params, listen, local), QUICK)
                .unwrap()
                .into_cache()
                .unwrap();
        // A mount the test makes is its user's alone: `allow_other` needs the system's consent.
        if let Some(fs) = &mut settings.fs {
            fs.allow_other = false;
        }
        settings
    }

    /// The kernel's files as a test makes them: a table of mounts that shows a mount of this file
    /// system at the mount point (`shown`), or nothing, and a directory of connections.
    fn kernel(&self, shown: bool) -> Kernel {
        let table = self.dir.path().join("mountinfo");
        let info = if shown {
            format!(
                "36 24 0:{CONNECTION} / {} rw - fuse.tierstore-fs x rw\n",
                self.mountpoint().display()
            )
        } else {
            String::new()
        };
        std::fs::write(&table, info).unwrap();
        let connections = self.dir.path().join("connections");
        std::fs::create_dir_all(connections.join(CONNECTION)).unwrap();
        Kernel {
            mountinfo: table,
            connections,
            // The stand-in for the device is a socket.
            device: |_| Ok(()),
        }
    }

    /// Whether the mount the kernel's table shows was aborted.
    fn aborted(&self) -> bool {
        self.dir
            .path()
            .join("connections")
            .join(CONNECTION)
            .join("abort")
            .exists()
    }
}

/// A platform with the manager `manager`, the descriptor `stored` kept under the name `fuse`, and
/// `kernel`.
fn platform(manager: &FakeManager, stored: Option<i32>, kernel: Kernel) -> Platform {
    Platform {
        manager: Manager::new(Some(manager.socket().as_os_str())),
        stored: stored
            .into_iter()
            .map(|fd| Stored {
                name: FUSE_NAME.to_owned(),
                fd,
            })
            .collect(),
        kernel,
        clock: Arc::new(Mono::new()),
    }
}

/// A process of the one-process layout, started in a task of its own.
struct Started {
    /// What it serves, once it does.
    bound: oneshot::Receiver<Addresses>,
    /// Stops it.
    stop: oneshot::Sender<()>,
    /// How it ended.
    ended: JoinHandle<Result<Drained, Error>>,
}

/// Starts a build node with `settings` over `upstream`, with `system`.
fn start(settings: CacheSettings, upstream: Arc<dyn Backend>, system: OnSystem) -> Started {
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let ended = tokio::spawn(async move {
        let stopped = async move {
            let _ = stopped.await;
        };
        let told = move |at| drop(told.send(at));
        let imaging = Imaging::default();
        serve_cache_on(&settings, (upstream, disk), system, stopped, told, imaging).await
    });
    Started { bound, stop, ended }
}

/// Starts a build node with `settings`, whose file has an `fs` interface, over `upstream`, on
/// `platform`.
fn start_on(settings: CacheSettings, upstream: Arc<dyn Backend>, platform: Platform) -> Started {
    let system = OnSystem {
        service: Some(Service::on(settings.fs.clone(), platform)),
    };
    start(settings, upstream, system)
}

/// A memory upstream.
fn memory() -> Arc<dyn Backend> {
    Arc::new(MemoryBackend::new())
}

/// Leaves in the runtime directory of `settings` what a process that served a mount leaves: the
/// table of tree numbers and the record of the `INIT` handshake, of a kernel that can send requests
/// again (so a process that adopts the connection starts no watch).
fn left_state(settings: &FsSettings) {
    std::fs::create_dir_all(&settings.runtime_dir).unwrap();
    std::fs::create_dir_all(&settings.cache_path).unwrap();
    let _table = TreeTable::create(&settings.runtime_dir).unwrap();
    let record = Record::first(fakedev::outcome(true), "mount-1".to_owned());
    init_record::write(&settings.runtime_dir, &record).unwrap();
}

/// A running `tierstore fs` process of the two-process layout over a stand-in for the kernel, on
/// the paths of `settings`, answering `take-fs 1` on its hand-over socket. Its fill socket is a
/// stand-in for the old `tierstore`'s, which answers it.
struct OldFs {
    running: Arc<Running>,
    kernel: FakeDevice,
    serving: JoinHandle<Result<(), HandoverError>>,
    _fill: Rig,
}

fn old_fs(settings: &FsSettings) -> OldFs {
    left_state(settings);
    let fill = Rig::new("tsfs-one-oldfill", sample());
    let settings = FsSettings {
        fill_socket: Some(fill.dir.path().join("fill.sock")),
        ..settings.clone()
    };
    let (device, kernel) = fakedev::pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running =
        Running::adopt((&settings, &Reach::Socket), prepared, device, (None, false)).unwrap();
    let running = Arc::new(running);
    let offer = Offer::bind(
        &settings.handoff_socket,
        &settings.mountpoint,
        &settings.control_socket,
        settings.handoff_wait,
    )
    .unwrap();
    let serving = tokio::spawn(offer.serve(Arc::clone(&running)));
    OldFs {
        running,
        kernel,
        serving,
        _fill: fill,
    }
}

/// Asks `kernel` for the attributes of the root as request `unique`, and asserts that it is
/// answered; a notification sent meanwhile (a kernel asked to send requests again) is passed over.
fn assert_answers(kernel: &FakeDevice, unique: u64) {
    kernel.getattr(unique, ino::ROOT);
    assert_eq!(reply_to(kernel), (unique, 0));
}

/// The request and the error of the next reply `kernel` gets, past a notification sent meanwhile.
fn reply_to(kernel: &FakeDevice) -> (u64, i32) {
    loop {
        let reply = kernel.reply();
        if reply.unique != 0 {
            return (reply.unique, reply.error);
        }
    }
}

/// Asserts that nothing answers `kernel`: a request sent now gets no reply.
fn assert_unanswered(kernel: &FakeDevice, unique: u64) {
    kernel.getattr(unique, ino::ROOT);
    kernel.assert_silent();
}

/// Asserts that `manager` was told `want`, in order: its thread reads what it is sent, so the test
/// waits for the texts to come, and fails if they never do or others come instead.
fn told(manager: &FakeManager, want: &[&str]) {
    until(|| manager.texts().len() >= want.len());
    assert_eq!(manager.texts(), want);
}

/// Stops `started` and gives how it ended.
async fn stop(started: Started) -> Result<Drained, Error> {
    started.stop.send(()).unwrap();
    within("the node ends", started.ended).await.unwrap()
}

/// Asserts the order of a start: a running instance that answers on the hand-over socket gives the
/// mount, and the descriptor the service manager kept is not served and the mount the table shows
/// is not touched; with no running instance the kept descriptor is served, and the mount the table
/// shows is still not touched; with neither, the mount the table shows (which no process serves)
/// is aborted, and the mount is made afresh. Catches a start that adopts or mounts while an
/// instance holds the mount, or mounts while the store holds it.
#[test]
fn a_start_takes_before_it_adopts_and_adopts_before_it_mounts() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let node = FsNode::new("tsfs-one-order");
    let settings = || node.settings("", "", "127.0.0.1:0");
    let fs = settings().fs.unwrap();
    let (stored, kept) = fakedev::pair();
    runtime.block_on(async {
        // A running instance, a kept descriptor and a mount in the table: the mount is taken.
        let old = old_fs(&fs);
        let manager = FakeManager::start(&node.dir.path().join("taker.sock"));
        let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(true));
        let mut taker = start_on(settings(), memory(), on_platform);
        let _addresses = within("the taker serves", &mut taker.bound).await.unwrap();
        within("the old instance gives the mount", old.serving)
            .await
            .unwrap()
            .unwrap();
        assert_answers(&old.kernel, 1);
        assert_unanswered(&kept, 2);
        assert!(!node.aborted());
        told(
            &manager,
            &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
        );
        stop(taker).await.unwrap();
        drop(old.running);

        // No running instance: the kept descriptor is served; the request it holds is answered.
        let manager = FakeManager::start(&node.dir.path().join("adopter.sock"));
        let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(true));
        let mut adopter = start_on(settings(), memory(), on_platform);
        let _addresses = within("the adopter serves", &mut adopter.bound)
            .await
            .unwrap();
        assert_eq!(reply_to(&kept), (2, 0));
        assert!(!node.aborted());
        told(&manager, &["READY=1"]);
        stop(adopter).await.unwrap();
    });
    // Neither: the mount the table shows is aborted, and the mount is made.
    with_fuse(|| {
        runtime.block_on(async {
            let manager = FakeManager::start(&node.dir.path().join("mounter.sock"));
            let on_platform = platform(&manager, None, node.kernel(true));
            let _detached = Detached(node.mountpoint());
            let mut mounter = start_on(settings(), memory(), on_platform);
            let _addresses = within("the mounter serves", &mut mounter.bound)
                .await
                .unwrap();
            assert!(node.aborted());
            assert!(mounted(&node.mountpoint()));
            told(
                &manager,
                &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
            );
            stop(mounter).await.unwrap();
            assert!(mounted(&node.mountpoint()), "the store keeps the mount");
        });
    });
}

/// Whether the system's table of mounts shows a mount at `mountpoint`.
fn mounted(mountpoint: &Path) -> bool {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    info.lines()
        .any(|line| line.contains(mountpoint.to_str().unwrap()))
}

/// Detaches, when dropped, a mount that a test made at the path and left for the manager to
/// keep, without waiting for a server; a failed assertion before the end leaves no mount behind.
struct Detached(PathBuf);

impl Drop for Detached {
    fn drop(&mut self) {
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(&self.0)
            .status();
    }
}

/// Asserts the start of release M on a node where a `tierstore fs` process of the two-process
/// layout serves the mount: it takes the mount from that process, which ends, and serves it; it
/// never aborts or detaches the mount, which the kernel's table shows all along; and a request the
/// kernel holds meanwhile is answered by the new process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_in_m_with_a_live_old_fs_process_takes_from_it_and_never_detaches() {
    let node = FsNode::new("tsfs-one-old");
    let settings = node.settings("", "", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    let old = old_fs(&fs);
    assert_answers(&old.kernel, 1);
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let mut new = start_on(settings, memory(), on_platform);
    within("the old process gives the mount", old.serving)
        .await
        .unwrap()
        .unwrap();
    let addresses = within("the new process serves", &mut new.bound)
        .await
        .unwrap();
    assert_answers(&old.kernel, 2);
    assert!(!node.aborted());
    told(
        &manager,
        &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
    );
    // The new process answers on the hand-over socket in its turn, and serves S3.
    assert!(fs.handoff_socket.exists());
    let (status, _) = probe(addresses.s3, "/healthz").await;
    assert_eq!(status, 200);
    stop(new).await.unwrap();
    assert!(!fs.handoff_socket.exists());
    drop(old.running);
}

/// Asserts the start that switches a node from two processes to one: the old `tierstore` (S3, no
/// mount) and the old `tierstore fs` both run, and the new process, on a file with an `fs`
/// interface and the same `server.handoff_socket`, takes the mount from the fs process and the S3
/// sockets from the other. Both old processes end; the new one serves the kernel's requests at the
/// old addresses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_start_connects_to_both_old_processes_and_takes_the_mount_from_the_fs_one() {
    let node = FsNode::new("tsfs-one-switch");
    let control = node.dir.path().join("control.sock");
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"",
        control.display().to_string()
    );
    // The old S3 process: the node's file of today, with no fs interface.
    let old_settings = settings_from_text(&node.node.text(&server, ""), QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let mut old_s3 = start(old_settings, memory(), OnSystem::default());
    let old_bound = within("the old S3 process serves", &mut old_s3.bound)
        .await
        .unwrap();
    let new_settings = node.settings(&server, "", &old_bound.s3.to_string());
    let fs = new_settings.fs.clone().unwrap();
    let old = old_fs(&fs);
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(true));
    let mut new = start_on(new_settings, memory(), on_platform);
    within("the old fs process gives the mount", old.serving)
        .await
        .unwrap()
        .unwrap();
    let new_bound = within("the new process serves", &mut new.bound)
        .await
        .unwrap();
    assert_eq!(new_bound.s3, old_bound.s3);
    let drained = within("the old S3 process ends", old_s3.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert_answers(&old.kernel, 1);
    assert!(!node.aborted());
    told(
        &manager,
        &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
    );
    stop(new).await.unwrap();
    drop(old.running);
}

/// Asserts that a running instance that accepts on the hand-over socket and never answers fails
/// the start, after `params.handoff_wait`, by name, and that nothing was taken, adopted, aborted,
/// detached or bound: the kept descriptor's request is not answered, the mount the table shows is
/// not aborted, the service manager is told nothing, and no address is said.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_instance_that_never_answers_fails_the_start_and_detaches_nothing() {
    let node = FsNode::new("tsfs-one-mute");
    let settings = node.settings("", "handoff_wait = \"1s\"", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    left_state(&fs);
    let mute = tokio::net::UnixListener::bind(&fs.handoff_socket).unwrap();
    let held = tokio::spawn(async move {
        let (stream, _) = mute.accept().await.unwrap();
        // Holds the connection and says nothing.
        std::future::pending::<()>().await;
        drop(stream);
    });
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(true));
    let started = start_on(settings, memory(), on_platform);
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "hand-over: no end of the hand-over at {} in time",
            fs.handoff_socket.display()
        )
    );
    assert_eq!(exit_status(&error), 1);
    assert!(started.bound.await.is_err(), "no address was said");
    assert_unanswered(&kept, 1);
    assert!(!node.aborted());
    assert!(manager.texts().is_empty(), "{:?}", manager.texts());
    held.abort();
}

/// A store whose reads wait for the test once it is armed: the request of an S3 client that reaches
/// it is in flight until the test lets it go.
struct Held {
    inner: MemoryBackend,
    armed: AtomicBool,
    reached: Notify,
    let_go: Notify,
}

impl Held {
    /// Holds the first read after the store was armed.
    async fn hold(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.let_go.notified().await;
        }
    }
}

#[async_trait]
impl Backend for Held {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), StoreError> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, StoreError> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, StoreError> {
        self.hold().await;
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, StoreError> {
        self.hold().await;
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, StoreError> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, StoreError> {
        self.inner.list_page(prefix, start_after, max_keys).await
    }
}

/// Asserts the stop order: at a stop the fronts drain first and the mount is served throughout (a
/// request of the kernel is answered while an S3 request is still in flight), and only once they
/// have drained do the readers stop, leaving the mount to the service manager's store (nothing is
/// taken out of it, and a request is no longer answered). Catches a stop that leaves the mount
/// before the drain, which fails the jobs whose fills wait for the upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fronts_drain_before_the_mount_is_left() {
    let node = FsNode::new("tsfs-one-drain");
    // With the put directory, whose puts are calls of the cache too.
    let put = format!(
        "[interface.params.put]\nenabled = true\nprefixes = [\"cas/b/\"]\nstage_dir = {:?}",
        node.dir.path().join("stage").display().to_string()
    );
    let settings = node.settings("", &put, "127.0.0.1:0");
    assert!(settings.fs.as_ref().unwrap().put.is_some());
    let fs = settings.fs.clone().unwrap();
    left_state(&fs);
    let held = Arc::new(Held {
        inner: MemoryBackend::new(),
        armed: AtomicBool::new(false),
        reached: Notify::new(),
        let_go: Notify::new(),
    });
    held.inner
        .put("cas/held", Bytes::from_static(b"held"))
        .await
        .unwrap();
    held.armed.store(true, Ordering::SeqCst);
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
    let upstream: Arc<dyn Backend> = held.clone();
    let mut started = start_on(settings, upstream, on_platform);
    let addresses = within("the node serves", &mut started.bound).await.unwrap();
    let s3 = client(addresses.s3);
    let reading = tokio::spawn(async move {
        let key = ObjectKey::new("cas/held").unwrap();
        s3.get_range(&key, ObjectRange::new(0, 4).unwrap()).await
    });
    within("the read reaches the upstream", held.reached.notified()).await;
    started.stop.send(()).unwrap();
    // The fronts drain, waiting for the read: the mount is served.
    assert_answers(&kept, 1);
    assert!(!started.ended.is_finished());
    held.let_go.notify_one();
    let read = within("the read ends", reading).await.unwrap().unwrap();
    assert_eq!(read, b"held".to_vec());
    within("the node ends", started.ended)
        .await
        .unwrap()
        .unwrap();
    assert_unanswered(&kept, 2);
    told(&manager, &["READY=1"]);
}

/// Asserts the update of a node of the one-process layout: a second process on the same file
/// takes the mount from the first (on the runtime directory's hand-over socket) and then its S3
/// sockets (on `server.handoff_socket`); the first serves S3 until then, drains and ends with
/// success, and never serves the mount again: it takes the descriptor out of its service manager's
/// store when it gives the mount up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_process_takes_the_mount_then_the_sockets_of_the_first() {
    let node = FsNode::new("tsfs-one-update");
    let control = node.dir.path().join("control.sock");
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"",
        control.display().to_string()
    );
    let first_settings = node.settings(&server, "", "127.0.0.1:0");
    let fs = first_settings.fs.clone().unwrap();
    left_state(&fs);
    let (stored, kernel) = fakedev::pair();
    let first_manager = FakeManager::start(&node.dir.path().join("first.sock"));
    let first_platform = platform(&first_manager, Some(stored.as_raw_fd()), node.kernel(false));
    let mut first = start_on(first_settings, memory(), first_platform);
    let first_bound = within("the first serves", &mut first.bound).await.unwrap();
    assert_answers(&kernel, 1);

    let second_settings = node.settings(&server, "", &first_bound.s3.to_string());
    let second_manager = FakeManager::start(&node.dir.path().join("second.sock"));
    let second_platform = platform(&second_manager, None, node.kernel(false));
    let mut second = start_on(second_settings, memory(), second_platform);
    let second_bound = within("the second serves", &mut second.bound)
        .await
        .unwrap();
    assert_eq!(second_bound.s3, first_bound.s3);
    let drained = within("the first ends", first.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    // The first gave the mount up at `ready`: its store no longer keeps the descriptor.
    told(&first_manager, &["READY=1", "FDSTOREREMOVE=1\nFDNAME=fuse"]);
    told(
        &second_manager,
        &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
    );
    assert_answers(&kernel, 2);
    assert_eq!(init_record::read(&fs.runtime_dir).unwrap().generation, 3);
    stop(second).await.unwrap();
    drop(stored);
}

/// Asserts that a mount that cannot be served on stops the node: a restart's connection on a
/// kernel that cannot send requests again, which keeps showing requests no one answers past
/// `params.recover_wait`, is aborted and taken out of the store, the fronts drain as at a signal
/// with no signal, and the start ends in failure (status 1), so that the unit starts again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_whose_requests_stay_unanswered_stops_the_node_in_failure() {
    let node = FsNode::new("tsfs-one-hung");
    let settings = node.settings("", "recover_wait = \"1s\"", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    left_state(&fs);
    // A kernel that cannot send requests again.
    let record = Record::first(fakedev::outcome(false), "mount-1".to_owned());
    init_record::write(&fs.runtime_dir, &record).unwrap();
    let kernel = node.kernel(true);
    std::fs::write(kernel.connections.join(CONNECTION).join("waiting"), "2\n").unwrap();
    let (stored, _kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), kernel);
    let started = start_on(settings, memory(), on_platform);
    let error = within("the node ends", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the connection was aborted: requests the process before read stayed unanswered"
    );
    assert_eq!(exit_status(&error), 1);
    assert!(node.aborted());
    told(&manager, &["READY=1", "FDSTOREREMOVE=1\nFDNAME=fuse"]);
}

/// Asserts that a start that serves the mount and cannot then answer on the hand-over socket leaves
/// the mount as a stop does (to the store) and fails by name before any front is bound, and the
/// manager is never told the unit is ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_cannot_bind_the_hand_over_socket_leaves_the_mount_and_fails() {
    let node = FsNode::new("tsfs-one-nobind");
    let settings = node.settings("", "", "127.0.0.1:0");
    let fs = settings.fs.clone().unwrap();
    left_state(&fs);
    // A directory where the socket would be: no instance answers there, and none can be bound.
    std::fs::create_dir_all(fs.handoff_socket.join("in-the-way")).unwrap();
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
    let started = start_on(settings, memory(), on_platform);
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("hand-over: removing the old hand-over socket: "),
        "{error}"
    );
    assert!(started.bound.await.is_err(), "no address was said");
    // The connection was served (the kernel was asked to send its requests again) and then left.
    assert_eq!(kept.reply().unique, 0);
    assert_unanswered(&kept, 1);
    assert!(manager.texts().is_empty(), "{:?}", manager.texts());
}

/// Asserts that a build node whose file has no `fs` interface mounts nothing and tells the service
/// manager it is ready once its fronts serve; and that a manager that cannot be told is said on
/// stderr and the node serves all the same. On one thread, which the capture of stderr is for.
#[tokio::test]
async fn a_build_node_says_ready_to_its_service_manager_with_or_without_a_mount() {
    let node = FsNode::new("tsfs-one-ready");
    let settings = settings_from_text(&node.node.text("", ""), QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, None, node.kernel(false));
    let system = OnSystem {
        service: Some(Service::on(None, on_platform)),
    };
    let mut started = start(settings, memory(), system);
    let addresses = within("the node serves", &mut started.bound).await.unwrap();
    told(&manager, &["READY=1"]);
    let (status, _) = probe(addresses.s3, "/healthz").await;
    assert_eq!(status, 200);
    stop(started).await.unwrap();
    assert!(!node.mountpoint().exists());

    let log = crate::server::treefs::testing::Logged::default();
    let guard = crate::server::treefs::testing::logged(&log, tracing::Level::WARN);
    let nobody = node.dir.path().join("nobody-home");
    let system = OnSystem {
        service: Some(Service::on(
            None,
            Platform {
                manager: Manager::new(Some(nobody.as_os_str())),
                stored: Vec::new(),
                kernel: node.kernel(false),
                clock: Arc::new(Mono::new()),
            },
        )),
    };
    let settings = settings_from_text(&node.node.text("", ""), QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let mut started = start(settings, memory(), system);
    let addresses = within("the node serves", &mut started.bound).await.unwrap();
    let (status, _) = probe(addresses.s3, "/healthz").await;
    assert_eq!(status, 200);
    stop(started).await.unwrap();
    drop(guard);
    assert!(
        log.text()
            .contains("tierstore: cannot tell the service manager it is ready: "),
        "{}",
        log.text()
    );
}

/// Asserts the exit status of a start whose mount was refused: 3 for a cache directory on a stacked
/// file system, as `tierstore fs` says, and 1 for any other refusal.
#[test]
fn a_refused_mount_has_the_exit_status_of_tierstore_fs() {
    let stacked: Error = Box::new(StartError::Stacked {
        path: "/var/cache".to_owned(),
        name: "overlayfs",
    });
    assert_eq!(exit_status(&stacked), 3);
    let aborted: Error = Box::new(StartError::Aborted);
    assert_eq!(exit_status(&aborted), 1);
}

/// Asserts that a start that served the mount and then cannot bind the address of its S3 front
/// fails by name, binds nothing, and leaves the mount it served (it reads the connection no more,
/// and the descriptor stays in the service manager's store, which is told nothing more), as a stop
/// leaves it, for the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_s3_address_is_taken_leaves_the_mount_it_served() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let node = FsNode::new("tsfs-one-taken");
    let settings = node.settings("", "", &address);
    let fs = settings.fs.clone().unwrap();
    left_state(&fs);
    let (stored, kept) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on_platform = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
    let started = start_on(settings, memory(), on_platform);
    let error = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains(&address), "{error}");
    assert!(started.bound.await.is_err(), "no address was said");
    // The connection was served (the kernel was asked to send its requests again) and then left.
    assert_eq!(kept.reply().unique, 0);
    assert_unanswered(&kept, 1);
    assert!(!fs.handoff_socket.exists());
    assert!(manager.texts().is_empty(), "{:?}", manager.texts());
    drop(taken);
}

#[path = "switch_tests.rs"]
mod switch;
