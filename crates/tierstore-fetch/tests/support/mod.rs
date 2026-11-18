//! The rig the engine's tests run on: an in-memory entry store (the fake of tierstore-fetch-api),
//! an upstream scripted per device, a FIFO budget with a given number of permits, a sha256
//! verifier, an L2 list of the test packs' cell digests, and a manual clock. Nothing here reads
//! the machine's clock; a test moves the manual clock only after the engine says (by its
//! counters) that it is waiting on it.
#![allow(
    dead_code,
    reason = "each test binary uses the part of the rig it needs"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine, Stats};
use tierstore_fetch_api::budget::{Budget, Charge, Observation, Permit};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::engine::{PackDigests, PackSpec};
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::ids::{ByteRange, PackId, PackKey, PackRoot};
use tierstore_fetch_api::list::{CellDigest, ListL2, NodeIndex, PackRecord};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, GetRequest, Located, Location, PartResponse, Report, Source,
    SourceKind, Upstream, UpstreamError,
};
use tierstore_fetch_api::verify::{CellJob, Verdict, Verifier};
use tierstore_format::ids::{BlockId, DriveId, Generation, NodeId};
use tokio::sync::{Notify, Semaphore};

pub const MIB: u64 = 1 << 20;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The bytes of a test pack: byte `i` is a function of `i` and `seed`, so every cell differs.
#[must_use]
pub fn pattern(size: u64, seed: u8) -> Bytes {
    (0..size)
        .map(|i| (i as u8).wrapping_mul(31) ^ ((i >> 20) as u8) ^ seed)
        .collect::<Vec<u8>>()
        .into()
}

#[must_use]
pub fn sha256(bytes: &[u8]) -> CellDigest {
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0; 32];
    out.copy_from_slice(d.as_ref());
    out
}

#[must_use]
pub fn generation(b: u8) -> Generation {
    Generation([b; 16])
}

/// An L2 list holding one pack's cell digests.
#[derive(Debug)]
pub struct TestList {
    pub key: PackKey,
    pub size: u64,
    pub digests: Vec<CellDigest>,
}

impl ListL2 for TestList {
    fn pack_count(&self) -> u32 {
        1
    }

    fn pack_record(&self, pack: u32) -> Option<PackRecord> {
        (pack == 0).then(|| PackRecord {
            key: self.key.clone(),
            pack_size: self.size,
            cell_first: 0,
            cell_count: self.digests.len() as u32,
            cell_shift: 20,
            pack_flags: 1,
        })
    }

    fn cell_digest(&self, pack: u32, cell: u32) -> Option<CellDigest> {
        (pack == 0)
            .then(|| self.digests.get(cell as usize).copied())
            .flatten()
    }

    fn pack_root(&self, _pack: u32) -> Option<PackRoot> {
        Some(PackRoot::Digest([7; 32]))
    }

    fn dir_lookup(&self, _dir: NodeIndex, _name: &[u8]) -> Option<NodeIndex> {
        None
    }
}

/// A pack of the tests: its spec (with cell digests) and its bytes.
#[derive(Debug, Clone)]
pub struct Pack {
    pub spec: PackSpec,
    pub bytes: Bytes,
}

impl Pack {
    /// A pack of `size` bytes under `key`, with cell digests of its bytes.
    #[must_use]
    pub fn new(key: &str, size: u64, seed: u8) -> Self {
        let bytes = pattern(size, seed);
        let digests = bytes.chunks(MIB as usize).map(sha256).collect();
        let key = PackKey(key.to_owned());
        let list = Arc::new(TestList {
            key: key.clone(),
            size,
            digests,
        });
        Self {
            spec: PackSpec {
                id: PackId {
                    key,
                    root: PackRoot::Digest([seed; 32]),
                },
                size,
                digests: Some(PackDigests { list, pack: 0 }),
            },
            bytes,
        }
    }

    /// A `digest=none` object (section 9): its id is `(key, ETag)`.
    #[must_use]
    pub fn etag(key: &str, size: u64, seed: u8, g: Generation) -> Self {
        let bytes = pattern(size, seed);
        let key = PackKey(key.to_owned());
        Self {
            spec: PackSpec {
                id: PackId {
                    key,
                    root: PackRoot::Etag(g),
                },
                size,
                digests: None,
            },
            bytes,
        }
    }

    #[must_use]
    pub fn key(&self) -> PackKey {
        self.spec.id.key.clone()
    }

    #[must_use]
    pub fn range(&self, offset: u64, len: u64) -> ByteRange {
        ByteRange { offset, len }
    }

    #[must_use]
    pub fn all(&self) -> ByteRange {
        ByteRange {
            offset: 0,
            len: self.spec.size,
        }
    }
}

pub fn hot(node: u8, len: u64) -> Source {
    Source {
        device: DeviceRef::Hot(NodeId(node)),
        kind: SourceKind::Hot {
            file: format!("seg-{node}"),
            offset: 0,
        },
        extent: ByteRange { offset: 0, len },
    }
}

pub fn hot_clean(node: u8, len: u64) -> Source {
    Source {
        device: DeviceRef::Hot(NodeId(node)),
        kind: SourceKind::HotClean {
            file: format!("cache-{node}"),
            offset: 0,
        },
        extent: ByteRange { offset: 0, len },
    }
}

pub fn drive(node: u8, d: u8) -> DeviceRef {
    DeviceRef::Drive(NodeId(node), DriveId([d; 16]))
}

pub fn cold(node: u8, d: u8, offset: u64, len: u64) -> Source {
    Source {
        device: drive(node, d),
        kind: SourceKind::Cold {
            block: BlockId::from_raw(u64::from(d)),
            shard: format!("shard-{d}"),
            offset: 0,
        },
        extent: ByteRange { offset, len },
    }
}

/// A degraded read by the reconstructor on `node` of the `k` drives `sources`.
pub fn degraded(node: u8, sources: &[DeviceRef], offset: u64, len: u64) -> Source {
    Source {
        device: DeviceRef::Hot(NodeId(node)),
        kind: SourceKind::Degraded {
            block: BlockId::from_raw(99),
            offset: 0,
            sources: sources.to_vec(),
        },
        extent: ByteRange { offset, len },
    }
}

pub fn location(g: Generation, size: u64, sources: Vec<Source>) -> Location {
    Location {
        generation: g,
        size: Some(size),
        sources,
        conflicts: vec![],
    }
}

/// What a device does with the next part sent to it.
#[derive(Debug, Clone)]
pub enum Script {
    /// The bytes, in chunks of at most `chunk` bytes (0: one chunk), with these signals.
    Serve { chunk: usize, signals: DriveSignals },
    /// The headers never come (the request is still queued at the drive).
    NoAnswer,
    /// The headers come with these signals, and the body never yields a byte.
    Hang(DriveSignals),
    /// The first `after` bytes, then the body never yields again (a stall after the first byte).
    StallAfter(usize),
    /// The request fails.
    Fail(UpstreamError),
    /// The first `after` bytes, then a transport error.
    Truncate(usize),
    /// The byte at this object offset flipped.
    Corrupt(u64),
    /// The bytes, once the test releases `gate` (one permit per part).
    Gated(Arc<Semaphore>),
}

struct Object {
    location: Location,
    /// The bytes of each generation the copies hold.
    bytes: BTreeMap<Generation, Bytes>,
    /// The generation each device holds (default: the location's).
    held: BTreeMap<DeviceRef, Generation>,
}

#[derive(Default)]
struct UpState {
    objects: BTreeMap<PackKey, Object>,
    located: BTreeMap<PackKey, VecDeque<Result<Located, UpstreamError>>>,
    scripts: BTreeMap<DeviceRef, VecDeque<Script>>,
    gets: Vec<GetRequest>,
    locates: Vec<PackKey>,
    reports: Vec<(PackKey, Source, ByteRange, Report)>,
}

/// An upstream whose devices are scripted one part at a time; a device with no script serves.
#[derive(Default)]
pub struct TestUpstream {
    state: Mutex<UpState>,
    pub changed: Notify,
}

impl TestUpstream {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `pack` with `location`; every copy holds the pack id's ETag, or the location's
    /// generation.
    pub fn add(&self, pack: &Pack, location: Location) {
        let g = match pack.spec.id.root {
            PackRoot::Etag(g) => g,
            PackRoot::Digest(_) => location.generation,
        };
        let mut bytes = BTreeMap::new();
        bytes.insert(g, pack.bytes.clone());
        lock(&self.state).objects.insert(
            pack.key(),
            Object {
                location,
                bytes,
                held: BTreeMap::new(),
            },
        );
    }

    /// `device` holds generation `g` of the object, whose bytes are `bytes`.
    pub fn hold(&self, key: &PackKey, device: DeviceRef, g: Generation, bytes: Bytes) {
        let mut s = lock(&self.state);
        let o = s.objects.get_mut(key).expect("object added");
        o.bytes.insert(g, bytes);
        o.held.insert(device, g);
    }

    /// Removes the object: every later part answers 404 and `locate` a definitive miss.
    pub fn remove(&self, key: &PackKey) {
        lock(&self.state).objects.remove(key);
    }

    pub fn script(&self, device: DeviceRef, script: Script) {
        lock(&self.state)
            .scripts
            .entry(device)
            .or_default()
            .push_back(script);
    }

    pub fn script_locate(&self, key: &PackKey, answer: Result<Located, UpstreamError>) {
        lock(&self.state)
            .located
            .entry(key.clone())
            .or_default()
            .push_back(answer);
    }

    #[must_use]
    pub fn gets(&self) -> Vec<GetRequest> {
        lock(&self.state).gets.clone()
    }

    #[must_use]
    pub fn locates(&self) -> Vec<PackKey> {
        lock(&self.state).locates.clone()
    }

    #[must_use]
    pub fn reports(&self) -> Vec<(PackKey, Source, ByteRange, Report)> {
        lock(&self.state).reports.clone()
    }
}

fn transport(req: &GetRequest, reason: &str) -> UpstreamError {
    UpstreamError::Transport {
        key: req.key.clone(),
        device: req.source.device,
        reason: reason.to_owned(),
    }
}

#[async_trait::async_trait]
impl Upstream for TestUpstream {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        let answer = {
            let mut s = lock(&self.state);
            s.locates.push(key.clone());
            match s.located.get_mut(key).and_then(VecDeque::pop_front) {
                Some(a) => a,
                None => Ok(match s.objects.get(key) {
                    Some(o) => Located::Found(o.location.clone()),
                    None => Located::DefinitiveMiss,
                }),
            }
        };
        self.changed.notify_waiters();
        answer
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let (script, served) = {
            let mut s = lock(&self.state);
            s.gets.push(req.clone());
            let script = s
                .scripts
                .get_mut(&req.source.device)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Script::Serve {
                    chunk: 0,
                    signals: DriveSignals::default(),
                });
            let served = s.objects.get(&req.key).map(|o| {
                let g = o
                    .held
                    .get(&req.source.device)
                    .copied()
                    .unwrap_or(o.location.generation);
                (g, o.bytes.get(&g).cloned().expect("bytes of g"))
            });
            (script, served)
        };
        self.changed.notify_waiters();
        let (g, all) = served.ok_or_else(|| UpstreamError::NotFound {
            key: req.key.clone(),
            device: req.source.device,
        })?;
        let bytes = || {
            if req.generation != g {
                return Err(UpstreamError::PreconditionFailed {
                    key: req.key.clone(),
                    device: req.source.device,
                    actual: Some(g),
                });
            }
            let end = req.range.end().expect("range end") as usize;
            all.get(req.range.offset as usize..end)
                .map(|_| all.slice(req.range.offset as usize..end))
                .ok_or_else(|| transport(&req, "range outside the object"))
        };
        let (signals, body) = match script {
            Script::Fail(e) => return Err(e),
            Script::NoAnswer => {
                match futures::future::pending::<std::convert::Infallible>().await {}
            }
            Script::Serve { chunk, signals } => {
                let bytes = bytes()?;
                let size = chunk.max(bytes.len().min(1)).max(1);
                let size = [size, bytes.len().max(1)][usize::from(chunk == 0)];
                let chunks: Vec<Result<Bytes, UpstreamError>> = (0..bytes.len())
                    .step_by(size)
                    .map(|i| Ok(bytes.slice(i..(i + size).min(bytes.len()))))
                    .collect();
                (signals, stream::iter(chunks).boxed())
            }
            Script::Hang(signals) => {
                bytes()?;
                (signals, stream::pending().boxed())
            }
            Script::StallAfter(after) => {
                let bytes = bytes()?;
                let first = stream::iter(vec![Ok(bytes.slice(..after.min(bytes.len())))]);
                (
                    DriveSignals::default(),
                    first.chain(stream::pending()).boxed(),
                )
            }
            Script::Truncate(after) => {
                let bytes = bytes()?;
                let items = vec![
                    Ok(bytes.slice(..after.min(bytes.len()))),
                    Err(transport(&req, "stream ended early")),
                ];
                (DriveSignals::default(), stream::iter(items).boxed())
            }
            Script::Corrupt(offset) => {
                let mut v = bytes()?.to_vec();
                v[(offset - req.range.offset) as usize] ^= 0xff;
                (
                    DriveSignals::default(),
                    stream::iter(vec![Ok(Bytes::from(v))]).boxed(),
                )
            }
            Script::Gated(gate) => {
                let bytes = bytes()?;
                let body = stream::once(async move {
                    gate.acquire().await.expect("gate open").forget();
                    Ok(bytes)
                });
                (DriveSignals::default(), body.boxed())
            }
        };
        Ok(PartResponse {
            generation: g,
            signals,
            body,
        })
    }

    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report) {
        lock(&self.state)
            .reports
            .push((key.clone(), source.clone(), range, report));
        self.changed.notify_waiters();
    }
}

/// A budget of `permits` parts in flight, granted first come first served, that logs every
/// charge and observation; `P` follows the starting values of 4.4 unless set.
pub struct TestBudget {
    permits: Arc<Semaphore>,
    part_size: Mutex<Option<u64>>,
    pub charges: Mutex<Vec<Charge>>,
    pub observations: Mutex<Vec<Observation>>,
}

impl TestBudget {
    #[must_use]
    pub fn new(permits: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            part_size: Mutex::new(None),
            charges: Mutex::new(Vec::new()),
            observations: Mutex::new(Vec::new()),
        }
    }

    pub fn set_part_size(&self, p: u64) {
        *lock(&self.part_size) = Some(p);
    }

    pub fn add_permits(&self, n: usize) {
        self.permits.add_permits(n);
    }

    #[must_use]
    pub fn charges(&self) -> Vec<Charge> {
        lock(&self.charges).clone()
    }

    #[must_use]
    pub fn observations(&self) -> Vec<Observation> {
        lock(&self.observations).clone()
    }
}

#[async_trait::async_trait]
impl Budget for TestBudget {
    async fn acquire(&self, charge: Charge) -> Permit {
        let n = charge.parts.max(1);
        lock(&self.charges).push(charge);
        let p = self
            .permits
            .clone()
            .acquire_many_owned(n)
            .await
            .expect("budget open");
        Permit::new(move || drop(p))
    }

    fn observe(&self, observation: Observation) {
        lock(&self.observations).push(observation);
    }

    fn part_size(&self, pack_size: u64) -> u64 {
        lock(&self.part_size).unwrap_or(if pack_size > 512 * MIB {
            64 * MIB
        } else {
            32 * MIB
        })
    }
}

/// sha256 against the list's digest.
#[derive(Default)]
pub struct Sha256Verifier;

#[async_trait::async_trait]
impl Verifier for Sha256Verifier {
    async fn verify(&self, job: CellJob) -> (Verdict, Bytes) {
        let v = if sha256(&job.bytes) == job.expected {
            Verdict::Match
        } else {
            Verdict::Mismatch
        };
        (v, job.bytes)
    }
}

/// An engine on fakes.
pub struct Rig {
    pub clock: ManualClock,
    pub store: Arc<MemEntryStore>,
    pub upstream: Arc<TestUpstream>,
    pub budget: Arc<TestBudget>,
    pub engine: FetchEngine,
}

impl Rig {
    /// A rig with `permits` budget permits and `config`.
    #[must_use]
    pub fn new(permits: usize, config: EngineConfig) -> Self {
        let clock = ManualClock::new();
        let store = Arc::new(MemEntryStore::new());
        let upstream = Arc::new(TestUpstream::new());
        let budget = Arc::new(TestBudget::new(permits));
        let parts = EngineParts {
            store: store.clone(),
            upstream: upstream.clone(),
            budget: budget.clone(),
            verifier: Arc::new(Sha256Verifier),
            timer: Arc::new(ManualTimer::new(clock.clone())),
        };
        let engine = FetchEngine::new(parts, config, &tokio::runtime::Handle::current());
        Self {
            clock,
            store,
            upstream,
            budget,
            engine,
        }
    }

    #[must_use]
    pub fn default_rig() -> Self {
        Self::new(64, EngineConfig::default())
    }

    /// Waits for the engine's counters to satisfy `f`.
    pub async fn until(&self, f: impl Fn(&Stats) -> bool) -> Stats {
        self.engine.until(f).await
    }

    /// The bytes of `pack` the entry holds in `range`.
    #[must_use]
    pub fn entry_bytes(&self, pack: &Pack, offset: u64, len: u64) -> Vec<u8> {
        self.store
            .mem_entry(&pack.spec.id)
            .expect("entry")
            .bytes(offset as usize, (offset + len) as usize)
    }

    /// The state of every cell of `pack`'s entry.
    #[must_use]
    pub fn cells(&self, pack: &Pack) -> Vec<CellState> {
        let e = self.store.get(&pack.spec.id).expect("entry");
        let n = e.state().cell_count();
        (0..n)
            .map(|c| e.state().load(c).expect("cell").state().expect("state"))
            .collect()
    }

    /// GETs of `pack`, as `(offset, len)` in MiB-free bytes.
    #[must_use]
    pub fn get_ranges(&self, pack: &Pack) -> Vec<(u64, u64)> {
        self.upstream
            .gets()
            .iter()
            .filter(|g| g.key == pack.key())
            .map(|g| (g.range.offset, g.range.len))
            .collect()
    }
}
