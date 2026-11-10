//! Fixtures of the tests of `tierstore fs`: trees written as manifests, a fill socket that
//! answers from them, and a core over both.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;

use super::core::{Core, Settings};
use super::fill_client::FillClient;
use super::locked;
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
    /// The requests received, in order.
    pub requests: Mutex<Vec<String>>,
}

impl Upstream {
    fn put(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// The answer to a line, or none for a request that is not answered.
    fn answer(&self, line: &str) -> Option<String> {
        locked(&self.requests).push(line.to_owned());
        let words: Vec<&str> = line.split(' ').collect();
        match words[0] {
            "fill" => {
                let sha = words[1];
                if let Some(why) = locked(&self.failing).get(sha) {
                    // `ok` is an answer that leaves no file.
                    return Some(why.clone());
                }
                let built = locked(&self.built);
                if words.len() == 2 {
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
            "touch" => None,
            "pin" => Some(format!("ok {} 0 0", words.len() - 3)),
            _ => Some("ok 0".to_owned()),
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
    _fill: FakeFill,
    pub cache: PathBuf,
    pub runtime: PathBuf,
    pub mountpoint: PathBuf,
    /// Last: the directories go after the socket that is in them is closed.
    pub dir: TestDir,
}

/// The owner every entry of the rigs reports.
pub(in crate::server) const OWNER: (u32, u32) = (1234, 5678);

impl Rig {
    /// A rig whose upstream holds `built`.
    pub(in crate::server) fn new(name: &str, built: Built) -> Self {
        Self::owned(name, built, OWNER)
    }

    /// A rig whose entries report `owner`.
    pub(in crate::server) fn owned(name: &str, built: Built, owner: (u32, u32)) -> Self {
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
            requests: Mutex::new(Vec::new()),
        });
        let socket = dir.path().join("fill.sock");
        let fill = FakeFill::start(&socket, Arc::clone(&upstream));
        let fills = FillClient::pausing(socket, 3, Duration::from_secs(30), Arc::new(|_| {}));
        let core = Arc::new(Core::new(
            Settings {
                owner,
                cache_path: cache.clone(),
                mountpoint: mountpoint.clone(),
                mount_id: "mount-1".to_owned(),
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
            _fill: fill,
            core,
        }
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
