//! Fixtures of the tests of the file system: trees written as manifests and a pack, an upstream
//! that holds them, the node's cache over that upstream, and a core that calls the cache in the
//! process, as the `fs` interface's does.
//!
//! The core's fills reach the [`Upstream`] as the cache's reads of it: a fill of a manifest is the
//! cache's HEAD of `cas/l/<sha256>` (then its GET), and a fill of a blob the cache's GET of the
//! blob's bytes in the pack (or of `cas/l/<sha256>`, for a file that is not packed). The upstream
//! keeps each in the words the core asks it with ([`Upstream::asked`]), paces them where a test
//! asks it to, and fails those a test names. What the core asks of the cache alone (pins,
//! placements, reports) is the cache's, and a test looks at the cache ([`Rig::node`]).

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::io::IoSliceMut;
#[cfg(target_os = "linux")]
use std::mem::MaybeUninit;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt as _;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
#[cfg(target_os = "linux")]
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;
use tokio::runtime::Runtime;

use crate::{
    Backend, ByteRange, Clock, Error, Listed, MemoryBackend, Meta, Mono, OpenUpload, PartBody,
    PutAnswer, TestClock, UploadedPart, UploadsAfter,
};

use super::backing::Guard;
use super::config::FsSettings;
use super::core::{Core, Errno, Opened, Settings};
use super::fills::Calls;
use super::in_process::InProcess;
use super::locked;
use super::table::TreeTable;
use crate::server::local_cache::testing::FakeDisk;
use crate::server::local_cache::{CacheConfig, LocalCache};

pub(in crate::server::treefs) use super::testing_image::{image_bytes, image_named};

/// The digest of `bytes` as hex.
pub(in crate::server) fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The key of the pack the packed files of a [`Built`] are in.
pub(in crate::server) const PACK: &str = "packs/one";

/// Where the first file of the pack begins: what comes before it is no file's.
pub(in crate::server) const PACK_START: u64 = 4096;

/// A tree to write.
#[derive(Clone)]
pub(in crate::server) enum Node {
    /// A file; `packed` gives it a source inside the pack [`PACK`], else it is the object
    /// `cas/l/<sha256>`.
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

/// The manifests, blobs and pack a tree is made of.
#[derive(Default)]
pub(in crate::server) struct Built {
    /// The digest of the root manifest.
    pub root: String,
    pub manifests: HashMap<String, Vec<u8>>,
    pub blobs: HashMap<String, Vec<u8>>,
    /// The bytes of the pack [`PACK`]: [`PACK_START`] bytes of no file, then each packed blob
    /// once, in the order they were first written.
    pub pack: Vec<u8>,
    /// Where each packed blob begins in the pack, by digest.
    pub offsets: HashMap<String, u64>,
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

    /// Where the blob `content`, of the digest `digest`, is in the pack: put at its end the first
    /// time.
    fn packed(&mut self, digest: &str, content: &[u8]) -> u64 {
        if let Some(offset) = self.offsets.get(digest) {
            return *offset;
        }
        if self.pack.is_empty() {
            self.pack = vec![0; usize::try_from(PACK_START).unwrap()];
        }
        let offset = self.pack.len() as u64;
        self.pack.extend_from_slice(content);
        let _ = self.offsets.insert(digest.to_owned(), offset);
        offset
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
                        let offset = self.packed(&digest, content);
                        item["source"] = serde_json::json!({"key": PACK, "offset": offset});
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

    /// The objects the upstream holds for these: each manifest and each file as
    /// `cas/l/<sha256>`, and the pack.
    fn objects(&self) -> Vec<(String, &[u8])> {
        let mut objects: Vec<(String, &[u8])> = Vec::new();
        for (digest, bytes) in self.manifests.iter().chain(&self.blobs) {
            objects.push((format!("cas/l/{digest}"), bytes.as_slice()));
        }
        if !self.pack.is_empty() {
            objects.push((PACK.to_owned(), self.pack.as_slice()));
        }
        objects
    }
}

/// The upstream of the node's cache: it holds the objects of a [`Built`], which a test may change
/// at any time, and keeps the fills the cache made of them in the words of the core's requests.
pub(in crate::server) struct Upstream {
    pub built: Mutex<Built>,
    /// The digests whose fill fails with these words: `upstream <status>` (an answer of that
    /// status), or `mismatch` (bytes that are not the digest's).
    pub failing: Mutex<HashMap<String, String>>,
    /// The fills received, in order, each in the words of the core's request: `fill <sha256>` for
    /// a manifest, `fill <sha256> <size> from <key> @ <offset>` for a blob.
    pub requests: Mutex<Vec<String>>,
    /// The objects, as the cache reads and writes them.
    store: MemoryBackend,
    /// What of the built objects is in `store`, by key.
    published: Mutex<HashMap<String, Bytes>>,
    /// Whether the fills of blobs wait (see [`Upstream::hold_fills`]).
    holding: Mutex<bool>,
    released: Condvar,
    /// The fills of manifests in flight (see [`Upstream::gate_manifests`]).
    gate: Mutex<Gate>,
    gate_changed: Condvar,
    /// The fills in flight, once a test closes their gate ([`Upstream::gate_verb`]).
    fills: Mutex<Option<Arc<VerbGate>>>,
}

/// The gate of the fills, and what wakes the fills that wait at it.
type VerbGate = (Mutex<Gate>, Condvar);

/// The progress of the fills, which a test may pace.
#[derive(Default)]
struct Gate {
    /// Whether a fill waits for a permit.
    gating: bool,
    /// The fills received.
    started: usize,
    /// The fills received and not answered, and the most there have been at once.
    in_flight: usize,
    most: usize,
    /// The permits given and the permits used.
    given: usize,
    used: usize,
    /// Whether the permits go to the fills in the order they were received (the gate of
    /// [`Upstream::gate_verb`]), and not to whichever asks first.
    ordered: bool,
}

/// What a read of the upstream is a fill of.
enum Fill {
    /// The manifest of this digest.
    Manifest(String),
    /// The blob of this digest, whose bytes begin at the read.
    Blob(String),
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new(Built::default())
    }
}

impl Upstream {
    /// An upstream that holds `built`.
    pub(in crate::server) fn new(built: Built) -> Self {
        Self {
            built: Mutex::new(built),
            failing: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            store: MemoryBackend::new(),
            published: Mutex::new(HashMap::new()),
            holding: Mutex::new(false),
            released: Condvar::new(),
            gate: Mutex::new(Gate::default()),
            gate_changed: Condvar::new(),
            fills: Mutex::new(None),
        }
    }

    /// Puts what `built` holds now in the store, where it is not there as it is.
    async fn publish(&self) {
        let objects: Vec<(String, Bytes)> = locked(&self.built)
            .objects()
            .into_iter()
            .map(|(key, bytes)| (key, Bytes::copy_from_slice(bytes)))
            .collect();
        for (key, bytes) in objects {
            if locked(&self.published).get(&key) == Some(&bytes) {
                continue;
            }
            let _ = self.store.delete(&key).await;
            self.store.put(&key, bytes.clone()).await.unwrap();
            let _ = locked(&self.published).insert(key, bytes);
        }
    }

    /// What a HEAD of `key` (`head`) or a read of `range` of it is a fill of: a HEAD of a
    /// manifest, or the read of a blob's first bytes. Other reads (the chunks after a blob's
    /// first, the GET of a manifest after its HEAD) are of no fill.
    fn fill_of(&self, key: &str, range: Option<ByteRange>) -> Option<(Fill, String)> {
        let built = locked(&self.built);
        let offset = range.map_or(0, |range| range.offset());
        if let Some(digest) = key.strip_prefix("cas/l/") {
            let manifest = built.manifests.contains_key(digest) || !built.blobs.contains_key(digest);
            return match (manifest, range) {
                (true, None) => Some((Fill::Manifest(digest.to_owned()), format!("fill {digest}"))),
                (false, Some(range)) if range.offset() == 0 => {
                    let size = built.blobs[digest].len();
                    let line = format!("fill {digest} {size} from {key} @ 0");
                    Some((Fill::Blob(digest.to_owned()), line))
                }
                _ => None,
            };
        }
        if key != PACK {
            return None;
        }
        let (digest, _) = built.offsets.iter().find(|(_, at)| **at == offset)?;
        let size = built.blobs[digest].len();
        let line = format!("fill {digest} {size} from {key} @ {offset}");
        Some((Fill::Blob(digest.clone()), line))
    }

    /// Takes the fill of a read: keeps its words, and waits at the gates a test closed. The
    /// failure the test named for its digest, if any.
    fn take(&self, fill: &Fill, line: String) -> Option<String> {
        locked(&self.requests).push(line);
        let gate = locked(&self.fills).clone();
        if let Some(gate) = &gate {
            tokio::task::block_in_place(|| Self::enter(&gate.0, &gate.1));
        }
        let digest = match fill {
            Fill::Manifest(digest) => {
                tokio::task::block_in_place(|| {
                    self.enter_manifest();
                    self.leave_manifest();
                });
                digest
            }
            Fill::Blob(digest) => {
                tokio::task::block_in_place(|| self.wait_while_held());
                digest
            }
        };
        if let Some(gate) = &gate {
            Self::leave(&gate.0, &gate.1);
        }
        locked(&self.failing).get(digest).cloned()
    }

    /// The error of a fill that fails with `why`.
    fn failure(key: &str, why: &str) -> Error {
        match why {
            "upstream 404" => Error::NotFound(key.to_owned()),
            "upstream 503" => Error::Unavailable(format!("{key}: the test says so")),
            other => panic!("the upstream cannot make a fill fail with {other:?}"),
        }
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
        Self::enter(&self.gate, &self.gate_changed);
    }

    fn leave_manifest(&self) {
        Self::leave(&self.gate, &self.gate_changed);
    }

    /// Makes every fill received from now on (`verb` is `fill`: the upstream sees no other
    /// request) wait for a permit of [`Upstream::permit_verb`], once received: for a test that
    /// needs a fill in flight.
    pub(in crate::server) fn gate_verb(&self, verb: &str) {
        assert_eq!(verb, "fill", "the upstream sees the fills alone");
        let gate = Gate {
            gating: true,
            ordered: true,
            ..Gate::default()
        };
        *locked(&self.fills) = Some(Arc::new((Mutex::new(gate), Condvar::new())));
    }

    /// The gate of the fills.
    fn verb(&self, verb: &str) -> Arc<VerbGate> {
        assert_eq!(verb, "fill", "the upstream sees the fills alone");
        locked(&self.fills).clone().expect("a gate of the fills")
    }

    /// Lets `count` more fills go on: the next ones in the order they were received.
    pub(in crate::server) fn permit_verb(&self, verb: &str, count: usize) {
        let gate = self.verb(verb);
        locked(&gate.0).given += count;
        gate.1.notify_all();
    }

    /// Waits until `count` fills have been received in all, since the gate was closed.
    pub(in crate::server) fn wait_for_verb(&self, verb: &str, count: usize) {
        let gate = self.verb(verb);
        let _gate = gate
            .1
            .wait_while(locked(&gate.0), |gate| gate.started < count)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The fills received in all, since the gate was closed.
    pub(in crate::server) fn received(&self, verb: &str) -> usize {
        locked(&self.verb(verb).0).started
    }

    /// The fills received and not answered now.
    pub(in crate::server) fn in_flight(&self, verb: &str) -> usize {
        locked(&self.verb(verb).0).in_flight
    }

    /// Counts a fill received on `gate`, and waits for a permit if the gate is closed.
    fn enter(gate: &Mutex<Gate>, changed: &Condvar) {
        let mut gate = locked(gate);
        gate.started += 1;
        gate.in_flight += 1;
        gate.most = gate.most.max(gate.in_flight);
        let mine = gate.started;
        changed.notify_all();
        while gate.gating
            && (if gate.ordered {
                mine > gate.given
            } else {
                gate.used >= gate.given
            })
        {
            gate = changed
                .wait(gate)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        gate.used += 1;
    }

    /// Counts a fill of `gate` answered.
    fn leave(gate: &Mutex<Gate>, changed: &Condvar) {
        locked(gate).in_flight -= 1;
        changed.notify_all();
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

    /// The requests that begin with `verb` (`fill`: the upstream sees no other).
    pub(in crate::server) fn asked(&self, verb: &str) -> Vec<String> {
        assert_eq!(verb, "fill", "the upstream sees the fills alone");
        locked(&self.requests).clone()
    }

    /// The objects the cache put, by key: what the put directory's puts made.
    pub(in crate::server) async fn put_objects(&self) -> Vec<(String, Bytes)> {
        let built: Vec<String> = locked(&self.published).keys().cloned().collect();
        let mut objects = Vec::new();
        for listed in self.store.list("").await.unwrap() {
            if !built.contains(&listed.key) {
                let bytes = self.store.get(&listed.key, None).await.unwrap();
                objects.push((listed.key, bytes));
            }
        }
        objects
    }
}

#[async_trait]
impl Backend for Upstream {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.store.put(key, body).await
    }

    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        self.store.put_tagged(key, body).await
    }

    async fn put_stream_answered(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        self.store
            .put_stream_answered(key, len, body, hot_only)
            .await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.store.put_if_match(key, body, etag).await
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        etag: &str,
    ) -> Result<String, Error> {
        self.store.put_stream_if_match(key, len, body, etag).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.store.delete_if_match(key, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.publish().await;
        let why = match self.fill_of(key, range) {
            Some((fill, line)) => self.take(&fill, line),
            None => None,
        };
        // A manifest's fill fails at its HEAD; its bytes, read after, are the digest's or not.
        let why = why.or_else(|| {
            let digest = key.strip_prefix("cas/l/")?;
            let why = locked(&self.failing).get(digest).cloned()?;
            (why == "mismatch").then_some(why)
        });
        match why.as_deref() {
            Some("mismatch") => {
                let size = range.map_or(1, ByteRange::size);
                Ok(Bytes::from(vec![0xa5; usize::try_from(size).unwrap()]))
            }
            Some(why) => Err(Self::failure(key, why)),
            None => self.store.get(key, range).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.publish().await;
        if let Some((fill @ Fill::Manifest(_), line)) = self.fill_of(key, None)
            && let Some(why) = self.take(&fill, line)
            && why != "mismatch"
        {
            return Err(Self::failure(key, &why));
        }
        self.store.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.store.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.publish().await;
        self.store.list(prefix).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.store.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.store.upload_part(key, id, number, body).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.store.complete_upload(key, id, parts).await
    }

    async fn complete_upload_if_match(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
        etag: &str,
    ) -> Result<String, Error> {
        self.store
            .complete_upload_if_match(key, id, parts, etag)
            .await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.store.abort_upload(key, id).await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        self.store.list_uploads(prefix, after, max_uploads).await
    }

    async fn list_parts(&self, key: &str, id: &str) -> Result<Vec<UploadedPart>, Error> {
        self.store.list_parts(key, id).await
    }
}

/// A runtime the cache's work runs on, shut down without waiting when dropped, so that a rig made
/// in a test that is itself async may be dropped there.
pub(in crate::server) struct Work(Option<Runtime>);

impl Work {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        Self(Some(runtime))
    }

    /// The handle of the runtime.
    pub(in crate::server) fn handle(&self) -> tokio::runtime::Handle {
        self.0.as_ref().unwrap().handle().clone()
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// The node's cache, over an [`Upstream`], in the directory `cache` of `dir`, opened on `work`.
fn node_cache(cache: &Path, upstream: &Arc<Upstream>, work: &Work) -> LocalCache {
    let mut config = CacheConfig::new(cache, 1 << 30, vec![("cas/".to_owned(), true)]);
    crate::server::local_cache::testing::never_expires(&mut config);
    let backend: Arc<dyn Backend> = upstream.clone();
    let handle = work.handle();
    let _entered = handle.enter();
    LocalCache::open(
        config,
        backend,
        Arc::new(FakeDisk::new()),
        Arc::new(TestClock::new()),
    )
    .unwrap()
}

/// A core over the node's cache, called in the process, in scratch directories.
pub(in crate::server) struct Rig {
    pub core: Arc<Core>,
    pub upstream: Arc<Upstream>,
    /// The node's cache, which the core calls.
    pub node: LocalCache,
    /// The clock of the core, which only the test moves.
    pub clock: Arc<Mono>,
    pub cache: PathBuf,
    pub runtime: PathBuf,
    pub mountpoint: PathBuf,
    /// The runtime the work of the cache runs on.
    pub work: Work,
    /// Last: the directories go after everything that is in them.
    pub dir: TestDir,
}

impl Rig {
    /// Settings of the file system over the scratch directories of the rig: the mount point, the
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
            cache_path: self.cache.clone(),
            fetch_timeout: Duration::from_secs(5),
            threads: 4,
            handoff_socket: self.dir.path().join("handoff.sock"),
            handoff_wait: Duration::from_secs(30),
            recover_wait: Duration::from_secs(30),
            reattach_concurrency: 4,
            tree_idle: TREE_IDLE,
            tree_list_ram_bytes: TREE_LIST_RAM,
            allow_other: false,
            nfs: crate::server::treefs::nfs::config::NfsSettings::default(),
            put: None,
        }
    }

    /// The node's cache, and no tree store, as a start of the file system calls them.
    pub(in crate::server::treefs) fn calls(&self) -> Calls {
        Calls {
            cache: self.node.clone(),
            trees: None,
            runtime: self.work.handle(),
        }
    }

    /// What a core of the rig calls: the node's cache, each request waiting at most 30 seconds,
    /// with no pause between the core's tries.
    pub(in crate::server::treefs) fn asks(&self) -> Arc<InProcess> {
        self.asks_pausing(Arc::new(|_| {}))
    }

    /// [`Rig::asks`], with the pauses between the core's tries taken by `pause`.
    pub(in crate::server::treefs) fn asks_pausing(
        &self,
        pause: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Arc<InProcess> {
        Arc::new(InProcess::new(
            (self.node.clone(), None),
            self.work.handle(),
            Duration::from_secs(30),
            pause,
        ))
    }

    /// The core of the process that takes over from the one of this rig: the generation after
    /// it, over the same table, runtime directory and cache.
    pub(in crate::server::treefs) fn successor(&self) -> Arc<Core> {
        let mut settings = self.core.settings().clone();
        settings.generation += 1;
        Arc::new(Core::new(
            settings,
            self.asks(),
            TreeTable::open(&self.runtime).unwrap(),
            &self.runtime,
        ))
    }
}

/// Tests that send signals to the process go one at a time: a signal is for every handler.
pub(in crate::server::treefs) static SIGNALS: Mutex<()> = Mutex::new(());

/// How long a tree a lookup attached stays attached with no use, in the rigs.
pub(in crate::server) const TREE_IDLE: Duration = Duration::from_secs(60);

/// The bytes of tree lists a rig keeps mapped, but where a test sets its own (`Rig::capped`).
pub(in crate::server) const TREE_LIST_RAM: u64 = 512 << 20;

/// The owner every entry of the rigs reports.
pub(in crate::server) const OWNER: (u32, u32) = (1234, 5678);

impl Rig {
    /// A rig whose upstream holds `built`.
    pub(in crate::server) fn new(name: &str, built: Built) -> Self {
        Self::owned(name, built, OWNER)
    }

    /// A rig whose entries report `owner`.
    pub(in crate::server) fn owned(name: &str, built: Built, owner: (u32, u32)) -> Self {
        Self::made(name, built, (owner, TREE_LIST_RAM), true)
    }

    /// A rig whose core keeps at most `cap` bytes of tree lists mapped (`fs.tree_list_ram_bytes`).
    pub(in crate::server) fn capped(name: &str, built: Built, cap: u64) -> Self {
        Self::made(name, built, (OWNER, cap), true)
    }

    /// A rig whose core does not count lookups: a frontend with no forget.
    pub(in crate::server) fn uncounted(name: &str, built: Built) -> Self {
        Self::made(name, built, (OWNER, TREE_LIST_RAM), false)
    }

    /// A second core over the same cache, with a table of tree numbers and a runtime directory of
    /// its own: what a restart makes, before it attaches anything.
    pub(in crate::server) fn second_core(&self) -> Arc<Core> {
        let runtime = self.dir.path().join("run2");
        std::fs::create_dir_all(&runtime).unwrap();
        Arc::new(Core::new(
            Settings {
                owner: OWNER,
                cache_path: self.cache.clone(),
                mountpoint: self.mountpoint.clone(),
                mount_id: "mount-2".to_owned(),
                generation: 1,
                count_lookups: false,
                tree_idle: TREE_IDLE,
                tree_list_ram_bytes: TREE_LIST_RAM,
                clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            },
            self.asks(),
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ))
    }

    fn made(
        name: &str,
        built: Built,
        (owner, cap): ((u32, u32), u64),
        count_lookups: bool,
    ) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let cache = dir.path().join("cache");
        let runtime = dir.path().join("run");
        let mountpoint = dir.path().join("mnt");
        for path in [&cache, &runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let upstream = Arc::new(Upstream::new(built));
        let work = Work::new();
        let node = node_cache(&cache, &upstream, &work);
        let clock = Arc::new(Mono::new());
        let asks = Arc::new(InProcess::new(
            (node.clone(), None),
            work.handle(),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        ));
        let core = Arc::new(Core::new(
            Settings {
                owner,
                cache_path: cache.clone(),
                mountpoint: mountpoint.clone(),
                mount_id: "mount-1".to_owned(),
                generation: 1,
                count_lookups,
                tree_idle: TREE_IDLE,
                tree_list_ram_bytes: cap,
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
            },
            asks,
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        ));
        Self {
            core,
            upstream,
            node,
            clock,
            cache,
            runtime,
            mountpoint,
            work,
            dir,
        }
    }
}

/// The kernel's backing ids, as the tests of the core have them: the ids an earlier release
/// registered, each closed when the last reference to its handle is dropped.
pub(in crate::server::treefs) struct FakeKernel {
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
            closed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The handle on the id `id`, which an earlier release registered.
    pub(in crate::server::treefs) fn wrap(&self, id: u32) -> Guard {
        Arc::new(Closes {
            id,
            closed: Arc::clone(&self.closed),
        })
    }

    /// The id a handle of a [`FakeKernel`] is on; none for a handle of another kind.
    pub(in crate::server::treefs) fn id_of(guard: &Guard) -> Option<u32> {
        guard.downcast_ref::<Closes>().map(|closes| closes.id)
    }
}

/// An open of `ino` on `core`: its handle, whether it is cached or answered with an adopted id.
pub(in crate::server::treefs) fn open_cached(
    core: &Core,
    ino: u64,
    writing: bool,
) -> Result<u64, Errno> {
    match core.open(ino, writing)? {
        Opened::Cached(fh) | Opened::Adopted { fh, .. } => Ok(fh),
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
        let told =
            UnixDatagram::unbound().and_then(|socket| socket.send_to(STOP.as_bytes(), &self.path));
        // A manager whose socket is gone (its directory was removed before it) cannot be told to
        // stop: its thread is left to the end of the process rather than waited for forever.
        if told.is_ok() {
            let _ = self.thread.take().map(std::thread::JoinHandle::join);
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

/// The lines this crate logs on a thread while a test holds the guard of [`logged`]: the
/// operator's stderr, as the process writes it.
#[derive(Clone, Default)]
pub(in crate::server) struct Logged(Arc<Mutex<Vec<u8>>>);

impl Logged {
    /// The lines written so far.
    pub(in crate::server) fn text(&self) -> String {
        String::from_utf8_lossy(&locked(&self.0)).into_owned()
    }
}

impl std::io::Write for Logged {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        locked(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Logs what this thread says at `level` and above to the [`Logged`] given, until the guard
/// drops.
pub(in crate::server) fn logged(
    log: &Logged,
    level: tracing::Level,
) -> tracing::dispatcher::DefaultGuard {
    let line_log = crate::server::build_info::stderr_log::LineLog::new(log.clone(), level);
    tracing::subscriber::set_default(line_log)
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
