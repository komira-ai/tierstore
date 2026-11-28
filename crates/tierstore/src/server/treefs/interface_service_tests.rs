//! The one hand-over of a node of the one-process layout (`docs/design/one-service.md` sections
//! 3.1 to 3.3): the running instance offers its sockets, its mount, its control socket and its
//! cache lock on `server.handoff_socket` in one conversation, and a start of the same file takes
//! them all there. Over the rig of `interface_tests.rs`.
//!
//! The running instance's drain is held by a HEAD of its S3 front that waits at its upstream (a
//! request that writes nothing to the cache), and its cache writes by a GET whose fill waits there:
//! the tests let each go.

use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use objstore_s3::{ByteRange as ObjectRange, ObjectKey, ObjectStore as _};
use tokio::net::UnixStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{
    FsNode, Started, assert_answers, assert_unanswered, left_state, memory, platform, start_on,
    stop, told,
};
use crate::server::Drained;
use crate::server::cache_cli_tests::{QUICK, client, probe};
use crate::server::cache_handoff_lock_tests::{Pin, pin};
use crate::server::cli::Addresses;
use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_cache::CacheSettings;
use crate::server::handoff::testing::{ask, identity, texts, within};
use crate::server::local_cache::testing::until;
use crate::server::lock::try_take;
use crate::server::treefs::fakedev::{self, FakeDevice};
use crate::server::treefs::testing::FakeManager;
use crate::{Backend, ByteRange, Error as StoreError, Listed, MemoryBackend, Meta};

/// The slots of the two processes, which tell their answers apart on the sockets they share.
const OLD: u16 = 9721;
const NEW: u16 = 9722;

/// The knobs of a run whose drain waits for its requests in flight as long as a test lasts.
const HELD: PerRun = PerRun {
    drain_deadline: Some(Duration::from_secs(3600)),
    ..QUICK
};

/// A gate of the upstream: the first call that reaches it once it is armed waits for the test.
#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    reached: Notify,
    let_go: Notify,
}

impl Gate {
    async fn pass(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.reached.notify_one();
            self.let_go.notified().await;
        }
    }
}

/// A memory upstream with a gate for its GETs (a fill of the cache waits there, a write in flight)
/// and one for its HEADs (an S3 HEAD waits there, writing nothing, and holds the drain).
#[derive(Default)]
struct Gated {
    inner: MemoryBackend,
    gets: Gate,
    heads: Gate,
}

#[async_trait]
impl Backend for Gated {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), StoreError> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, StoreError> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, StoreError> {
        self.gets.pass().await;
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, StoreError> {
        self.heads.pass().await;
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

/// The settings of a process of `node` with `slot`, handing over on `control`, whose S3 front
/// listens on `listen` and OCI front on `oci` (with the policy that caches what it serves), with a
/// `mount-provider` interface on `provider`.
fn settings(
    node: &FsNode,
    (control, slot): (&Path, u16),
    (listen, oci): (&str, &str),
    provider: &Path,
) -> CacheSettings {
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"\nslot = {slot}",
        control.display().to_string()
    );
    let interfaces = format!(
        "\n[[interface]]\ntype = \"oci\"\nparams = {{ addresses = [\"OCI\"] }}\n\
         \n[[interface]]\ntype   = \"mount-provider\"\nparams = {{ socket = {:?} }}\n\
         \n[[policy]]\nprefix = \"oci/\"\ncache  = true\n",
        provider.display().to_string()
    );
    let text = node
        .text_with(&server, "", listen, &interfaces)
        .replace("\"OCI\"", &format!("{oci:?}"));
    let mut settings = settings_from_text(&text, HELD)
        .unwrap()
        .into_cache()
        .unwrap();
    // A mount the test makes is its user's alone: `allow_other` needs the system's consent.
    if let Some(fs) = &mut settings.fs {
        fs.allow_other = false;
    }
    settings
}

/// The `/readyz` answer of the process behind `pin`, once it is `want`.
async fn readyz_of(pin: &Pin, want: &str) {
    loop {
        let (status, text) = probe(pin.at, "/readyz").await;
        if (status, text.as_str()) == (200, want) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// The running instance of an update: a node of the one-process layout serving the mount, its S3
/// and OCI fronts and its mount provider, over a gated upstream, with its drain held by a HEAD.
struct Old {
    /// Dropped before the node's directory, where its socket is.
    manager: FakeManager,
    node: FsNode,
    control: PathBuf,
    provider: PathBuf,
    old: Started,
    bound: Addresses,
    kernel: FakeDevice,
    stored: OwnedFd,
    upstream: Arc<Gated>,
    to_old: Pin,
    heading: JoinHandle<Result<Option<u64>, objstore_s3::ObjectStoreError>>,
}

impl Old {
    /// Starts it, in a scratch directory called `name`, and its HEAD of `cas/head`.
    async fn start(name: &str) -> Self {
        let node = FsNode::new(name);
        let control = node.dir.path().join("control.sock");
        let provider = node.dir.path().join("provider.sock");
        let any = "127.0.0.1:0";
        let old_settings = settings(&node, (&control, OLD), (any, any), &provider);
        left_state(old_settings.fs.as_ref().unwrap());
        let upstream = Arc::new(Gated::default());
        for key in ["cas/head", "cas/fill"] {
            upstream
                .inner
                .put(key, Bytes::from_static(b"held"))
                .await
                .unwrap();
        }
        let (stored, kernel) = fakedev::pair();
        let manager = FakeManager::start(&node.dir.path().join("old.sock"));
        let on = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
        let gated: Arc<dyn Backend> = upstream.clone();
        let mut old = start_on(old_settings, gated, on);
        let bound = within("the old serves", &mut old.bound).await.unwrap();
        assert_answers(&kernel, 1);
        until("the old binds the provider", || provider.exists()).await;
        let to_old = pin(bound.s3, OLD).await;
        upstream.heads.armed.store(true, Ordering::SeqCst);
        let at = to_old.at;
        let heading =
            tokio::spawn(async move { client(at).len(&ObjectKey::new("cas/head").unwrap()).await });
        within(
            "the head reaches the upstream",
            upstream.heads.reached.notified(),
        )
        .await;
        Self {
            manager,
            node,
            control,
            provider,
            old,
            bound,
            kernel,
            stored,
            upstream,
            to_old,
            heading,
        }
    }

    /// The lock file of its cache.
    fn lock(&self) -> PathBuf {
        self.node.node.cache_dir().join(".lock")
    }

    /// Starts the process that takes over from it, on the same file, and gives it once it serves,
    /// with a pin to it.
    async fn take_over(&self) -> (Started, Pin, FakeManager) {
        let new_settings = self.settings(NEW);
        let manager = FakeManager::start(&self.node.dir.path().join("new.sock"));
        let on = platform(&manager, None, self.node.kernel(false));
        let mut new = start_on(new_settings, memory(), on);
        let bound = within("the new serves", &mut new.bound).await.unwrap();
        assert_eq!(bound.s3, self.bound.s3);
        let to_new = pin(self.bound.s3, NEW).await;
        (new, to_new, manager)
    }

    /// Asserts that it still drains: it has not ended, and its HEAD is in flight.
    fn still_draining(&self) {
        assert!(!self.old.ended.is_finished(), "the old still drains");
        assert!(!self.heading.is_finished(), "its head is in flight");
    }

    /// Lets the HEAD go, and asserts that it is answered and that the process then ends with
    /// success, stopped first when `stopped` says so (no successor took its sockets): the node's
    /// directory, the manager and the descriptor its store kept.
    async fn let_go(self, stopped: bool) -> (FsNode, FakeManager, OwnedFd) {
        if stopped {
            self.old.stop.send(()).unwrap();
        }
        self.upstream.heads.let_go.notify_one();
        let answered = within("the head ends", self.heading).await.unwrap();
        assert_eq!(answered.unwrap(), Some(4));
        drop(self.to_old);
        let drained = within("the old ends", self.old.ended)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(drained, Drained { abandoned: 0 });
        (self.node, self.manager, self.stored)
    }
}

/// The lines an instance of `bound`, whose mount is at `mountpoint` with its control socket at
/// `control` and whose lock is `lock`, offers to a taker that asks for the mount.
fn offer_lines(
    bound: &Addresses,
    (mountpoint, control): (&Path, &Path),
    lock: &Path,
) -> Vec<String> {
    let mut lines = vec!["offer".to_owned()];
    lines.extend(bound.s3_all.iter().map(|addr| format!("s3 {addr}")));
    lines.extend(bound.oci_all.iter().map(|addr| format!("oci {addr}")));
    lines.push(format!("fuse {}", mountpoint.display()));
    lines.push(format!("control {}", control.display()));
    lines.push(format!("lock {}", lock.display()));
    lines
}

/// Asserts the offer of a running instance of the one process to a taker that asks `take 1`,
/// `ready`, `mount` on `server.handoff_socket`: an `s3` line for each socket of its S3 front, an
/// `oci` line for each of its OCI front, `fuse <mountpoint>`, `control <path>` and `lock <path>`,
/// each with its descriptor (the sockets on those addresses, the device it serves, the control
/// socket's listening socket, the lock file). At `ack` it reads the device no more and removes its
/// socket's file; at `ready` it answers `bye`, has removed its descriptor store's entry, its cache
/// starts no write (its mount provider, bound while it is the writer, closes) while it still
/// drains, it says `written`, and it ends without unlocking the lock the taker holds. Catches an
/// instance that offers its sockets alone, or keeps writing, serving the provider or holding the
/// store entry past `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_one_process_instance_offers_sockets_mount_control_and_lock_on_the_handoff_socket()
 {
    let old = Old::start("tsfs-svc-offer").await;
    let fs = old.old_fs();
    let lock = old.lock();
    let taker = UnixStream::connect(&old.control).await.unwrap();
    let offer = ask(&taker, &["take 1", "ready", "mount"]).await;
    let paths = (fs.mountpoint.as_path(), fs.control_socket.as_path());
    assert_eq!(offer.lines, offer_lines(&old.bound, paths, &lock));
    assert_eq!(offer.fds.len(), offer.lines.len() - 1);
    let mut fds = offer.fds.into_iter();
    for addr in old.bound.s3_all.iter().chain(&old.bound.oci_all) {
        let socket = std::net::TcpListener::from(fds.next().unwrap());
        assert_eq!(socket.local_addr().unwrap(), *addr);
    }
    let device = fds.next().unwrap();
    assert_eq!(identity(device.as_fd()), identity(old.stored.as_fd()));
    let control = std::os::unix::net::UnixListener::from(fds.next().unwrap());
    let at = control.local_addr().unwrap();
    assert_eq!(at.as_pathname(), Some(fs.control_socket.as_path()));
    let held = fds.next().unwrap();
    let file = std::fs::File::open(&lock).unwrap();
    assert_eq!(identity(held.as_fd()), identity(file.as_fd()));

    assert_eq!(texts(&ask(&taker, &["ack"]).await.lines), ["done"]);
    assert_unanswered(&old.kernel, 2);
    assert!(!old.control.exists(), "the old removed its socket's file");
    assert!(old.provider.exists(), "the old is the writer until ready");
    assert_eq!(texts(&ask(&taker, &["ready"]).await.lines), ["bye"]);
    told(&old.manager, &["READY=1", "FDSTOREREMOVE=1\nFDNAME=fuse"]);
    until("the old closes the provider", || !old.provider.exists()).await;
    old.still_draining();
    let written = within("written", crate::server::handoff::wire::recv(&taker)).await;
    assert_eq!(texts(&written.unwrap().lines), ["written"]);
    let (_node, _manager, _stored) = old.let_go(false).await;
    // The old ended without unlocking (`flock(LOCK_UN)` through any descriptor would free the lock
    // for every holder): no third process takes it.
    assert!(try_take(&lock).unwrap().is_none());
    drop((taker, held, file));
}

/// Asserts that the successor of a node of the one-process layout takes the mount, the sockets and
/// the lock in the one hand-over and is the cache's writer at the old process's `written`, not at
/// its exit: while the old process still fills an object (a cache write in flight) the successor
/// serves the mount read-only; once that fill ends it is read-write, with the old process still
/// draining. Catches a successor that writes the cache before the old one's last write has ended,
/// or only after the old one has exited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_of_one_process_is_writer_at_written_not_at_the_old_exit() {
    let old = Old::start("tsfs-svc-written").await;
    old.upstream.gets.armed.store(true, Ordering::SeqCst);
    let at = old.to_old.at;
    let filling = tokio::spawn(async move {
        let key = ObjectKey::new("cas/fill").unwrap();
        client(at)
            .get_range(&key, ObjectRange::new(0, 4).unwrap())
            .await
    });
    within(
        "the fill reaches the upstream",
        old.upstream.gets.reached.notified(),
    )
    .await;

    let (new, to_new, manager) = old.take_over().await;
    readyz_of(&to_new, "ready read-only").await;
    assert_answers(&old.kernel, 2);
    told(
        &manager,
        &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"],
    );
    old.still_draining();
    // Still read-only while the old process's fill is in flight.
    readyz_of(&to_new, "ready read-only").await;

    old.upstream.gets.let_go.notify_one();
    let filled = within("the fill ends", filling).await.unwrap().unwrap();
    assert_eq!(filled, b"held".to_vec());
    within(
        "the new takes the write role",
        readyz_of(&to_new, "ready read-write"),
    )
    .await;
    old.still_draining();
    let (status, _) = probe(to_new.at, "/healthz").await;
    assert_eq!(status, 200);
    let kept = old.let_go(false).await;
    drop(to_new);
    stop(new).await.unwrap();
    drop(manager);
    drop(kept);
}

/// Asserts that a start of a node with a mount and a cache, taking over from an instance of the one
/// process, is read-write once its predecessor has said `bye` (and `written`, at once, with no cache
/// write in flight) while that instance has not exited: it drains a request. The predecessor gave
/// its descriptor store's entry up at `ready`, and the successor's mount provider answers. Catches
/// a successor that stays read-only through the predecessor's drain, as one that asks for the
/// sockets alone does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taker_with_a_mount_and_a_cache_is_read_write_at_bye_without_the_old_process_exiting() {
    let old = Old::start("tsfs-svc-bye").await;
    let (new, to_new, manager) = old.take_over().await;
    within(
        "the new takes the write role",
        readyz_of(&to_new, "ready read-write"),
    )
    .await;
    old.still_draining();
    told(&old.manager, &["READY=1", "FDSTOREREMOVE=1\nFDNAME=fuse"]);
    assert_answers(&old.kernel, 2);
    until("the new binds the provider", || old.provider.exists()).await;
    let kept = old.let_go(false).await;
    drop(to_new);
    stop(new).await.unwrap();
    drop(manager);
    drop(kept);
}

/// Asserts that a taker that says `nak` to the offer, or goes away after `done` and before `ready`,
/// leaves the running instance as it was: it serves the mount (it answers the kernel again after
/// the quiesce), holds its lock (no third process takes it), keeps its descriptor store's entry,
/// and answers the next taker on its socket. Catches an instance that gives the mount or the lock
/// up before `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taker_that_naks_or_vanishes_before_ready_leaves_the_old_serving_with_its_lock() {
    let old = Old::start("tsfs-svc-nak").await;
    let lock = old.lock();
    let taker = UnixStream::connect(&old.control).await.unwrap();
    let offer = ask(&taker, &["take 1", "ready", "mount"]).await;
    assert_eq!(offer.lines[0], "offer");
    crate::server::handoff::testing::send(&taker, &["nak"]).await;
    drop((taker, offer));
    assert_answers(&old.kernel, 2);
    assert!(try_take(&lock).unwrap().is_none());

    let taker = UnixStream::connect(&old.control).await.unwrap();
    let offer = ask(&taker, &["take 1", "ready", "mount"]).await;
    assert_eq!(texts(&ask(&taker, &["ack"]).await.lines), ["done"]);
    drop((taker, offer));
    // It resumes, and makes its socket again.
    until("the old makes its socket again", || old.control.exists()).await;
    assert_answers(&old.kernel, 3);
    assert!(try_take(&lock).unwrap().is_none());
    told(&old.manager, &["READY=1"]);
    let again = UnixStream::connect(&old.control).await.unwrap();
    let offer = ask(&again, &["take 1", "ready", "mount"]).await;
    assert_eq!(offer.lines[0], "offer");
    drop((again, offer));
    old.still_draining();
    let _kept = old.let_go(true).await;
}

impl Old {
    /// The settings of a process with `slot` on its file and its addresses.
    fn settings(&self, slot: u16) -> CacheSettings {
        let listen = self.bound.s3.to_string();
        let oci = self.bound.oci.expect("an OCI front").to_string();
        settings(
            &self.node,
            (&self.control, slot),
            (&listen, &oci),
            &self.provider,
        )
    }

    /// The settings of its `fs` interface.
    fn old_fs(&self) -> crate::server::treefs::config::FsSettings {
        self.settings(OLD).fs.unwrap()
    }
}

/// What a running instance played by the test does at its `ack`: whether it then says `done`
/// (or goes away, as one that dies there does).
type AtAck = Box<dyn FnOnce() -> bool + Send>;

/// A running instance of the one process played by the test at `control`: it offers its S3
/// socket `s3`, the mount of `fs` (the device `device`, and a control socket it binds at its
/// path) and the lock file `lock`, reads `ack`, does `at_ack` and says `done` if it says so. It
/// gives what the taker sent next: none when the connection ended instead, or it went away. It
/// answers no `ready`.
fn played(
    control: &Path,
    fs: &crate::server::treefs::config::FsSettings,
    (s3, device, lock): (std::net::TcpListener, OwnedFd, PathBuf),
    at_ack: AtAck,
) -> JoinHandle<Option<Vec<String>>> {
    use crate::server::handoff::wire;
    let socket = tokio::net::UnixListener::bind(control).unwrap();
    let fs_control = std::os::unix::net::UnixListener::bind(&fs.control_socket).unwrap();
    let lines = vec![
        "offer".to_owned(),
        format!("s3 {}", s3.local_addr().unwrap()),
        format!("fuse {}", fs.mountpoint.display()),
        format!("control {}", fs.control_socket.display()),
        format!("lock {}", lock.display()),
    ];
    tokio::spawn(async move {
        let (stream, _) = socket.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap();
        assert_eq!(request.lines, ["take 1", "ready", "mount"]);
        // The taker opened its cache, and so its lock file, before it asked.
        let file = std::fs::File::open(&lock).unwrap();
        let fds = [s3.as_fd(), device.as_fd(), fs_control.as_fd(), file.as_fd()];
        wire::send(&stream, &lines, &fds).await.unwrap();
        assert_eq!(wire::recv(&stream).await.unwrap().lines, ["ack"]);
        if !at_ack() {
            return None;
        }
        wire::send(&stream, &["done".to_owned()], &[])
            .await
            .unwrap();
        wire::recv(&stream).await.ok().map(|frame| frame.lines)
    })
}

/// A start of `node` on the file that hands over on `control` and serves S3 on `s3`, with a fresh
/// service manager: what it ended with, and that manager.
async fn start_failing(
    node: &FsNode,
    control: &Path,
    s3: std::net::SocketAddr,
) -> (String, FakeManager) {
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"",
        control.display().to_string()
    );
    let manager = FakeManager::start(&node.dir.path().join("new.sock"));
    let on = platform(&manager, None, node.kernel(false));
    let new = start_on(node.settings(&server, "", &s3.to_string()), memory(), on);
    let error = within("the start fails", new.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(crate::server::cli::exit_status(&error), 1);
    assert!(new.bound.await.is_err(), "no address was said");
    (error.to_string(), manager)
}

/// Asserts that a start whose predecessor in the one hand-over ends the connection after its
/// `ready` without a `bye` (it gave up first, and serves on) fails before it serves anything: it
/// never reads the device, and tells its manager nothing
/// past the descriptor it stored before `ack`, which its restart adopts or takes again. Catches a
/// start that serves the mount beside an instance that still serves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_predecessor_says_no_bye_fails_and_leaves_the_mount_to_its_store() {
    let node = FsNode::new("tsfs-svc-nobye");
    let control = node.dir.path().join("control.sock");
    let fs = node.settings("", "", "127.0.0.1:0").fs.unwrap();
    left_state(&fs);
    let (device, kernel) = fakedev::pair();
    let s3 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = s3.local_addr().unwrap();
    let lock = node.node.cache_dir().join(".lock");
    let old = played(&control, &fs, (s3, device, lock), Box::new(|| true));
    let (error, manager) = start_failing(&node, &control, addr).await;
    assert!(error.starts_with("hand-off: "), "{error}");
    assert_eq!(
        within("the taker's ready", old).await.unwrap(),
        Some(vec!["ready".to_owned()])
    );
    assert_unanswered(&kernel, 1);
    told(&manager, &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1"]);
}

/// Asserts that a start whose take-over of the mount fails after its predecessor's `done` (the
/// runtime state cannot be read again) ends the connection before `ready`, so that the predecessor,
/// which has handed nothing over, resumes and serves on; the start fails by name and reads nothing
/// of the device. Catches a start that says `ready` for a mount it cannot serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_take_over_that_fails_after_done_ends_the_connection_before_ready() {
    let node = FsNode::new("tsfs-svc-unready");
    let control = node.dir.path().join("control.sock");
    let fs = node.settings("", "", "127.0.0.1:0").fs.unwrap();
    left_state(&fs);
    let (device, kernel) = fakedev::pair();
    let s3 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = s3.local_addr().unwrap();
    let lock = node.node.cache_dir().join(".lock");
    let record = fs
        .runtime_dir
        .join(crate::server::treefs::init_record::FILE);
    let gone: AtAck = Box::new(move || std::fs::remove_file(record).is_ok());
    let old = played(&control, &fs, (s3, device, lock), gone);
    let (error, manager) = start_failing(&node, &control, addr).await;
    assert!(!error.starts_with("hand-off: "), "{error}");
    assert_eq!(
        within("the end of the connection", old).await.unwrap(),
        None
    );
    assert_unanswered(&kernel, 1);
    told(&manager, &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1"]);
}

/// Asserts that an instance whose resume after a taker gone before `ready` cannot serve the mount
/// again (its put directory's journal cannot be opened again) stops its fronts as a signal does and
/// ends in failure by name, leaving the descriptor in its store for the restart. Catches an
/// instance that goes on serving S3 with a mount no one reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_that_cannot_serve_the_mount_again_ends_the_node_in_failure() {
    let node = FsNode::new("tsfs-svc-stuck");
    let control = node.dir.path().join("control.sock");
    let stage = node.dir.path().join("stage");
    let server = format!(
        "handoff_socket = {:?}\nhandoff_wait = \"30s\"",
        control.display().to_string()
    );
    let put = format!(
        "[interface.params.put]\nenabled = true\nprefixes = [\"cas/b/\"]\nstage_dir = {:?}",
        stage.display().to_string()
    );
    let settings = node.settings(&server, &put, "127.0.0.1:0");
    left_state(settings.fs.as_ref().unwrap());
    let (stored, kernel) = fakedev::pair();
    let manager = FakeManager::start(&node.dir.path().join("notify.sock"));
    let on = platform(&manager, Some(stored.as_raw_fd()), node.kernel(false));
    let mut old = start_on(settings, memory(), on);
    within("the old serves", &mut old.bound).await.unwrap();
    assert_answers(&kernel, 1);
    let taker = UnixStream::connect(&control).await.unwrap();
    let offer = ask(&taker, &["take 1", "ready", "mount"]).await;
    assert_eq!(texts(&ask(&taker, &["ack"]).await.lines), ["done"]);
    std::fs::remove_dir_all(&stage).unwrap();
    drop((taker, offer));
    let error = within("the old ends", old.ended)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("hand-over: the instance could not serve again: "),
        "{error}"
    );
    told(&manager, &["READY=1"]);
    drop(stored);
}

/// Asserts that a start whose predecessor in the one hand-over goes away after its `ack` and before
/// `done` (it died there) fails by name before it reads the device or serves anything, with the
/// descriptor in its store for its restart, which takes the mount again or adopts it. Catches a
/// start that serves a mount whose predecessor may still read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_predecessor_dies_after_ack_fails_with_the_mount_in_its_store() {
    let node = FsNode::new("tsfs-svc-diesack");
    let control = node.dir.path().join("control.sock");
    let fs = node.settings("", "", "127.0.0.1:0").fs.unwrap();
    left_state(&fs);
    let (device, kernel) = fakedev::pair();
    let s3 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = s3.local_addr().unwrap();
    let lock = node.node.cache_dir().join(".lock");
    let old = played(&control, &fs, (s3, device, lock), Box::new(|| false));
    let (error, manager) = start_failing(&node, &control, addr).await;
    assert!(error.starts_with("hand-off: "), "{error}");
    assert_eq!(within("the old goes", old).await.unwrap(), None);
    assert_unanswered(&kernel, 1);
    told(&manager, &["FDSTORE=1\nFDNAME=fuse", "BARRIER=1"]);
}

/// A running instance of the node without a mount, played by the test at `control`: it refuses the
/// request with the `mount` line as an instance that holds no mount does, offers its S3 socket
/// `s3` and the lock file `lock` to the request without it, and gives what the taker sent next:
/// none when the connection ended instead.
fn played_without_mount(
    control: &Path,
    (s3, lock): (std::net::TcpListener, PathBuf),
) -> JoinHandle<Option<Vec<String>>> {
    use crate::server::handoff::wire;
    let socket = tokio::net::UnixListener::bind(control).unwrap();
    let lines = vec![
        "offer".to_owned(),
        format!("s3 {}", s3.local_addr().unwrap()),
        format!("lock {}", lock.display()),
    ];
    tokio::spawn(async move {
        let (stream, _) = socket.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap();
        assert_eq!(request.lines, ["take 1", "ready", "mount"]);
        let refusal = [
            "refused".to_owned(),
            "this instance holds no mount".to_owned(),
        ];
        wire::send(&stream, &refusal, &[]).await.unwrap();
        drop(stream);
        let (stream, _) = socket.accept().await.unwrap();
        let request = wire::recv(&stream).await.unwrap();
        assert_eq!(request.lines, ["take 1", "ready"]);
        let file = std::fs::File::open(&lock).unwrap();
        let fds = [s3.as_fd(), file.as_fd()];
        wire::send(&stream, &lines, &fds).await.unwrap();
        wire::recv(&stream).await.ok().map(|frame| frame.lines)
    })
}

/// Asserts that a start whose predecessor holds no mount but offers its sockets, and whose own
/// mount cannot be made (the path of its runtime directory is a regular file), fails by the mount's
/// reason, not as a hand-off, and refuses the predecessor's offer with `nak` before it accepts
/// anything, so that the predecessor serves on with its sockets. Catches a start that accepts the
/// sockets of a running instance before it has the mount it needs, which leaves the node with
/// neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_whose_mount_cannot_be_made_refuses_the_sockets_a_mountless_instance_offered() {
    let node = FsNode::new("tsfs-svc-nomount");
    let control = node.dir.path().join("control.sock");
    std::fs::write(node.runtime(), b"").unwrap();
    let s3 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = s3.local_addr().unwrap();
    let lock = node.node.cache_dir().join(".lock");
    let old = played_without_mount(&control, (s3, lock));
    let (error, _manager) = start_failing(&node, &control, addr).await;
    assert!(!error.starts_with("hand-off: "), "{error}");
    assert_eq!(
        within("the taker's answer", old).await.unwrap(),
        Some(vec!["nak".to_owned()])
    );
}
