//! One client's fetch of one tree, three ways: the read path of the redesign ([`run_engine`]:
//! the Prepare planner's ensures through the fetch engine, the budget, the hash pool and the file
//! entry store, with parts read straight from the volume servers), the way the shipped daemon
//! fills ([`run_shipped`]: one ranged read per piece of each file through the filer), and demand
//! reads of single files on an engine nobody ensured anything on ([`run_demand`]).
//!
//! [`prepare`] comes first: it reads the tree's manifests, makes the version 2 list and its L2
//! sidecar (kept in a directory, so a tree's packs are read for their cells once), and opens it.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use tierstore::{ListKind, TreeDigest, TreeList};
use tierstore_budget::{BudgetConfig, ClientBudget, Topology};
use tierstore_entry::FileEntryStore;
use tierstore_fetch::timer::TokioTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine};
use tierstore_fetch_api::engine::{Engine as _, PackDigests, PackSpec, WaitOutcome};
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, PackId, PackKey};
use tierstore_fetch_api::list::ListL2;
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_hashpool::{DEFAULT_TARGET_RATE, HashPool};
use tierstore_list::{CellHasher, TreeListL2};
use tierstore_plan::config::PlanConfig;
use tierstore_plan::plan::{Backlog, Needed, PlanInput, plan};
use tokio::runtime::Handle;

use crate::Error;
use crate::clock::Clock;
use crate::http::HttpClient;
use crate::manifest::{Walked, encode_v2, walk};
use crate::record::{ClientRun, DemandRead, Mode, Summary};
use crate::sidecar::{build, read_or_none};
use crate::swfs::{FilerFetch, VolumeMap};
use crate::upstream::{Recorder, SwfsUpstream};

/// One MiB.
const MIB: u64 = 1 << 20;

/// The farm's store as the replays reach it: the filer, the volume map and the client both use.
#[derive(Debug, Clone)]
pub struct Store {
    /// The filer of the bucket.
    pub fetch: FilerFetch,
    /// Which node and disk holds each volume's copies.
    pub map: Arc<VolumeMap>,
    /// The client the parts go through.
    pub http: HttpClient,
}

/// The configuration of the read path for one run. Every field is printed into
/// [`ClientRun::config`].
#[derive(Debug, Clone)]
pub struct Knobs {
    /// The Prepare planner's constants.
    pub plan: PlanConfig,
    /// The fetch engine's constants.
    pub engine: EngineConfig,
    /// The budget's topology and constants.
    pub budget: BudgetConfig,
    /// The hash pool's threads; `None` measures the core and sizes the pool for 500 MB/s.
    pub hash_threads: Option<NonZeroUsize>,
    /// Manifests read at once when a tree is walked.
    pub walk_parallel: usize,
    /// The shipped fill's piece, in bytes.
    pub fill_chunk: u64,
    /// The shipped fill's reads in flight.
    pub fill_concurrency: usize,
}

impl Knobs {
    /// The design's starting values for `total_clients` clients sharing the store: twelve cold
    /// drives of one stream, no hot device, the shipped daemon's 8 MiB pieces 16 at a time and
    /// 16 manifests at a time.
    #[must_use]
    pub fn new(total_clients: u32) -> Self {
        Self {
            plan: PlanConfig::default(),
            engine: EngineConfig::default(),
            budget: BudgetConfig::new(Topology {
                cold_drives: 12,
                hot_devices: 0,
                clients: total_clients.max(1),
                drive_streams: 1,
                hot_queue_depth: 4,
            }),
            hash_threads: None,
            walk_parallel: 16,
            fill_chunk: 8 * MIB,
            fill_concurrency: 16,
        }
    }

    /// The knobs as `name = value` text, the form [`ClientRun::config`] keeps. `hash_threads` is
    /// the number of threads the pool ran (the knob may have left it to be measured).
    #[must_use]
    pub fn describe(&self, hash_threads: usize, sidecar_built: bool) -> BTreeMap<String, String> {
        let t = &self.budget.topology;
        let sizes: Vec<String> = self
            .budget
            .part_sizes
            .iter()
            .map(|s| (s / MIB).to_string())
            .collect();
        let pairs = [
            ("part_sizes_mib", sizes.join(",")),
            ("drive_streams", t.drive_streams.to_string()),
            ("cold_drives", t.cold_drives.to_string()),
            ("hot_devices", t.hot_devices.to_string()),
            ("clients", t.clients.to_string()),
            (
                "cold_positioning_us",
                self.plan.cold.positioning_us.to_string(),
            ),
            (
                "cold_media_mb_per_s",
                self.plan.cold.media_mb_per_s.to_string(),
            ),
            ("whole_cov_percent", self.plan.whole_cov_percent.to_string()),
            ("small_pack_bytes", self.plan.small_pack_bytes.to_string()),
            (
                "small_whole_cov_percent",
                self.plan.small_whole_cov_percent.to_string(),
            ),
            ("hash_threads", hash_threads.to_string()),
            ("walk_parallel", self.walk_parallel.to_string()),
            ("fill_chunk", self.fill_chunk.to_string()),
            ("fill_concurrency", self.fill_concurrency.to_string()),
            ("window_packs", self.engine.window_packs.to_string()),
            ("sidecar_built", sidecar_built.to_string()),
        ];
        pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }
}

/// Who a record is made for.
#[derive(Debug, Clone)]
pub struct Client {
    /// The name of the run.
    pub run: String,
    /// The node the client runs on.
    pub node: String,
    /// The client's index in the run, from 0.
    pub client: u32,
    /// The clients of the run, on every node.
    pub clients: u32,
    /// The job the engine's queues know the client's fetches by.
    pub job: u64,
}

/// A tree read from the store and ready to be fetched.
#[derive(Debug)]
pub struct Prepared {
    /// The tree: the hex digest of its root manifest.
    pub root: String,
    /// What the walk found.
    pub walked: Walked,
    /// Microseconds the walk took.
    pub manifest_us: u64,
    /// The version 2 list of the tree.
    pub v2: Vec<u8>,
    /// Its L2 sidecar, opened.
    pub l2: Arc<TreeListL2<Vec<u8>>>,
    /// The size of each pack the tree names, by key.
    pub sizes: BTreeMap<String, u64>,
    /// Whether the sidecar was made now (its packs read), not found in the sidecar directory.
    pub sidecar_built: bool,
}

/// An [`Error::ReadPath`] of `what` from any error that prints.
fn fail<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> Error {
    move |e| Error::ReadPath {
        what,
        why: e.to_string(),
    }
}

/// The pack keys of `list` in order of first use.
fn pack_keys(list: &TreeList) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut keys = Vec::new();
    for entry in &list.entries {
        if let ListKind::File {
            source: Some(source),
            ..
        } = &entry.kind
            && seen.insert(source.key.clone())
        {
            keys.push(source.key.to_string());
        }
    }
    keys
}

/// Reads the tree `root`, makes its version 2 list and opens its L2 sidecar from `sidecars` (named
/// by the list's digest), making it first from the cells of the tree's packs when the directory
/// has none.
///
/// # Errors
/// The walk or a pack read failed, the sidecar file is not the tree's sidecar or cannot be
/// written, or the list is refused.
pub async fn prepare(
    store: &Store,
    root: &str,
    sidecars: &Path,
    parallel: usize,
    clock: &dyn Clock,
) -> Result<Prepared, Error> {
    let began = clock.now_us();
    let walked = walk(root, &store.fetch, parallel).await?;
    let manifest_us = clock.now_us().saturating_sub(began);
    let v2 = encode_v2(&walked.list)?;
    let id = TreeDigest::of(&v2);
    std::fs::create_dir_all(sidecars).map_err(|e| Error::io("create", sidecars, e))?;
    let path = sidecars.join(format!("{id}.l2"));
    let (bytes, sidecar_built) = if let Some(bytes) = read_or_none(&path)? {
        (bytes, false)
    } else {
        let sizes = pack_sizes(store, &walked.list, parallel).await?;
        let bytes = build(&v2, &store.fetch, &sizes, parallel).await?;
        std::fs::write(&path, &bytes).map_err(|e| Error::io("write", &path, e))?;
        (bytes, true)
    };
    let l2 = TreeListL2::open_sidecar(Some(bytes), id.as_bytes()).map_err(|e| Error::ReadPath {
        what: "sidecar",
        why: format!("{}: {e}", path.display()),
    })?;
    let sizes = (0..l2.pack_count())
        .filter_map(|pack| l2.pack_record(pack))
        .map(|r| (r.key.0, r.pack_size))
        .collect();
    Ok(Prepared {
        root: root.to_owned(),
        walked,
        manifest_us,
        v2,
        l2: Arc::new(l2),
        sizes,
        sidecar_built,
    })
}

/// The size of every pack of `list`, from the filer's entries.
async fn pack_sizes(
    store: &Store,
    list: &TreeList,
    parallel: usize,
) -> Result<BTreeMap<String, u64>, Error> {
    stream::iter(pack_keys(list))
        .map(|key| async move {
            match store.fetch.entry(&key).await? {
                Some(entry) => Ok((key, entry.size)),
                None => Err(Error::parse(key, "the filer has no such pack")),
            }
        })
        .buffer_unordered(parallel.max(1))
        .try_collect()
        .await
}

/// What the engine needs to fill pack `pack` of the tree.
///
/// # Errors
/// The list has no such pack.
pub fn pack_spec(p: &Prepared, pack: u32) -> Result<PackSpec, Error> {
    let l2 = &p.l2;
    let (Some(record), Some(root)) = (l2.pack_record(pack), l2.pack_root(pack)) else {
        return Err(Error::ReadPath {
            what: "fill",
            why: format!("the list has no pack {pack}"),
        });
    };
    Ok(PackSpec {
        id: PackId {
            key: record.key,
            root,
        },
        size: record.pack_size,
        digests: Some(PackDigests {
            list: l2.clone(),
            pack,
        }),
    })
}

/// Whether a wait ended well.
///
/// # Errors
/// The wait ended in `eio` or `lost`; the error names the pack.
pub fn check_outcome(pack: &PackKey, outcome: WaitOutcome) -> Result<(), Error> {
    let why = match outcome {
        WaitOutcome::Ready => return Ok(()),
        WaitOutcome::Eio { cell } => format!("{}: cell {cell} failed (eio)", pack.0),
        WaitOutcome::Lost => format!("{}: the fill is lost", pack.0),
    };
    Err(Error::ReadPath { what: "fill", why })
}

/// The bytes of `run` in a pack of `size` bytes (the last cell is clipped).
fn run_bytes(run: CellRun, size: u64) -> ByteRange {
    let offset = u64::from(run.first) << CELL_SHIFT;
    let len = (u64::from(run.count) << CELL_SHIFT).min(size.saturating_sub(offset));
    ByteRange { offset, len }
}

/// The engine of one client with everything it is made of.
struct Rig {
    engine: FetchEngine,
    recorder: Arc<Recorder>,
    hash_threads: usize,
}

impl Rig {
    fn start(
        store: &Store,
        knobs: &Knobs,
        cache: &Path,
        clock: Arc<dyn Clock>,
        rt: &Handle,
    ) -> Result<Self, Error> {
        let entries = FileEntryStore::new(cache).map_err(fail("entry store"))?;
        let recorder = Recorder::new();
        let upstream = SwfsUpstream::new(
            store.fetch.clone(),
            store.map.clone(),
            store.http.clone(),
            recorder.clone(),
            clock,
        );
        let timer = Arc::new(TokioTimer::new());
        let budget =
            ClientBudget::new(knobs.budget.clone(), timer.clone()).map_err(fail("budget"))?;
        let pool = match knobs.hash_threads {
            Some(threads) => HashPool::start(threads, timer.clone()),
            None => HashPool::start_measured(DEFAULT_TARGET_RATE, timer.clone()),
        }
        .map_err(fail("hash pool"))?;
        let hash_threads = pool.threads();
        let parts = EngineParts {
            store: Arc::new(entries),
            upstream: Arc::new(upstream),
            budget: Arc::new(budget),
            verifier: Arc::new(pool),
            timer,
        };
        Ok(Self {
            engine: FetchEngine::new(parts, knobs.engine.clone(), rt),
            recorder,
            hash_threads,
        })
    }
}

/// The record of a client from what its parts did.
struct Totals<'a> {
    client: &'a Client,
    mode: Mode,
    p: &'a Prepared,
    gets: BTreeMap<String, u64>,
    bytes_fetched: u64,
    tree_ready_us: u64,
    first_byte_us: Vec<u64>,
    get_us: Vec<u64>,
    get_errors: u64,
    config: BTreeMap<String, String>,
}

impl Totals<'_> {
    fn into_run(self) -> ClientRun {
        ClientRun {
            run: self.client.run.clone(),
            mode: self.mode,
            tree: self.p.root.clone(),
            client: self.client.client,
            clients: self.client.clients,
            node: self.client.node.clone(),
            manifests: self.p.walked.manifests,
            files: self.p.walked.files,
            packs: self.p.walked.packs,
            bytes_needed: self.p.walked.bytes,
            bytes_fetched: self.bytes_fetched,
            gets: self.gets,
            tree_ready_us: self.tree_ready_us,
            first_byte_us: Summary::of(&self.first_byte_us),
            get_us: Summary::of(&self.get_us),
            get_errors: self.get_errors,
            config: self.config,
        }
    }
}

/// Fetches the whole tree the way the redesigned read path does: plans every pack Prepare would
/// ensure (every pack on a cold drive, no backlog), ensures them in plan order on an engine over
/// a fresh entry store in `cache`, and waits for every ensured cell.
///
/// # Errors
/// The plan, the entry store, the budget or the hash pool refused, or a wait did not end
/// `ready` (the error names the pack).
pub async fn run_engine(
    store: &Store,
    p: &Prepared,
    knobs: &Knobs,
    client: &Client,
    cache: &Path,
    clock: Arc<dyn Clock>,
    rt: &Handle,
) -> Result<ClientRun, Error> {
    let needed: Vec<Needed> =
        p.l2.ranges()
            .map(|r| Needed {
                pack: r.pack,
                range: ByteRange {
                    offset: r.offset,
                    len: r.len,
                },
            })
            .collect();
    let list: &dyn ListL2 = p.l2.as_ref();
    let media = BTreeMap::new();
    let input = PlanInput {
        list,
        needed: &needed,
        profile: &[],
        media: &media,
        backlog: Backlog::default(),
    };
    let planned = plan(&input, &knobs.plan).map_err(fail("plan"))?;
    let mut specs = BTreeMap::new();
    for pack in planned
        .ensures
        .iter()
        .map(|e| e.pack)
        .collect::<BTreeSet<_>>()
    {
        specs.insert(pack, pack_spec(p, pack)?);
    }
    let rig = Rig::start(store, knobs, cache, clock.clone(), rt)?;
    let engine = &rig.engine;
    let job = JobId(client.job);
    let began = clock.now_us();
    for e in &planned.ensures {
        engine.ensure(&specs[&e.pack], &e.runs, Class::Prepare, job, e.rank);
    }
    let waits = planned
        .ensures
        .iter()
        .flat_map(|e| e.runs.iter().map(move |run| (e.pack, *run)))
        .map(|(pack, run)| {
            let spec = &specs[&pack];
            async move {
                let outcome = engine.wait(spec, run_bytes(run, spec.size), job).await;
                check_outcome(&spec.id.key, outcome)
            }
        });
    let answers = futures::future::join_all(waits).await;
    let tree_ready_us = clock.now_us().saturating_sub(began);
    answers.into_iter().collect::<Result<(), Error>>()?;
    let stats = engine.stats();
    let gets = [
        ("grid_part", tierstore_fetch::GetKind::GridPart),
        ("demand_sub", tierstore_fetch::GetKind::DemandSub),
        ("retry", tierstore_fetch::GetKind::Retry),
        ("hedge", tierstore_fetch::GetKind::Hedge),
    ]
    .into_iter()
    .map(|(name, kind)| (name.to_owned(), stats.gets(kind)))
    .collect();
    let parts = rig.recorder.parts();
    Ok(Totals {
        client,
        mode: Mode::Engine,
        p,
        gets,
        bytes_fetched: rig.recorder.bytes(),
        tree_ready_us,
        first_byte_us: parts.iter().map(|t| t.first_byte_us).collect(),
        get_us: parts.iter().map(|t| t.total_us).collect(),
        get_errors: parts.iter().filter(|t| !t.ok).count() as u64,
        config: knobs.describe(rig.hash_threads, p.sidecar_built),
    }
    .into_run())
}

/// A file's bytes in its pack.
struct Span {
    key: String,
    offset: u64,
    size: u64,
}

/// The files of the tree with bytes, by path (the walk gives a source to a file only when it has
/// bytes).
fn spans(list: &TreeList) -> Vec<Span> {
    let mut files: Vec<(&str, Span)> = list
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            ListKind::File {
                size,
                source: Some(source),
                ..
            } => Some((
                entry.path.as_str(),
                Span {
                    key: source.key.to_string(),
                    offset: source.offset,
                    size: *size,
                },
            )),
            _ => None,
        })
        .collect();
    files.sort_by_key(|(path, _)| *path);
    files.into_iter().map(|(_, span)| span).collect()
}

/// One piece read through the filer: its bytes and the microseconds to its first byte and to its
/// end.
async fn read_piece(
    fetch: &FilerFetch,
    clock: &dyn Clock,
    key: &str,
    offset: u64,
    len: u64,
) -> Result<(Vec<u8>, u64, u64), Error> {
    let began = clock.now_us();
    let mut body = fetch.read_range(key, offset, len).await?.body;
    let mut bytes = Vec::new();
    let mut first = None;
    while let Some(piece) = body.try_next().await? {
        first.get_or_insert_with(|| clock.now_us().saturating_sub(began));
        bytes.extend_from_slice(&piece);
    }
    if bytes.len() as u64 != len {
        return Err(Error::parse(
            key,
            format!("read {} bytes of the {len} at {offset}", bytes.len()),
        ));
    }
    let total = clock.now_us().saturating_sub(began);
    Ok((bytes, first.unwrap_or(total), total))
}

/// Fetches the whole tree the way the shipped daemon does: each file's bytes in `fill_chunk`
/// pieces, one ranged read through the filer for each, `fill_concurrency` in flight; each file's
/// bytes are hashed as they come (as the stand-in for the shipped digest check) and dropped.
///
/// # Errors
/// A read failed or was cut short (the error names the pack).
pub async fn run_shipped(
    store: &Store,
    p: &Prepared,
    knobs: &Knobs,
    client: &Client,
    clock: Arc<dyn Clock>,
) -> Result<ClientRun, Error> {
    let chunk = knobs.fill_chunk.max(1);
    let files = spans(&p.walked.list);
    let mut pieces = Vec::new();
    for (file, span) in files.iter().enumerate() {
        for i in 0..span.size.div_ceil(chunk) {
            let start = i * chunk;
            pieces.push((
                file,
                span.key.clone(),
                span.offset + start,
                chunk.min(span.size - start),
            ));
        }
    }
    let fetch = &store.fetch;
    let clock_ref = clock.as_ref();
    let began = clock.now_us();
    let mut done = stream::iter(pieces)
        .map(|(file, key, offset, len)| async move {
            read_piece(fetch, clock_ref, &key, offset, len)
                .await
                .map(|read| (file, read))
        })
        .buffered(knobs.fill_concurrency.max(1));
    let (mut first_byte_us, mut get_us) = (Vec::new(), Vec::new());
    let (mut fetched, mut hashed) = (0u64, 0u64);
    let (mut current, mut hasher) = (None, CellHasher::new());
    while let Some((file, (bytes, first, total))) = done.try_next().await? {
        if current != Some(file) {
            hashed += hasher.size();
            (current, hasher) = (Some(file), CellHasher::new());
        }
        hasher.update(&bytes);
        fetched += bytes.len() as u64;
        first_byte_us.push(first);
        get_us.push(total);
    }
    hashed += hasher.size();
    let tree_ready_us = clock.now_us().saturating_sub(began);
    let mut config = knobs.describe(0, p.sidecar_built);
    config.insert("hashed_bytes".to_owned(), hashed.to_string());
    Ok(Totals {
        client,
        mode: Mode::Shipped,
        p,
        gets: BTreeMap::from([("chunk".to_owned(), get_us.len() as u64)]),
        bytes_fetched: fetched,
        tree_ready_us,
        first_byte_us,
        get_us,
        get_errors: 0,
        config,
    }
    .into_run())
}

/// The label of a wait's outcome in a [`DemandRead`]: `ready`, `eio` or `lost`.
#[must_use]
pub fn outcome_label(outcome: WaitOutcome) -> &'static str {
    match outcome {
        WaitOutcome::Ready => "ready",
        WaitOutcome::Eio { .. } => "eio",
        WaitOutcome::Lost => "lost",
    }
}

/// The files demand reads probe: the first file of each pack (by path), spread by taking every
/// step-th of them so that at most `probes` are left.
fn probe_spans(list: &TreeList, probes: usize) -> Vec<Span> {
    let mut seen = BTreeSet::new();
    let firsts: Vec<Span> = spans(list)
        .into_iter()
        .filter(|s| seen.insert(s.key.clone()))
        .collect();
    let step = (firsts.len() / probes.max(1)).max(1);
    firsts.into_iter().step_by(step).take(probes).collect()
}

/// Reads `probes` files of the tree on their own, as a job would before Prepare fetched them: each
/// is one `wait` on the file's bytes of its pack, with nothing ensured, one after the other on one
/// engine over a fresh entry store in `cache`. `load_clients` is what the record says was fetching
/// at the same time.
///
/// # Errors
/// The entry store, the budget or the hash pool refused, or the list has no pack of a file.
#[expect(
    clippy::too_many_arguments,
    reason = "the signature the harness's command line and load driver agree on"
)]
pub async fn run_demand(
    store: &Store,
    p: &Prepared,
    knobs: &Knobs,
    client: &Client,
    cache: &Path,
    probes: usize,
    load_clients: u32,
    clock: Arc<dyn Clock>,
    rt: &Handle,
) -> Result<Vec<DemandRead>, Error> {
    let by_key: BTreeMap<&str, u32> = (0..p.l2.pack_count())
        .filter_map(|pack| p.l2.pack_key(pack).map(|key| (key, pack)))
        .collect();
    let rig = Rig::start(store, knobs, cache, clock.clone(), rt)?;
    let mut reads = Vec::new();
    for span in probe_spans(&p.walked.list, probes) {
        let pack = by_key.get(span.key.as_str()).copied().unwrap_or(u32::MAX);
        let spec = pack_spec(p, pack)?;
        let range = ByteRange {
            offset: span.offset,
            len: span.size,
        };
        let began = clock.now_us();
        let outcome = rig.engine.wait(&spec, range, JobId(client.job)).await;
        reads.push(DemandRead {
            run: client.run.clone(),
            node: client.node.clone(),
            load_clients,
            len: span.size,
            wait_us: clock.now_us().saturating_sub(began),
            outcome: outcome_label(outcome).to_owned(),
        });
    }
    Ok(reads)
}
