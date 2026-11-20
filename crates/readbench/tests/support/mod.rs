//! An in-process fake of the farm's object store, for the tests of `readbench`.
//!
//! The fake is SeaweedFS as the harness sees it: filers that serve objects by path
//! (`/buckets/<bucket>/<key>`) and volume servers that serve the pieces of those objects
//! (`/<volume id>,<needle hex>`) and a status page (`/status`). Every node is its own listener on
//! `127.0.0.1`, port 0, served by tasks of the test's runtime; dropping the [`Fake`] stops them.
//!
//! ```ignore
//! let fake = Fake::builder()
//!     .filers(1)                     // one filer (the default)
//!     .volume_servers(2)             // two volume servers
//!     .volume(7, &[0, 1], 1)         // volume 7 is on both servers, on their disk 1
//!     .chunk_size(4)                 // the filer splits objects in chunks of 4 bytes
//!     .object("kbf-cas", "cas/l/ab", b"hello world".to_vec())
//!     .start()
//!     .await;
//! let url = fake.filer(0).url("/buckets/kbf-cas/cas/l/ab");
//! ```
//!
//! What it serves:
//! - A filer answers `GET` and `HEAD` of an object with its bytes, `Content-Length`, `ETag`
//!   (quoted, 32 hex digits of the bytes' SHA-256) and `Accept-Ranges`; `Range: bytes=a-b` and
//!   `bytes=a-` are answered `206` with `Content-Range` (the end is clamped to the object, a start
//!   past it is `416`); any other `Range` is ignored and the whole object is answered. With
//!   `?metadata=true` and `Accept: application/json` it answers
//!   `{"FullPath", "FileSize", "Mtime", "Md5": null, "chunks": [{"file_id", "offset", "size", ..}]}`.
//!   A path it does not hold is `404`.
//! - A filer answers `GET` of a directory path (one ending in `/`, under `/buckets/`) with
//!   `Accept: application/json` as the real filer's listing: `{"Path", "Entries": [{"FullPath",
//!   "FileSize", "Mtime"}, ..], "Limit", "LastFileName", "ShouldDisplayLoadMore"}`. The entries
//!   are the objects directly under the directory, by name, those named after `?lastFileName=`
//!   only, at most `?limit=` of them (default 100); `ShouldDisplayLoadMore` says whether more
//!   follow, and `LastFileName` is the last name listed. Objects added with
//!   [`FakeBuilder::object_at`] carry their `Mtime`, the others `2026-01-01T00:00:00Z`.
//! - A volume server answers `GET` and `HEAD` of a needle it holds (no `ETag`), with ranges as the
//!   filer does, and `GET /status` with `{"Version", "DiskStatuses": [{"dir", ..}], "Volumes":
//!   [{"Collection", "Id", "DiskId", "Size"}]}` for the volumes declared on it.
//! - Any method but `HEAD` is answered as `GET`.
//!
//! Every object is split in chunks of at most [`FakeBuilder::chunk_size`] bytes. The chunk with
//! number `n` of the whole fake (counted from 0 in the order objects were added) goes to volume
//! `volumes[n % volumes.len()]` unless the object was added with [`FakeBuilder::object_on`]; its
//! file id is `"<volume>,<n + 1 as 8 hex digits>"`, and its bytes are held by every server the
//! volume is declared on.
//!
//! Tests read what each node was asked ([`NodeHandle::count`], [`NodeHandle::path_count`],
//! [`NodeHandle::connections`], [`NodeHandle::bytes_served`]) and script failures per node and per
//! request path (query excluded): [`NodeHandle::fail_status`], [`NodeHandle::cut_after`],
//! [`NodeHandle::ignore_range`], [`NodeHandle::gate`], [`NodeHandle::clear_rules`]. A request is
//! counted when it arrives, before its path's gate and failure are applied.
//!
//! A test crate that uses part of this module only must say so for the rest with
//! `#[expect(dead_code, reason = "...")]` on `mod support;`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::io::{self, Read as _, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use http::header::{ACCEPT, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, RANGE};
use http::{HeaderValue, Method, Request, Response};
use http_body_util::BodyExt;
use http_body_util::StreamBody;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

/// The body type of every answer.
type Body = UnsyncBoxBody<Bytes, io::Error>;

/// The kind of a request, as a node counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A filer's `GET` of an object's bytes.
    FilerGet,
    /// A filer's `HEAD`.
    FilerHead,
    /// A filer's `GET` of a directory path (one ending in `/`): its listing.
    FilerList,
    /// A filer's `GET` with `?metadata=true` and `Accept: application/json`.
    FilerMetadata,
    /// A volume server's `GET` of a needle (or of any path but `/status`).
    VolumeGet,
    /// A volume server's `HEAD` of a needle.
    VolumeHead,
    /// A volume server's `/status`.
    VolumeStatus,
    /// A request a filer got for a path outside `/buckets/`.
    Other,
}

/// One chunk of an object, as the filer's metadata names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    /// The file id: `"<volume>,<needle hex>"`.
    pub fid: String,
    /// The volume id.
    pub volume: u32,
    /// Where the chunk starts in the object.
    pub offset: u64,
    /// The chunk's length.
    pub size: u64,
}

/// A request path that holds back its answers until a test opens it. Clones share the gate.
#[derive(Debug, Clone)]
pub struct Gate {
    open: Arc<watch::Sender<bool>>,
    arrived: Arc<watch::Sender<u64>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            open: Arc::new(watch::Sender::new(false)),
            arrived: Arc::new(watch::Sender::new(0)),
        }
    }

    /// Lets every request that waits at the gate, and every later one, through.
    pub fn open(&self) {
        self.open.send_replace(true);
    }

    /// Waits until `n` requests have reached the gate (the open ones count too).
    pub async fn arrived(&self, n: u64) {
        let mut seen = self.arrived.subscribe();
        let _ = seen.wait_for(|count| *count >= n).await;
    }

    /// Called by a request: counts it, then waits for the gate to open.
    async fn pass(&self) {
        self.arrived.send_modify(|count| *count += 1);
        let mut open = self.open.subscribe();
        let _ = open.wait_for(|is_open| *is_open).await;
    }
}

/// An object held by the filers.
#[derive(Debug)]
struct Object {
    data: Bytes,
    etag: String,
    mtime: String,
    chunks: Vec<ChunkInfo>,
}

/// An object the builder was given, not yet split in chunks.
#[derive(Debug)]
struct Pending {
    bucket: String,
    key: String,
    data: Vec<u8>,
    volume: Option<u32>,
    mtime: Option<String>,
}

/// A volume declared on some servers.
#[derive(Debug)]
struct VolumeDecl {
    id: u32,
    servers: Vec<usize>,
    disk: u32,
}

/// What a volume server holds.
#[derive(Debug, Default)]
struct VolumeData {
    needles: BTreeMap<String, Bytes>,
    /// Volume id, disk id, bytes held.
    volumes: BTreeMap<u32, (u32, u64)>,
    disks: u32,
}

/// What a node serves.
#[derive(Debug)]
enum Role {
    Filer(Arc<BTreeMap<String, Object>>),
    Volume(VolumeData),
}

/// A failure scripted for a path: `status` for the next `left` requests (every one if `None`).
#[derive(Debug)]
struct Fail {
    status: u16,
    left: Option<u32>,
}

/// The failures scripted on a node.
#[derive(Debug, Default)]
struct Rules {
    fail: HashMap<String, Fail>,
    cut: HashMap<String, u64>,
    gates: HashMap<String, Gate>,
    ignore_range: HashSet<String>,
}

/// What a request found when it was counted and what the node scripted for its path.
struct Script {
    gate: Option<Gate>,
    fail: Option<u16>,
    cut: Option<u64>,
    ignore_range: bool,
}

/// The state of one node, shared by its connections.
#[derive(Debug)]
struct NodeState {
    role: Role,
    counts: Mutex<BTreeMap<Kind, u64>>,
    paths: Mutex<BTreeMap<String, u64>>,
    rules: Mutex<Rules>,
    connections: AtomicU64,
    bytes_served: AtomicU64,
}

/// Locks a mutex whose holder panicked all the same: a test's failure is the thing reported.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One listening node of the fake, and the handle a test scripts and reads it with. Dropping the
/// handle stops the node.
#[derive(Debug)]
pub struct NodeHandle {
    addr: String,
    state: Arc<NodeState>,
    task: JoinHandle<()>,
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl NodeHandle {
    /// The node's address, `127.0.0.1:<port>`.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// `http://<addr><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// How many requests of `kind` the node has been sent.
    pub fn count(&self, kind: Kind) -> u64 {
        lock(&self.state.counts).get(&kind).copied().unwrap_or(0)
    }

    /// How many requests the node has been sent for `path` (the query is not part of it).
    pub fn path_count(&self, path: &str) -> u64 {
        lock(&self.state.paths).get(path).copied().unwrap_or(0)
    }

    /// How many connections the node has accepted.
    pub fn connections(&self) -> u64 {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// The bytes of body the node began to send: each answer's whole body, or the part of it
    /// before a cut (see [`NodeHandle::cut_after`]).
    pub fn bytes_served(&self) -> u64 {
        self.state.bytes_served.load(Ordering::SeqCst)
    }

    /// Answers the next `times` requests for `path` (every one if `None`) with `status` and no
    /// body.
    pub fn fail_status(&self, path: &str, status: u16, times: Option<u32>) {
        lock(&self.state.rules).fail.insert(
            path.to_owned(),
            Fail {
                status,
                left: times,
            },
        );
    }

    /// Sends the first `bytes` bytes of the body of every `GET` of `path` and then breaks the
    /// connection, after the headers promised the whole body.
    pub fn cut_after(&self, path: &str, bytes: u64) {
        lock(&self.state.rules).cut.insert(path.to_owned(), bytes);
    }

    /// Makes `path` answer a ranged `GET` with the whole object and `200`, as a server that does
    /// not know ranges does.
    pub fn ignore_range(&self, path: &str) {
        lock(&self.state.rules).ignore_range.insert(path.to_owned());
    }

    /// Holds back every request for `path`, from now on, until the returned gate is opened.
    pub fn gate(&self, path: &str) -> Gate {
        let gate = Gate::new();
        lock(&self.state.rules)
            .gates
            .insert(path.to_owned(), gate.clone());
        gate
    }

    /// Removes every failure, cut, ignored range and gate scripted for `path`.
    pub fn clear_rules(&self, path: &str) {
        let mut rules = lock(&self.state.rules);
        rules.fail.remove(path);
        rules.cut.remove(path);
        rules.gates.remove(path);
        rules.ignore_range.remove(path);
    }
}

/// The fake store: its filers and volume servers.
#[derive(Debug)]
pub struct Fake {
    filers: Vec<NodeHandle>,
    volumes: Vec<NodeHandle>,
    objects: Arc<BTreeMap<String, Object>>,
}

impl Fake {
    /// A builder of a fake with one filer, one volume server and no object.
    pub fn builder() -> FakeBuilder {
        FakeBuilder {
            filers: 1,
            volume_servers: 1,
            disks: 2,
            chunk_size: 64 << 20,
            volumes: Vec::new(),
            objects: Vec::new(),
        }
    }

    /// Filer number `i`.
    pub fn filer(&self, i: usize) -> &NodeHandle {
        &self.filers[i]
    }

    /// Volume server number `i`.
    pub fn volume(&self, i: usize) -> &NodeHandle {
        &self.volumes[i]
    }

    /// The chunks of an object, in order of offset (none for an object the filers do not hold).
    pub fn chunks(&self, bucket: &str, key: &str) -> Vec<ChunkInfo> {
        self.objects
            .get(&object_path(bucket, key))
            .map(|o| o.chunks.clone())
            .unwrap_or_default()
    }

    /// How many requests of `kind` all the nodes together have been sent.
    pub fn total(&self, kind: Kind) -> u64 {
        self.filers
            .iter()
            .chain(&self.volumes)
            .map(|n| n.count(kind))
            .sum()
    }
}

/// The builder of a [`Fake`].
#[derive(Debug)]
pub struct FakeBuilder {
    filers: usize,
    volume_servers: usize,
    disks: u32,
    chunk_size: u64,
    volumes: Vec<VolumeDecl>,
    objects: Vec<Pending>,
}

impl FakeBuilder {
    /// The number of filers (default 1). Every filer holds every object.
    #[must_use]
    pub fn filers(mut self, n: usize) -> Self {
        self.filers = n;
        self
    }

    /// The least number of volume servers (default 1); a server a volume is declared on exists
    /// whatever this says.
    #[must_use]
    pub fn volume_servers(mut self, n: usize) -> Self {
        self.volume_servers = n;
        self
    }

    /// The number of disks every volume server lists in its `/status` (default 2, directories
    /// `/mnt/hdd1/swfs` ...).
    #[must_use]
    pub fn disks(mut self, n: u32) -> Self {
        self.disks = n;
        self
    }

    /// The largest chunk of an object, in bytes (default 64 MiB; 0 counts as 1).
    #[must_use]
    pub fn chunk_size(mut self, bytes: u64) -> Self {
        self.chunk_size = bytes.max(1);
        self
    }

    /// Declares volume `id` on the volume servers `servers` (by index), on their disk `disk`.
    /// Without any declaration the fake has volume 1 on server 0, disk 1.
    #[must_use]
    pub fn volume(mut self, id: u32, servers: &[usize], disk: u32) -> Self {
        self.volumes.push(VolumeDecl {
            id,
            servers: servers.to_vec(),
            disk,
        });
        self
    }

    /// Adds the object `key` of `bucket`, its chunks spread over the declared volumes.
    #[must_use]
    pub fn object(mut self, bucket: &str, key: &str, data: Vec<u8>) -> Self {
        self.objects.push(Pending {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            data,
            volume: None,
            mtime: None,
        });
        self
    }

    /// Adds the object `key` of `bucket`, all its chunks on volume `volume`, declared or not (a
    /// volume no server is declared for has chunks nobody can serve).
    #[must_use]
    pub fn object_on(mut self, bucket: &str, key: &str, data: Vec<u8>, volume: u32) -> Self {
        self.objects.push(Pending {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            data,
            volume: Some(volume),
            mtime: None,
        });
        self
    }

    /// Adds the object `key` of `bucket`, last modified at `mtime` (RFC 3339 text), its chunks
    /// spread over the declared volumes.
    #[must_use]
    pub fn object_at(mut self, bucket: &str, key: &str, data: Vec<u8>, mtime: &str) -> Self {
        self.objects.push(Pending {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            data,
            volume: None,
            mtime: Some(mtime.to_owned()),
        });
        self
    }

    /// Binds the listeners and starts serving.
    ///
    /// # Panics
    /// If a loopback port cannot be bound.
    pub async fn start(mut self) -> Fake {
        if self.volumes.is_empty() {
            self.volumes.push(VolumeDecl {
                id: 1,
                servers: vec![0],
                disk: 1,
            });
        }
        let server_count = self
            .volumes
            .iter()
            .flat_map(|v| v.servers.iter().map(|s| s + 1))
            .fold(self.volume_servers, usize::max);
        let mut datas: Vec<VolumeData> = (0..server_count)
            .map(|_| VolumeData {
                disks: self.disks,
                ..VolumeData::default()
            })
            .collect();
        for decl in &self.volumes {
            for server in &decl.servers {
                datas[*server].volumes.insert(decl.id, (decl.disk, 0));
            }
        }
        let ids: Vec<u32> = self.volumes.iter().map(|v| v.id).collect();
        let mut objects = BTreeMap::new();
        let mut number = 0_u64;
        for pending in &self.objects {
            let mut chunks = Vec::new();
            let mut offset = 0_u64;
            let step = usize::try_from(self.chunk_size).unwrap_or(usize::MAX);
            for piece in pending.data.chunks(step) {
                let volume = pending
                    .volume
                    .unwrap_or(ids[usize::try_from(number).unwrap_or(0) % ids.len()]);
                number += 1;
                let fid = format!("{volume},{number:08x}");
                let size = piece.len() as u64;
                for decl in self.volumes.iter().filter(|d| d.id == volume) {
                    for server in &decl.servers {
                        datas[*server]
                            .needles
                            .insert(fid.clone(), Bytes::copy_from_slice(piece));
                        datas[*server]
                            .volumes
                            .entry(volume)
                            .and_modify(|held| held.1 += size);
                    }
                }
                chunks.push(ChunkInfo {
                    fid,
                    volume,
                    offset,
                    size,
                });
                offset += size;
            }
            let digest = ring::digest::digest(&ring::digest::SHA256, &pending.data);
            objects.insert(
                object_path(&pending.bucket, &pending.key),
                Object {
                    data: Bytes::from(pending.data.clone()),
                    etag: hex::encode(&digest.as_ref()[..16]),
                    mtime: pending
                        .mtime
                        .clone()
                        .unwrap_or_else(|| "2026-01-01T00:00:00Z".to_owned()),
                    chunks,
                },
            );
        }
        let objects = Arc::new(objects);
        let mut filers = Vec::new();
        for _ in 0..self.filers {
            filers.push(spawn_node(Role::Filer(objects.clone())).await);
        }
        let mut volumes = Vec::new();
        for data in datas {
            volumes.push(spawn_node(Role::Volume(data)).await);
        }
        Fake {
            filers,
            volumes,
            objects,
        }
    }
}

/// The filer path of an object.
fn object_path(bucket: &str, key: &str) -> String {
    format!("/buckets/{bucket}/{key}")
}

/// Binds a loopback port and serves `role` on it.
async fn spawn_node(role: Role) -> NodeHandle {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener
        .local_addr()
        .expect("the address of a bound listener")
        .to_string();
    let state = Arc::new(NodeState {
        role,
        counts: Mutex::default(),
        paths: Mutex::default(),
        rules: Mutex::default(),
        connections: AtomicU64::new(0),
        bytes_served: AtomicU64::new(0),
    });
    let task = tokio::spawn(accept_loop(listener, state.clone()));
    NodeHandle { addr, state, task }
}

/// Accepts connections for ever (the node's task is aborted to stop it, which drops the set and
/// with it every connection).
async fn accept_loop(listener: TcpListener, state: Arc<NodeState>) {
    let mut connections = JoinSet::new();
    loop {
        let (stream, _) = listener.accept().await.expect("accept a connection");
        state.connections.fetch_add(1, Ordering::SeqCst);
        let state = state.clone();
        connections.spawn(async move {
            let service = service_fn(move |req| handle(state.clone(), req));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// An answer before it becomes an HTTP response.
struct Reply {
    status: u16,
    headers: Vec<(http::HeaderName, String)>,
    body: Bytes,
    /// The `Content-Length` to announce (a `HEAD`'s is the body's it does not send).
    announced: u64,
}

impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
            announced: 0,
        }
    }

    fn json(value: &serde_json::Value) -> Self {
        let body = Bytes::from(value.to_string());
        Self {
            status: 200,
            headers: vec![(CONTENT_TYPE, "application/json".to_owned())],
            announced: body.len() as u64,
            body,
        }
    }
}

/// One request, as `handle` hands it to the node.
async fn handle(
    node: Arc<NodeState>,
    req: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let head = req.method() == Method::HEAD;
    let path = req.uri().path().to_owned();
    let metadata = req
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|p| p == "metadata=true"))
        && header(&req, ACCEPT).is_some_and(|v| v.contains("application/json"));
    let range = header(&req, RANGE);
    let query = req.uri().query().unwrap_or_default().to_owned();
    node.record(node.classify(head, &path, metadata), &path);
    let script = node.script(&path);
    if let Some(gate) = &script.gate {
        gate.pass().await;
    }
    let reply = match script.fail {
        Some(status) => Reply::empty(status),
        None => node.serve(
            &path,
            &query,
            metadata,
            range.as_deref(),
            script.ignore_range,
        ),
    };
    Ok(into_response(&node, reply, head, script.cut))
}

/// A header's value as text.
fn header(req: &Request<Incoming>, name: http::HeaderName) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

impl NodeState {
    /// The kind of a request.
    fn classify(&self, head: bool, path: &str, metadata: bool) -> Kind {
        match (&self.role, head) {
            (Role::Filer(_), _) if !path.starts_with("/buckets/") => Kind::Other,
            (Role::Filer(_), true) => Kind::FilerHead,
            (Role::Filer(_), false) if path.ends_with('/') => Kind::FilerList,
            (Role::Filer(_), false) if metadata => Kind::FilerMetadata,
            (Role::Filer(_), false) => Kind::FilerGet,
            (Role::Volume(_), _) if path == "/status" => Kind::VolumeStatus,
            (Role::Volume(_), true) => Kind::VolumeHead,
            (Role::Volume(_), false) => Kind::VolumeGet,
        }
    }

    /// Counts a request.
    fn record(&self, kind: Kind, path: &str) {
        *lock(&self.counts).entry(kind).or_default() += 1;
        *lock(&self.paths).entry(path.to_owned()).or_default() += 1;
    }

    /// What is scripted for `path`; a counted failure is used up.
    fn script(&self, path: &str) -> Script {
        let mut rules = lock(&self.rules);
        let fail = match rules.fail.get_mut(path) {
            None => None,
            Some(Fail { left: None, status }) => Some(*status),
            Some(Fail { left: Some(0), .. }) => None,
            Some(Fail {
                left: Some(left),
                status,
            }) => {
                *left -= 1;
                Some(*status)
            }
        };
        Script {
            gate: rules.gates.get(path).cloned(),
            fail,
            cut: rules.cut.get(path).copied(),
            ignore_range: rules.ignore_range.contains(path),
        }
    }

    /// The answer for a path the node holds, or `404`.
    fn serve(
        &self,
        path: &str,
        query: &str,
        metadata: bool,
        range: Option<&str>,
        ignore_range: bool,
    ) -> Reply {
        let range = if ignore_range { None } else { range };
        match &self.role {
            Role::Filer(objects) if path.ends_with('/') => {
                Reply::json(&listing(objects, path, query))
            }
            Role::Filer(objects) => match objects.get(path) {
                None => Reply::empty(404),
                Some(object) if metadata => Reply::json(&json!({
                    "FullPath": path,
                    "FileSize": object.data.len(),
                    "Mtime": object.mtime,
                    "Md5": null,
                    "chunks": object.chunks.iter().map(|c| json!({
                        "file_id": c.fid,
                        "offset": c.offset,
                        "size": c.size,
                        "modified_ts_ns": 0,
                        "e_tag": "",
                    })).collect::<Vec<_>>(),
                })),
                Some(object) => serve_bytes(&object.data, Some(&object.etag), range),
            },
            Role::Volume(data) if path == "/status" => Reply::json(&json!({
                "Version": "30GB 4.54 fake",
                "DiskStatuses": (1..=data.disks).map(|d| json!({
                    "dir": format!("/mnt/hdd{d}/swfs"),
                    "all": 1_000_000_000_u64,
                })).collect::<Vec<_>>(),
                "Volumes": data.volumes.iter().map(|(id, (disk, size))| json!({
                    "Collection": "kbf-cas",
                    "Id": id,
                    "DiskId": disk,
                    "Size": size,
                })).collect::<Vec<_>>(),
            })),
            Role::Volume(data) => match data.needles.get(path.trim_start_matches('/')) {
                None => Reply::empty(404),
                Some(needle) => serve_bytes(needle, None, range),
            },
        }
    }
}

/// The listing of the directory `dir` (a path ending in `/`) as the filer answers it for `query`.
fn listing(objects: &BTreeMap<String, Object>, dir: &str, query: &str) -> serde_json::Value {
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
            .unwrap_or_default()
    };
    let limit: usize = param("limit").parse().unwrap_or(100);
    let after = param("lastFileName");
    let mut named: Vec<(&str, &Object)> = objects
        .iter()
        .filter_map(|(path, o)| Some((path.strip_prefix(dir)?, o)))
        .filter(|(name, _)| !name.is_empty() && !name.contains('/') && *name > after)
        .collect();
    let more = named.len() > limit;
    named.truncate(limit);
    json!({
        "Path": dir.trim_end_matches('/'),
        "Entries": named.iter().map(|(name, o)| json!({
            "FullPath": format!("{dir}{name}"),
            "FileSize": o.data.len(),
            "Mtime": o.mtime,
        })).collect::<Vec<_>>(),
        "Limit": limit,
        "LastFileName": named.last().map_or("", |(name, _)| name),
        "ShouldDisplayLoadMore": more,
    })
}

/// The bytes of an object or needle: all of it, or the range asked for.
fn serve_bytes(data: &Bytes, etag: Option<&str>, range: Option<&str>) -> Reply {
    let len = data.len() as u64;
    let mut headers = vec![(http::header::ACCEPT_RANGES, "bytes".to_owned())];
    if let Some(etag) = etag {
        headers.push((ETAG, format!("\"{etag}\"")));
    }
    let Some((start, end)) = range.and_then(parse_range) else {
        return Reply {
            status: 200,
            headers,
            announced: len,
            body: data.clone(),
        };
    };
    let last = end.min(len.saturating_sub(1));
    if start >= len || start > last {
        return Reply {
            status: 416,
            headers: vec![(CONTENT_RANGE, format!("bytes */{len}"))],
            body: Bytes::new(),
            announced: 0,
        };
    }
    headers.push((CONTENT_RANGE, format!("bytes {start}-{last}/{len}")));
    // `start <= last < len`, and `len` is the length of an in-memory buffer.
    let body = data.slice(usize::try_from(start).unwrap_or(0)..=usize::try_from(last).unwrap_or(0));
    Reply {
        status: 206,
        headers,
        announced: body.len() as u64,
        body,
    }
}

/// `bytes=a-b` as `(a, b)` and `bytes=a-` as `(a, u64::MAX)`; nothing for any other `Range`.
fn parse_range(spec: &str) -> Option<(u64, u64)> {
    let (start, end) = spec.strip_prefix("bytes=")?.split_once('-')?;
    let end = if end.is_empty() {
        u64::MAX
    } else {
        end.parse().ok()?
    };
    Some((start.parse().ok()?, end))
}

/// Makes the HTTP response of a reply, counting the body it sends.
fn into_response(node: &NodeState, reply: Reply, head: bool, cut: Option<u64>) -> Response<Body> {
    let frames: BoxStream<'static, Result<Frame<Bytes>, io::Error>> = if head {
        futures::stream::empty().boxed()
    } else if let Some(cut) = cut {
        let kept = reply.body.slice(
            ..usize::try_from(cut)
                .unwrap_or(usize::MAX)
                .min(reply.body.len()),
        );
        node.bytes_served
            .fetch_add(kept.len() as u64, Ordering::SeqCst);
        // The break comes after a pause, so that the headers and the bytes before it are sent.
        futures::stream::once(async move { Ok(Frame::data(kept)) })
            .chain(futures::stream::once(async {
                tokio::task::yield_now().await;
                Err(io::Error::other("the connection was cut by the test"))
            }))
            .boxed()
    } else {
        node.bytes_served
            .fetch_add(reply.body.len() as u64, Ordering::SeqCst);
        futures::stream::once(async move { Ok(Frame::data(reply.body)) }).boxed()
    };
    let mut response = Response::new(BodyExt::boxed_unsync(StreamBody::new(frames)));
    *response.status_mut() =
        http::StatusCode::from_u16(reply.status).expect("the fake's statuses are valid");
    let headers = response.headers_mut();
    headers.insert(CONTENT_LENGTH, HeaderValue::from(reply.announced));
    for (name, value) in reply.headers {
        headers.insert(
            name,
            HeaderValue::from_str(&value).expect("the fake's header values are ASCII"),
        );
    }
    response
}

/// Sends `head` (a request line and headers, without the final blank line) to `addr` as raw
/// HTTP/1.1 and returns the whole answer as text: for the requests the client does not make
/// (a `Range` the fake must not understand).
pub async fn raw(addr: &str, head: &str) -> String {
    let addr = addr.to_owned();
    let request = format!("{head}\r\nHost: fake\r\nConnection: close\r\n\r\n");
    tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect to the fake");
        stream.write_all(request.as_bytes()).expect("send");
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).expect("read");
        String::from_utf8_lossy(&answer).into_owned()
    })
    .await
    .expect("the raw client ran")
}
