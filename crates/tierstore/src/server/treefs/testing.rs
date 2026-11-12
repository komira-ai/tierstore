//! Fixtures of the tests of `tierstore fs`: trees written as manifests, a fill socket that
//! answers from them, and a core over both.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io::IoSliceMut;
use std::io::{BufRead as _, BufReader, Write as _};
#[cfg(target_os = "linux")]
use std::mem::MaybeUninit;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt as _;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixDatagram;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[cfg(target_os = "linux")]
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;

use crate::{Clock, Mono};

use super::backing::{Guard, Registered};
use super::config::FsSettings;
use super::core::{Core, Errno, Opened, Settings};
use super::fill_client::FillClient;
use super::locked;
use super::put::fake::FakePuts;
use super::table::TreeTable;
use crate::server::local_cache::{blob_path, manifest_path};

/// The digest of `bytes` as hex.
pub(in crate::server) fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A tree to write.
#[derive(Clone)]
pub(in crate::server) enum Node {
    /// A file; `packed` gives it a source inside a pack, else it is the object `cas/l/<sha256>`.
    File {
        content: Vec<u8>,
        exec: bool,
        packed: bool,
    },
    Dir(Vec<(String, Node)>),
    Link(String),
}

/// A file of `content`, not executable, packed.
pub(in crate::server) fn file(content: &[u8]) -> Node {
    Node::File {
        content: content.to_vec(),
        exec: false,
        packed: true,
    }
}

/// A directory of `children`.
pub(in crate::server) fn dir(children: Vec<(&str, Node)>) -> Node {
    Node::Dir(
        children
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect(),
    )
}

/// The manifests and blobs a tree is made of.
#[derive(Default)]
pub(in crate::server) struct Built {
    /// The digest of the root manifest.
    pub root: String,
    pub manifests: HashMap<String, Vec<u8>>,
    pub blobs: HashMap<String, Vec<u8>>,
}

impl Built {
    /// Writes the tree whose root holds `children`.
    pub(in crate::server) fn of(children: Vec<(&str, Node)>) -> Self {
        let mut built = Self::default();
        let children: Vec<(String, Node)> = children
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect();
        built.root = built.manifest(&children);
        built
    }

    /// Adds the tree whose root holds `children` beside the others; the digest of its root.
    pub(in crate::server) fn add_tree(&mut self, children: Vec<(&str, Node)>) -> String {
        let children: Vec<(String, Node)> = children
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect();
        self.manifest(&children)
    }

    fn manifest(&mut self, children: &[(String, Node)]) -> String {
        let mut sorted = children.to_vec();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut items = Vec::new();
        for (name, child) in &sorted {
            let item = match child {
                Node::Dir(grandchildren) => {
                    let digest = self.manifest(grandchildren);
                    let size = self.manifests[&digest].len();
                    serde_json::json!({"name": name, "kind": "dir", "sha256": digest, "size": size})
                }
                Node::File {
                    content,
                    exec,
                    packed,
                } => {
                    let digest = sha(content);
                    let _ = self.blobs.insert(digest.clone(), content.clone());
                    let mut item = serde_json::json!({
                        "name": name, "kind": "file", "sha256": digest, "size": content.len(),
                        "exec": exec,
                    });
                    if *packed && !content.is_empty() {
                        item["source"] = serde_json::json!({"key": "packs/one", "offset": 4096});
                    }
                    item
                }
                Node::Link(target) => {
                    serde_json::json!({"name": name, "kind": "symlink", "target": target})
                }
            };
            items.push(item);
        }
        let bytes = serde_json::to_vec(&serde_json::json!({"v": 1, "children": items})).unwrap();
        let digest = sha(&bytes);
        let _ = self.manifests.insert(digest.clone(), bytes);
        digest
    }
}

/// What the fake fill socket knows and does.
pub(in crate::server) struct Upstream {
    cache: PathBuf,
    pub built: Mutex<Built>,
    /// The digests whose fill is answered with these words.
    pub failing: Mutex<HashMap<String, String>>,
    /// When set, the answer to every `pin` instead of `ok`.
    pub pin_answer: Mutex<Option<String>>,
    /// The requests received, in order.
    pub requests: Mutex<Vec<String>>,
    /// What answers the `put` requests.
    pub puts: Arc<FakePuts>,
    /// Whether the fills of blobs wait (see [`Upstream::hold_fills`]).
    holding: Mutex<bool>,
    released: Condvar,
    /// The fills of manifests in flight (see [`Upstream::gate_manifests`]).
    gate: Mutex<Gate>,
    gate_changed: Condvar,
}

/// The progress of the fills of manifests, which a test may pace.
#[derive(Default)]
struct Gate {
    /// Whether a fill of a manifest waits for a permit.
    gating: bool,
    /// The fills received.
    started: usize,
    /// The fills received and not answered, and the most there have been at once.
    in_flight: usize,
    most: usize,
    /// The permits given and the permits used.
    given: usize,
    used: usize,
}

impl Upstream {
    /// Writes `bytes` at `path` by a rename of a file written beside it, so that a reader never
    /// sees a part of them, as the cache links a whole file into place.
    fn put(path: &Path, bytes: &[u8]) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut beside = path.as_os_str().to_owned();
        beside.push(format!(".part-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&beside, bytes).unwrap();
        std::fs::rename(&beside, path).unwrap();
    }

    /// The answer to a line, or none for a request that is not answered.
    fn answer(&self, line: &str) -> Option<String> {
        locked(&self.requests).push(line.to_owned());
        let words: Vec<&str> = line.split(' ').collect();
        match words[0] {
            "fill" if words.len() == 2 => {
                self.enter_manifest();
                let answer = self.answer_fill(words[1], 2);
                self.leave_manifest();
                answer
            }
            "fill" => {
                self.wait_while_held();
                self.answer_fill(words[1], words.len())
            }
            "touch" => None,
            "put" => Some(self.puts.answer(line)),
            "pin" => Some(
                locked(&self.pin_answer)
                    .clone()
                    .unwrap_or_else(|| format!("ok {} 0 0", words.len() - 3)),
            ),
            _ => Some("ok 0".to_owned()),
        }
    }

    /// The answer to a fill of `sha`: a manifest when `words` is 2, else a blob.
    fn answer_fill(&self, sha: &str, words: usize) -> Option<String> {
        if let Some(why) = locked(&self.failing).get(sha) {
            // `ok` is an answer that leaves no file.
            return Some(why.clone());
        }
        let built = locked(&self.built);
        if words == 2 {
            let Some(bytes) = built.manifests.get(sha) else {
                return Some("upstream 404".to_owned());
            };
            Self::put(&manifest_path(&self.cache, sha), bytes);
        } else {
            let Some(bytes) = built.blobs.get(sha) else {
                return Some("upstream 404".to_owned());
            };
            Self::put(&blob_path(&self.cache, sha), bytes);
        }
        Some("ok".to_owned())
    }

    /// Makes every fill of a manifest wait, once it is received, for a permit of
    /// [`Upstream::permit_manifests`]: for a test that paces the fills by its own events.
    pub(in crate::server) fn gate_manifests(&self) {
        locked(&self.gate).gating = true;
    }

    /// Lets `count` more fills of manifests go on.
    pub(in crate::server) fn permit_manifests(&self, count: usize) {
        locked(&self.gate).given += count;
        self.gate_changed.notify_all();
    }

    /// Waits until `count` fills of manifests have been received in all.
    pub(in crate::server) fn wait_for_manifest_fills(&self, count: usize) {
        let gate = locked(&self.gate);
        let _gate = self
            .gate_changed
            .wait_while(gate, |gate| gate.started < count)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The most fills of manifests that were in flight at once.
    pub(in crate::server) fn most_manifest_fills(&self) -> usize {
        locked(&self.gate).most
    }

    fn enter_manifest(&self) {
        let mut gate = locked(&self.gate);
        gate.started += 1;
        gate.in_flight += 1;
        gate.most = gate.most.max(gate.in_flight);
        self.gate_changed.notify_all();
        while gate.gating && gate.used >= gate.given {
            gate = self
                .gate_changed
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        gate.used += 1;
    }

    fn leave_manifest(&self) {
        locked(&self.gate).in_flight -= 1;
        self.gate_changed.notify_all();
    }

    /// Makes every fill of a blob wait, once it is received, until [`Upstream::release_fills`]: for
    /// a test that needs a fill in flight.
    pub(in crate::server) fn hold_fills(&self) {
        *locked(&self.holding) = true;
    }

    /// Lets the fills that wait go on, and those that come.
    pub(in crate::server) fn release_fills(&self) {
        *locked(&self.holding) = false;
        self.released.notify_all();
    }

    fn wait_while_held(&self) {
        let mut holding = locked(&self.holding);
        while *holding {
            holding = self
                .released
                .wait(holding)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// The requests that begin with `verb`.
    pub(in crate::server) fn asked(&self, verb: &str) -> Vec<String> {
        locked(&self.requests)
            .iter()
            .filter(|line| line.split(' ').next() == Some(verb))
            .cloned()
            .collect()
    }
}

/// A Unix socket that answers the lines of the fill socket from an [`Upstream`].
pub(in crate::server) struct FakeFill {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeFill {
    /// Serves `upstream` at `path`.
    pub(in crate::server) fn start(path: &Path, upstream: Arc<Upstream>) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                let upstream = Arc::clone(&upstream);
                let _ = std::thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
                    let mut writer = &stream;
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if let Some(answer) = upstream.answer(line.trim_end_matches('\n')) {
                            let _ = writer.write_all(format!("{answer}\n").as_bytes());
                        }
                        line.clear();
                    }
                });
            }
        });
        Self {
            path: path.to_owned(),
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeFill {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        let _ = self.thread.take().map(std::thread::JoinHandle::join);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A core over a fake fill socket, in scratch directories.
pub(in crate::server) struct Rig {
    pub core: Arc<Core>,
    pub upstream: Arc<Upstream>,
    /// The clock of the core, which only the test moves.
    pub clock: Arc<Mono>,
    _fill: FakeFill,
    pub cache: PathBuf,
    pub runtime: PathBuf,
    pub mountpoint: PathBuf,
    /// Last: the directories go after the socket that is in them is closed.
    pub dir: TestDir,
}

impl Rig {
    /// Settings of `tierstore fs` over the scratch directories of the rig: the mount point, the
    /// runtime directory, the sockets and the cache.
    pub(in crate::server::treefs) fn settings(&self) -> FsSettings {
        FsSettings {
            mountpoint: self.mountpoint.clone(),
            runtime_dir: self.runtime.clone(),
            control_socket: self.dir.path().join("control.sock"),
            client_group: std::fs::metadata(&self.runtime).unwrap().gid(),
            owner: (
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw(),
            ),
            fill_socket: self.dir.path().join("fill.sock"),
            cache_path: self.cache.clone(),
            fetch_timeout: Duration::from_secs(5),
            threads: 4,
            passthrough: true,
            max_backing_ids: 100,
            handoff_socket: self.dir.path().join("handoff.sock"),
            handoff_wait: Duration::from_secs(30),
            recover_wait: Duration::from_secs(30),
            reattach_concurrency: 4,
            tree_idle: TREE_IDLE,
            allow_other: false,
            nfs: crate::server::treefs::nfs::config::NfsSettings::default(),
            put: None,
        }
    }

    /// The core of the process that takes over from the one of this rig: the generation after
    /// it, over the same table, runtime directory, cache and fill socket.
    pub(in crate::server::treefs) fn successor(&self) -> Arc<Core> {
        let fills = FillClient::pausing(
            self.dir.path().join("fill.sock"),
            3,
            Duration::from_secs(30),
            Arc::new(|_| {}),
        );
        let mut settings = self.core.settings().clone();
        settings.generation += 1;
        Arc::new(Core::new(
            settings,
            fills,
            TreeTable::open(&self.runtime).unwrap(),
            &self.runtime,
        ))
    }
}

/// Tests that send signals to the process go one at a time: a signal is for every handler.
pub(in crate::server::treefs) static SIGNALS: Mutex<()> = Mutex::new(());

/// How long a tree a lookup attached stays attached with no use, in the rigs.
pub(in crate::server) const TREE_IDLE: Duration = Duration::from_secs(60);

/// The owner every entry of the rigs reports.
pub(in crate::server) const OWNER: (u32, u32) = (1234, 5678);

impl Rig {
    /// A rig whose upstream holds `built`.
    pub(in crate::server) fn new(name: &str, built: Built) -> Self {
        Self::owned(name, built, OWNER)
    }

    /// A rig whose entries report `owner`.
    pub(in crate::server) fn owned(name: &str, built: Built, owner: (u32, u32)) -> Self {
        Self::limited(name, built, owner, 100)
    }

    /// A rig whose entries report `owner` and whose core gives at most `most` backing ids.
    pub(in crate::server) fn limited(
        name: &str,
        built: Built,
        owner: (u32, u32),
        most: usize,
    ) -> Self {
        Self::made(name, built, (owner, most), true)
    }

    /// A rig whose core does not count lookups: a frontend with no forget.
    pub(in crate::server) fn uncounted(name: &str, built: Built) -> Self {
        Self::made(name, built, (OWNER, 100), false)
    }

    /// A second core over the same cache and fill socket, with a table of tree numbers and a
    /// runtime directory of its own: what a restart makes, before it attaches anything.
    pub(in crate::server) fn second_core(&self) -> Arc<Core> {
        let runtime = self.dir.path().join("run2");
        std::fs::create_dir_all(&runtime).unwrap();
        let socket = self.dir.path().join("fill.sock");
        let fills = FillClient::pausing(socket, 3, Duration::from_secs(30), Arc::new(|_| {}));
        Arc::new(Core::new(
            Settings {
                owner: OWNER,
                cache_path: self.cache.clone(),
                mountpoint: self.mountpoint.clone(),
                mount_id: "mount-2".to_owned(),
                max_backing_ids: 100,
                generation: 1,
                count_lookups: false,
                tree_idle: TREE_IDLE,
                clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            },
            fills,
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ))
    }

    fn made(
        name: &str,
        built: Built,
        (owner, most): ((u32, u32), usize),
        count_lookups: bool,
    ) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let cache = dir.path().join("cache");
        let runtime = dir.path().join("run");
        let mountpoint = dir.path().join("mnt");
        for path in [&cache, &runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let upstream = Arc::new(Upstream {
            cache: cache.clone(),
            built: Mutex::new(built),
            failing: Mutex::new(HashMap::new()),
            pin_answer: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
            puts: Arc::new(FakePuts::default()),
            holding: Mutex::new(false),
            released: Condvar::new(),
            gate: Mutex::new(Gate::default()),
            gate_changed: Condvar::new(),
        });
        let socket = dir.path().join("fill.sock");
        let fill = FakeFill::start(&socket, Arc::clone(&upstream));
        let fills = FillClient::pausing(socket, 3, Duration::from_secs(30), Arc::new(|_| {}));
        let clock = Arc::new(Mono::new());
        let core = Arc::new(Core::new(
            Settings {
                owner,
                cache_path: cache.clone(),
                mountpoint: mountpoint.clone(),
                mount_id: "mount-1".to_owned(),
                max_backing_ids: most,
                generation: 1,
                count_lookups,
                tree_idle: TREE_IDLE,
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
            },
            fills,
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ));
        Self {
            dir,
            cache,
            runtime,
            mountpoint,
            upstream,
            clock,
            _fill: fill,
            core,
        }
    }
}

/// The kernel's backing ids, as the tests of the core have them: numbers it gives, the files it was
/// asked to register, and the ids closed (an id is closed when its guard is dropped).
pub(in crate::server::treefs) struct FakeKernel {
    next: AtomicU32,
    /// Whether the next registrations are refused (`EPERM`).
    pub refusing: AtomicBool,
    /// The registrations, as the length of each file registered.
    pub registered: Mutex<Vec<u64>>,
    /// The ids closed, in order.
    pub closed: Arc<Mutex<Vec<u32>>>,
}

/// Closes an id of a [`FakeKernel`] when dropped.
struct Closes {
    id: u32,
    closed: Arc<Mutex<Vec<u32>>>,
}

impl Drop for Closes {
    fn drop(&mut self) {
        locked(&self.closed).push(self.id);
    }
}

impl FakeKernel {
    pub(in crate::server::treefs) fn new() -> Self {
        Self {
            next: AtomicU32::new(100),
            refusing: AtomicBool::new(false),
            registered: Mutex::new(Vec::new()),
            closed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Registers `file`: the next number, or the refusal of a process without the capability.
    pub(in crate::server::treefs) fn register(
        &self,
        file: &std::fs::File,
    ) -> std::io::Result<Registered> {
        if self.refusing.load(Ordering::Acquire) {
            return Err(std::io::Error::from_raw_os_error(1));
        }
        locked(&self.registered).push(file.metadata()?.len());
        Ok(self.registration())
    }

    /// The guard that closes the id `id`, which an earlier process registered.
    pub(in crate::server::treefs) fn wrap(&self, id: u32) -> Guard {
        Arc::new(Closes {
            id,
            closed: Arc::clone(&self.closed),
        })
    }

    /// The next number, as a registration: closed when its guard is dropped.
    pub(in crate::server::treefs) fn registration(&self) -> Registered {
        let id = self.next.fetch_add(1, Ordering::AcqRel);
        let closed = Arc::clone(&self.closed);
        Registered {
            id,
            guard: Arc::new(Closes { id, closed }),
        }
    }

    /// An open of `ino` on `core`, registering with this kernel.
    pub(in crate::server::treefs) fn open(&self, core: &Core, ino: u64) -> Result<Opened, Errno> {
        core.open(ino, false, &mut |file| self.register(file))
    }
}

/// A cached open of `ino` on a core that has not chosen passthrough: the handle.
pub(in crate::server::treefs) fn open_cached(
    core: &Core,
    ino: u64,
    writing: bool,
) -> Result<u64, Errno> {
    match core.open(ino, writing, &mut |file| FakeKernel::new().register(file)) {
        Ok(Opened::Cached(fh) | Opened::Backed { fh, .. }) => Ok(fh),
        Err(error) => Err(error),
    }
}

/// A message the fake service manager got: its text, and the descriptors that came with it.
#[cfg(target_os = "linux")]
pub(in crate::server::treefs) struct Message {
    pub text: String,
    pub fds: Vec<OwnedFd>,
}

/// A stand-in for the service manager: a datagram socket that keeps what it is sent, descriptors
/// included, and closes the pipe of a `BARRIER=1` as the manager does when it gets there.
#[cfg(target_os = "linux")]
pub(in crate::server::treefs) struct FakeManager {
    path: PathBuf,
    messages: Arc<Mutex<Vec<Message>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The room for the ancillary data of a message to the fake manager.
#[cfg(target_os = "linux")]
const ANCILLARY: usize = rustix::cmsg_space!(ScmRights(8), ScmCredentials(1));

/// The text a [`FakeManager`] is sent to end its thread.
#[cfg(target_os = "linux")]
const STOP: &str = "stop the fake manager";

#[cfg(target_os = "linux")]
impl FakeManager {
    /// A manager that listens at `path`.
    pub(in crate::server::treefs) fn start(path: &Path) -> Self {
        let socket = UnixDatagram::bind(path).unwrap();
        // Every message then carries the sender's credentials beside any descriptors, as a real
        // manager's does not, so that the loop below sees an ancillary message it passes over.
        rustix::net::sockopt::set_socket_passcred(&socket, true).unwrap();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&messages);
        let thread = std::thread::spawn(move || {
            loop {
                let mut buffer = [0u8; 1024];
                let mut space = [MaybeUninit::uninit(); ANCILLARY];
                let mut control = RecvAncillaryBuffer::new(&mut space);
                let mut iov = [IoSliceMut::new(&mut buffer)];
                let got = recvmsg(&socket, &mut iov, &mut control, RecvFlags::empty()).unwrap();
                let mut fds = Vec::new();
                for message in control.drain() {
                    if let RecvAncillaryMessage::ScmRights(rights) = message {
                        fds.extend(rights);
                    }
                }
                let text = String::from_utf8_lossy(&buffer[..got.bytes]).into_owned();
                if text == STOP {
                    return;
                }
                if text == "BARRIER=1" {
                    // The manager has handled everything sent before: it lets go of the pipe.
                    locked(&kept).push(Message {
                        text,
                        fds: Vec::new(),
                    });
                    drop(fds);
                } else {
                    locked(&kept).push(Message { text, fds });
                }
            }
        });
        Self {
            path: path.to_owned(),
            messages,
            thread: Some(thread),
        }
    }

    /// The socket's path, as `NOTIFY_SOCKET` holds it.
    pub(in crate::server::treefs) fn socket(&self) -> &Path {
        &self.path
    }

    /// The texts received so far, in order.
    pub(in crate::server::treefs) fn texts(&self) -> Vec<String> {
        locked(&self.messages)
            .iter()
            .map(|message| message.text.clone())
            .collect()
    }

    /// Takes the descriptors that came with the first message whose text is `text`.
    pub(in crate::server::treefs) fn take_fds(&self, text: &str) -> Vec<OwnedFd> {
        locked(&self.messages)
            .iter_mut()
            .find(|message| message.text == text)
            .map(|message| std::mem::take(&mut message.fds))
            .unwrap_or_default()
    }
}

#[cfg(target_os = "linux")]
impl Drop for FakeManager {
    fn drop(&mut self) {
        let _ =
            UnixDatagram::unbound().and_then(|socket| socket.send_to(STOP.as_bytes(), &self.path));
        let _ = self.thread.take().map(std::thread::JoinHandle::join);
    }
}

/// The sample tree most tests use:
///
/// ```text
/// big.bin      packed, 300000 bytes
/// bin/cc       packed, executable, 3000 bytes
/// bin/tool     not packed (the object cas/l/<sha256>), 10 bytes
/// empty.h      no bytes
/// include/a.h  packed, 6 bytes
/// lib          a link to bin
/// ```
pub(in crate::server) fn sample() -> Built {
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
    Built::of(vec![
        ("big.bin", file(&big)),
        (
            "bin",
            dir(vec![
                (
                    "cc",
                    Node::File {
                        content: cc,
                        exec: true,
                        packed: true,
                    },
                ),
                (
                    "tool",
                    Node::File {
                        content: b"tool bytes".to_vec(),
                        exec: false,
                        packed: false,
                    },
                ),
            ]),
        ),
        ("empty.h", file(b"")),
        ("include", dir(vec![("a.h", file(b"int a;"))])),
        ("lib", Node::Link("bin".to_owned())),
    ])
}

/// Why a FUSE mount cannot be made, given whether `/dev/fuse` can be opened for reading and
/// writing and `fusermount3` is installed.
pub(in crate::server) fn reasons(device: bool, helper: bool) -> Option<&'static str> {
    [
        (device, "/dev/fuse cannot be opened for reading and writing"),
        (helper, "fusermount3 is not installed"),
    ]
    .into_iter()
    .find_map(|(ok, why)| (!ok).then_some(why))
}

/// Whether this machine can make a FUSE mount as the user of the tests; if not, why.
pub(in crate::server) fn unavailable() -> Option<&'static str> {
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok();
    // The mount runs the helper by its name, so the test finds it the same way.
    let helper = std::process::Command::new("fusermount3")
        .arg("--version")
        .output()
        .is_ok();
    reasons(device, helper)
}

/// Says on stderr why a test does not run.
pub(in crate::server) fn skipped(why: &str) {
    let _ = writeln!(std::io::stderr(), "skipped: {why}");
}

/// Runs `test` where a FUSE mount can be made, and says why it does not run where it cannot.
pub(in crate::server) fn with_fuse(test: impl FnOnce()) {
    unavailable().map_or_else(test, skipped);
}

/// Looks at `condition` again and again, yielding between looks, until it holds: for an event that
/// another thread brings, with no clock in it. The test fails if the event never comes.
pub(in crate::server) fn until(mut condition: impl FnMut() -> bool) {
    let looks = (0..50_000_000u32).find(|_| {
        std::thread::yield_now();
        condition()
    });
    let _ = looks.expect("the event never came");
}

#[cfg(test)]
#[path = "testing_tests.rs"]
mod tests;
