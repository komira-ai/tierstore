//! The `fs` interface of the one process as macOS serves it (`interface.rs`): a build node's start
//! that serves the NFS front, from a core that calls the node's own cache, and mounts its export
//! before the sockets of its fronts; a restart that finds the mount and does not make it again; a
//! stop that unmounts once the fronts have drained; and the hand-over to the next image of the
//! program, whose offer carries the NFS listener and the control socket. Over the fake mount
//! command of `run_tests.rs` (no mount is made, no program is run) and real loopback sockets, on
//! every system; no assertion is about time.

use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use nfsserve::tcp::NFSTcpListener;
use objstore_s3::{ByteRange as ObjectRange, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use tracing::Level;
use tracing::instrument::WithSubscriber as _;

use super::super::boot::BootSource;
use super::super::fs::TreeNfs;
use super::super::interface::NfsService;
use super::super::wire_client::{Client, Dec, Enc, NFS, READ};
use super::handoff_tests::{getattr, lookup, me};
use super::tests::{Recorder, ask, closed, euid};
use super::{Serve, handle};
use crate::server::Drained;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::cache_cli::next_image::NextImage;
use crate::server::cache_cli::{Imaging, OnSystem, serve_cache_on};
use crate::server::cache_cli_tests::{Node, QUICK, probe};
use crate::server::cli::{Addresses, Error};
use crate::server::config::settings_from_text;
use crate::server::config_cache::CacheSettings;
use crate::server::handoff::testing::within;
use crate::server::local_cache::{Disk, SystemDisk};
use crate::server::treefs::testing::{Built, Logged, Node as Tree};
use crate::{Backend, ByteRange, Error as StoreError, Listed, MemoryBackend, Meta};

/// The program of the mount, and of the listing of the mounts and the unmount, as the fake
/// runner records them.
const MOUNT_NFS: &str = "/sbin/mount_nfs";
const LISTING: &str = "/sbin/mount";
const UMOUNT: &str = "/sbin/umount";

/// The bytes of the one file of the tree the tests attach.
const TOOL: &[u8] = b"tool bytes";

/// A build node whose file has an `fs` interface served over NFS, in scratch directories, with the
/// identity of its boot in a file (no command is run for it).
struct NfsNode {
    node: Node,
    dir: TestDir,
    control: PathBuf,
}

impl NfsNode {
    fn new(name: &str) -> Self {
        let (dir, control) =
            TestDir::for_socket(&[&std::env::temp_dir()], &format!("{name}-fs"), "ctl.sock");
        std::fs::write(dir.path().join("boot_id"), "boot-1\n").unwrap();
        Self {
            node: Node::new(name),
            dir,
            control,
        }
    }

    fn mountpoint(&self) -> PathBuf {
        self.dir.path().join("mnt")
    }

    /// The settings of the node, its S3 front on `s3` and its NFS front on `nfs`.
    fn settings(&self, s3: &str, nfs: &str) -> CacheSettings {
        let (uid, gid) = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        let fs = format!(
            "[[interface]]\ntype = \"fs\"\n[interface.params]\nmountpoint = {:?}\nruntime_dir = {:?}\ncontrol_socket = {:?}\nclient_group = \"{gid}\"\nowner = \"{uid}:{gid}\"\n[interface.params.nfs]\nlisten = \"{nfs}\"\n\n",
            self.mountpoint().display().to_string(),
            self.dir.path().join("run").display().to_string(),
            self.control.display().to_string(),
        );
        let node = self.node.text("", "").replace("127.0.0.1:0", s3);
        settings_from_text(&format!("{fs}{node}"), QUICK)
            .unwrap()
            .into_cache()
            .unwrap()
    }

    /// The NFS front of `settings` over `runner`, served by `serve`.
    fn service(&self, settings: &CacheSettings, runner: &Arc<Recorder>, serve: Serve) -> OnSystem {
        let boot = BootSource::of(
            &self.dir.path().join("boot_id"),
            "/nonexistent/boot-id",
            &[],
        );
        let runner: Arc<Recorder> = Arc::clone(runner);
        let nfs = NfsService::on(settings.fs.clone().unwrap(), (runner, euid(), boot), serve);
        OnSystem {
            #[cfg(target_os = "linux")]
            service: None,
            nfs: Some(nfs),
        }
    }
}

/// The programs `runner` ran, in order.
fn programs(runner: &Recorder) -> Vec<String> {
    runner
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect()
}

/// An upstream that holds the tree of one file, `tool`, whose root manifest is the digest given,
/// each object at `cas/l/<sha256>`, as the trees' fills read them.
async fn with_tree() -> (MemoryBackend, String) {
    let tool = Tree::File {
        content: TOOL.to_vec(),
        exec: false,
        packed: false,
    };
    let built = Built::of(vec![("tool", tool)]);
    let upstream = MemoryBackend::new();
    for (sha, bytes) in built.manifests.iter().chain(&built.blobs) {
        let key = format!("cas/l/{sha}");
        upstream
            .put(&key, Bytes::from(bytes.clone()))
            .await
            .unwrap();
    }
    (upstream, built.root)
}

/// A node started in a task: what it said once it serves (its addresses, and the programs its
/// mount runner had run by then), the stop, and its end.
struct Started {
    bound: oneshot::Receiver<(Addresses, Vec<String>)>,
    stop: oneshot::Sender<()>,
    ended: JoinHandle<Result<Drained, Error>>,
}

/// Starts a node of `settings` over `upstream`, with `system`; `runner` is its mount runner.
/// `next`, when given, is told at the stop that it is a hand-over to the next image; `handed` is
/// the offer the image before handed over.
fn start(
    settings: CacheSettings,
    upstream: Arc<dyn Backend>,
    (system, runner): (OnSystem, Arc<Recorder>),
    next: Option<Arc<NextImage>>,
    handed: Option<OwnedFd>,
) -> Started {
    start_logged(settings, upstream, (system, runner), (next, handed), None)
}

/// [`start`], with what the node says at WARN and above written to `log`, whichever thread says it.
fn start_logged(
    settings: CacheSettings,
    upstream: Arc<dyn Backend>,
    (system, runner): (OnSystem, Arc<Recorder>),
    (next, handed): (Option<Arc<NextImage>>, Option<OwnedFd>),
    log: Option<&Logged>,
) -> Started {
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    start_over(
        settings,
        (upstream, disk),
        (system, runner),
        (next, handed),
        log,
    )
}

/// [`start_logged`] over `disk`, the file system under the node's cache.
fn start_over(
    settings: CacheSettings,
    (upstream, disk): (Arc<dyn Backend>, Arc<dyn Disk>),
    (system, runner): (OnSystem, Arc<Recorder>),
    (next, handed): (Option<Arc<NextImage>>, Option<OwnedFd>),
    log: Option<&Logged>,
) -> Started {
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel();
    let node = async move {
        let asking = next.clone();
        let stopped = async move {
            let _ = stopped.await;
            if let Some(next) = asking {
                next.ask();
            }
        };
        let told = move |at| drop(told.send((at, programs(&runner))));
        let imaging = Imaging {
            next: next.as_deref(),
            handed,
        };
        serve_cache_on(&settings, (upstream, disk), system, stopped, told, imaging).await
    };
    let ended = match log {
        Some(log) => tokio::spawn(node.with_subscriber(LineLog::new(log.clone(), Level::WARN))),
        None => tokio::spawn(node),
    };
    Started { bound, stop, ended }
}

/// Attaches the tree `root` on the control socket at `control`, off the runtime (the attach waits
/// for the cache's fill of the manifest), and says whether it was attached.
async fn attach(control: &Path, root: &str) -> bool {
    let (control, request) = (control.to_owned(), format!("attach {root} for run-1"));
    let answer = tokio::task::spawn_blocking(move || ask(&control, &request));
    within("the attach", answer).await.unwrap()["ok"] == serde_json::json!(true)
}

/// The handle of `tool` in the tree `root`, from the root of the export.
async fn tool_of(client: &mut Client, root: &str) -> Vec<u8> {
    let top = client.mount("/").await;
    let trees = lookup(client, &top, "trees").await;
    let tree = lookup(client, &trees, root).await;
    lookup(client, &tree, "tool").await
}

/// The status of a READ of the first bytes of `handle`, and whether the reply holds the bytes of
/// the tree's file.
async fn read(client: &mut Client, handle: &[u8]) -> (u32, bool) {
    let args = Enc::default().opaque(handle).u64(0).u32(100);
    let reply = client.call(NFS, READ, me(), args).await;
    let status = Dec::new(&reply.body).u32();
    let holds = reply.body.windows(TOOL.len()).any(|window| window == TOOL);
    (status, holds)
}

/// The loopback address of `port`.
fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Asserts the start of a build node whose file has an `fs` interface, as macOS serves it: the
/// export is mounted, as the user of the process, before the fronts serve (the runner had run the
/// listing and the mount when the node said it served), the core calls the node's cache (a tree
/// attached on the control socket is filled from the node's upstream, with no fill socket, and a
/// file of it reads through NFS), S3 serves beside it, and a stop unmounts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_on_macos_serves_the_fs_interface_over_nfs_from_the_nodes_cache() {
    let node = NfsNode::new("nfs-one-start");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let runner = Recorder::new(true);
    let (upstream, root) = with_tree().await;
    let system = node.service(&settings, &runner, handle);
    let mut started = start(
        settings,
        Arc::new(upstream),
        (system, Arc::clone(&runner)),
        None,
        None,
    );
    let (addresses, ran) = within("the node serves", &mut started.bound).await.unwrap();
    assert_eq!(ran, [LISTING, MOUNT_NFS]);
    assert_eq!(
        probe(addresses.s3, "/readyz").await,
        (200, "ready read-write".to_owned())
    );
    assert!(attach(&node.control, &root).await);
    let address = loopback(runner.mounted_port().unwrap());
    let mut client = Client::connect(address).await;
    let tool = tool_of(&mut client, &root).await;
    assert_eq!(read(&mut client, &tool).await, (0, true));
    drop(client);

    started.stop.send(()).unwrap();
    within("the node ends", started.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(programs(&runner), [LISTING, MOUNT_NFS, LISTING, UMOUNT]);
    closed(address).await;
}

/// Asserts that a start after a crash, with the mount the crashed process made still up, makes no
/// second mount: it lists the mounts, finds the export mounted from the address it serves on, and
/// serves the clients' mount there (a handle the client got before the crash is answered).
/// Catches a restart that mounts again over a mount the jobs hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_with_the_mount_up_makes_no_second_mount() {
    let node = NfsNode::new("nfs-one-restart");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let runner = Recorder::new(true);
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let system = node.service(&settings, &runner, handle);
    let mut first = start(
        settings,
        Arc::clone(&upstream),
        (system, Arc::clone(&runner)),
        None,
        None,
    );
    within("the first serves", &mut first.bound).await.unwrap();
    let port = runner.mounted_port().unwrap();
    let mut client = Client::connect(loopback(port)).await;
    let top = client.mount("/").await;
    drop(client);
    // The crash: the process is gone, and its mount is not unmounted.
    first.ended.abort();
    assert!(first.ended.await.unwrap_err().is_cancelled());
    closed(loopback(port)).await;
    assert!(runner.has_mount());

    let made = runner.calls().len();
    let settings = node.settings("127.0.0.1:0", &loopback(port).to_string());
    let system = node.service(&settings, &runner, handle);
    let mut second = start(
        settings,
        upstream,
        (system, Arc::clone(&runner)),
        None,
        None,
    );
    within("the restart serves", &mut second.bound)
        .await
        .unwrap();
    assert_eq!(programs(&runner)[made..], [LISTING]);
    let mut client = Client::connect(loopback(port)).await;
    assert_eq!(getattr(&mut client, &top).await, 0);
    drop(client);
    second.stop.send(()).unwrap();
    within("the restart ends", second.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(programs(&runner)[made..], [LISTING, LISTING, UMOUNT]);
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

/// Asserts the stop order: the fronts drain first and the NFS front answers throughout (a GETATTR
/// is answered, and nothing is unmounted, while an S3 read is still in flight), and only once they
/// have drained is the export unmounted, while the NFS front still answers (the runner's probe at
/// the unmount is answered). Catches a stop that unmounts before the drain, which fails the jobs
/// whose fills wait for the upstream, and one that stops serving NFS before it unmounts, which
/// leaves a busy mount no unmount clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_unmounts_after_the_fronts_drain() {
    let node = NfsNode::new("nfs-one-drain");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let runner = Recorder::new(true);
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
    let system = node.service(&settings, &runner, handle);
    let upstream: Arc<dyn Backend> = held.clone();
    let mut started = start(
        settings,
        upstream,
        (system, Arc::clone(&runner)),
        None,
        None,
    );
    let (addresses, _) = within("the node serves", &mut started.bound).await.unwrap();
    let address = loopback(runner.mounted_port().unwrap());
    let mut client = Client::connect(address).await;
    let top = client.mount("/").await;
    let s3 = crate::server::cache_cli_tests::client(addresses.s3);
    let reading = tokio::spawn(async move {
        let key = ObjectKey::new("cas/held").unwrap();
        s3.get_range(&key, ObjectRange::new(0, 4).unwrap()).await
    });
    within("the read reaches the upstream", held.reached.notified()).await;
    runner.probe_at_umount(std::net::TcpStream::connect(address).unwrap());
    started.stop.send(()).unwrap();
    // The fronts drain, waiting for the read: the NFS front answers, and nothing is unmounted.
    assert_eq!(getattr(&mut client, &top).await, 0);
    assert!(!programs(&runner).contains(&UMOUNT.to_owned()));
    assert!(!started.ended.is_finished());
    held.let_go.notify_one();
    let read = within("the read ends", reading).await.unwrap().unwrap();
    assert_eq!(read, b"held".to_vec());
    drop(client);
    within("the node ends", started.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(programs(&runner), [LISTING, MOUNT_NFS, LISTING, UMOUNT]);
    assert_eq!(runner.checked_at_umount(), Some(true));
}

/// Asserts the hand-over to the next image (SIGUSR2): the stop drains and leaves the mount up (no
/// unmount), and the offer it writes carries the NFS listener and the control socket with the S3
/// sockets and the lock. A client that connects to the NFS address once the old image is done
/// waits in the backlog of the handed listener; the next image, started from the offer, serves on
/// the same S3 address, finds the mount and makes none, and answers that client: a READ of the
/// handle of a file the old image served gives its bytes, from the attachment the next image made
/// again. Catches an offer that leaves the NFS listener out (the client's connection would be
/// refused, and the next image would bind a socket of its own) or the control socket, and a
/// hand-over that unmounts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_exec_offer_carries_the_nfs_listener_and_the_next_image_serves_on_it() {
    let node = NfsNode::new("nfs-one-exec");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let old_runner = Recorder::new(true);
    let (upstream, root) = with_tree().await;
    let upstream: Arc<dyn Backend> = Arc::new(upstream);
    let next = Arc::new(NextImage::default());
    let system = node.service(&settings, &old_runner, handle);
    let mut old = start(
        settings,
        Arc::clone(&upstream),
        (system, Arc::clone(&old_runner)),
        Some(Arc::clone(&next)),
        None,
    );
    let (old_addresses, _) = within("the old image serves", &mut old.bound)
        .await
        .unwrap();
    assert!(attach(&node.control, &root).await);
    let address = loopback(old_runner.mounted_port().unwrap());
    let mut client = Client::connect(address).await;
    let tool = tool_of(&mut client, &root).await;
    assert_eq!(read(&mut client, &tool).await, (0, true));
    drop(client);
    old.stop.send(()).unwrap();
    within("the old image ends", old.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(programs(&old_runner), [LISTING, MOUNT_NFS]);
    let program = || Ok(PathBuf::from("tierstore"));
    let image = next.image(program, ["tierstore".into()]).await.unwrap();
    let image = image.expect("a hand-over writes the offer");

    // Between the images: the listener is open, in the offer, and nothing accepts on it.
    let mut queued = within("the queued connection", Client::connect(address)).await;
    let new_runner = Recorder::mounted_at(&node.mountpoint());
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let system = node.service(&settings, &new_runner, handle);
    let mut new = start(
        settings,
        upstream,
        (system, Arc::clone(&new_runner)),
        None,
        Some(image.into_handed()),
    );
    let (new_addresses, ran) = within("the next image serves", &mut new.bound)
        .await
        .unwrap();
    assert_eq!(new_addresses.s3, old_addresses.s3);
    assert_eq!(ran, [LISTING]);
    assert_eq!(
        within("the queued read", read(&mut queued, &tool)).await,
        (0, true)
    );
    drop(queued);
    new.stop.send(()).unwrap();
    within("the next image ends", new.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(programs(&new_runner), [LISTING, LISTING, UMOUNT]);
}

/// Asserts that a start with an `fs` interface refuses an offer handed by an image that served no
/// NFS front (its file had no `fs` interface), before it lists or makes a mount: the offer is not
/// the one this file asks for, and the start fails; launchd starts the job again, cold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handed_offer_without_the_nfs_front_is_refused_before_anything_is_mounted() {
    let node = NfsNode::new("nfs-one-refused");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let next = Arc::new(NextImage::default());
    let unused = Recorder::new(true);
    let mut old = start(
        node.node.settings("", ""),
        Arc::clone(&upstream),
        (OnSystem::default(), unused),
        Some(Arc::clone(&next)),
        None,
    );
    let (old_addresses, _) = within("the old image serves", &mut old.bound)
        .await
        .unwrap();
    old.stop.send(()).unwrap();
    within("the old image ends", old.ended)
        .await
        .unwrap()
        .unwrap();
    let program = || Ok(PathBuf::from("tierstore"));
    let image = next.image(program, ["tierstore".into()]).await.unwrap();

    let runner = Recorder::new(true);
    let system = node.service(&settings, &runner, handle);
    let new = start(
        settings,
        upstream,
        (system, Arc::clone(&runner)),
        None,
        Some(image.unwrap().into_handed()),
    );
    let refused = within("the next image fails", new.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        format!(
            "hand-off: the running instance listens on [\"s3 {}\", \"lock {}\"], these settings name [\"s3 127.0.0.1:0\", \"nfs 127.0.0.1:0\"]",
            old_addresses.s3,
            node.node.cache_dir().join(".lock").display()
        )
    );
    assert!(runner.calls().is_empty());
}

/// Asserts that a start whose mount does not succeed fails before any socket of a front is bound:
/// the node never says it serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mount_that_fails_fails_the_start_before_the_fronts_bind() {
    let node = NfsNode::new("nfs-one-nomount");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let mountpoint = node.mountpoint();
    let runner = Recorder::new(false);
    let system = node.service(&settings, &runner, handle);
    let started = start(
        settings,
        Arc::new(MemoryBackend::new()),
        (system, Arc::clone(&runner)),
        None,
        None,
    );
    let failed = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        failed.to_string().starts_with(&format!(
            "mounting {} did not succeed in",
            mountpoint.display()
        )),
        "{failed}"
    );
    assert!(started.bound.await.is_err(), "the node never served");
    assert!(!programs(&runner).contains(&UMOUNT.to_owned()));
}

/// Asserts that a start that fails after the NFS front serves (its S3 address is taken) leaves the
/// mount it made where it is, says so, and ends the serving: the next start finds it and serves it
/// on the same address. Catches a failed start that unmounts under the jobs that hold the mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_s3_address_is_taken_leaves_the_mount_it_made() {
    let node = NfsNode::new("nfs-one-taken");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let settings = node.settings(&taken.local_addr().unwrap().to_string(), "127.0.0.1:0");
    let runner = Recorder::new(true);
    let system = node.service(&settings, &runner, handle);
    let log = Logged::default();
    let started = start_logged(
        settings,
        Arc::new(MemoryBackend::new()),
        (system, Arc::clone(&runner)),
        (None, None),
        Some(&log),
    );
    let failed = within("the start fails", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(failed.to_string().contains("Address"), "{failed}");
    assert!(
        log.text().contains(
            "tierstore fs: the start failed after the NFS front served; the mount is left for the next start"
        ),
        "{}",
        log.text()
    );
    assert_eq!(programs(&runner), [LISTING, MOUNT_NFS]);
    assert!(runner.has_mount());
    closed(loopback(runner.mounted_port().unwrap())).await;
}

/// The server of a test whose accept fails at once.
fn failing<'a>(
    _: &'a NFSTcpListener<TreeNfs>,
    _: BoxFuture<'static, ()>,
) -> BoxFuture<'a, std::io::Result<()>> {
    Box::pin(async { Err(std::io::Error::other("the accept failed")) })
}

/// Asserts that an NFS server that stops by itself stops the fronts as a signal does, is unmounted,
/// and ends the node in failure with why it stopped, so that launchd starts the job again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_nfs_server_that_stops_by_itself_unmounts_and_ends_the_node_in_failure() {
    let node = NfsNode::new("nfs-one-stopped");
    let settings = node.settings("127.0.0.1:0", "127.0.0.1:0");
    let runner = Recorder::new(true);
    let system = node.service(&settings, &runner, failing);
    let started = start(
        settings,
        Arc::new(MemoryBackend::new()),
        (system, Arc::clone(&runner)),
        None,
        None,
    );
    let failed = within("the node ends", started.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        failed.to_string(),
        "the NFS server stopped (the accept failed)"
    );
    assert_eq!(programs(&runner), [LISTING, MOUNT_NFS, LISTING, UMOUNT]);
}

#[path = "live_config_tests.rs"]
mod live_config;
