//! [`FetchEngine`]: the engine, its state, the three class loops and the frozen `Engine`'s
//! `ensure` and `wait`.
//!
//! **Locking.** One mutex holds the scheduling state of every fill and the three class queues.
//! It is held only for bookkeeping in memory (and the cell CAS of a claim or an abandon, which are
//! loads and stores of the state page), never across an await, a GET, a hash or the entry's
//! blocking I/O (`open`, `publish`, `read_cell`, which run on the blocking pool). The queue is
//! engine-wide by design (deficit round robin across every job of the node, 4.5), so the fills'
//! state shares its lock rather than a sharded map.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::FutureExt;
use tierstore_fetch_api::budget::{Budget, Charge};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::engine::{Engine, PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::{ClaimedCell, Entry, EntryStore, StateView};
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, PackId, Rank};
use tierstore_fetch_api::page::CELL_SHIFT;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, Source, SourceKind, Upstream};
use tierstore_fetch_api::verify::Verifier;
use tierstore_format::ids::Generation;
use tierstore_metrics::Cache;
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::baseline::Baselines;
use crate::config::EngineConfig;
use crate::fill::{Fill, FillSched, Phase, SUB_ITEM, Want};
use crate::gate::{Gate, Level};
use crate::plan::cell_count;
use crate::pool::CellPool;
use crate::queue::{ClassQueue, Queued};
use crate::route::{DeviceBook, Routing};
use crate::stats::{GetKind, Stats, TimerKind};
use crate::timer::Timer;

/// What the engine is built from.
#[derive(Clone)]
pub struct EngineParts {
    /// The node cache's entries.
    pub store: Arc<dyn EntryStore>,
    /// Where parts come from (the storage tier's `StoreUpstream`, or a fake).
    pub upstream: Arc<dyn Upstream>,
    /// The client's flow control.
    pub budget: Arc<dyn Budget>,
    /// The hash pool.
    pub verifier: Arc<dyn Verifier>,
    /// The injected clock, and sleeping on it.
    pub timer: Arc<dyn Timer>,
    /// The node cache's families the engine counts into: `dirty_bytes` and
    /// `writeback_stall_seconds` (4.8). `None` counts only into [`Stats`].
    pub cache_metrics: Option<Arc<Cache>>,
}

impl std::fmt::Debug for EngineParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineParts").finish_non_exhaustive()
    }
}

/// The class queues' index of a class.
pub(crate) fn class_index(c: Class) -> usize {
    match c {
        Class::Demand => 0,
        Class::Prepare => 1,
        Class::Background => 2,
    }
}

const CLASSES: [Class; 3] = [Class::Demand, Class::Prepare, Class::Background];

/// The wake mask of every class.
pub(crate) const ALL_CLASSES: u8 = 0b111;

/// The engine's state under its lock.
pub(crate) struct State {
    pub queues: [ClassQueue; 3],
    pub fills: BTreeMap<u64, FillSched>,
    pub by_pack: BTreeMap<PackId, u64>,
    pub book: DeviceBook,
    pub baselines: Baselines,
    pub stats: Stats,
    pub hedges: usize,
    pub stall_retry_at: BTreeMap<DeviceRef, u64>,
    pub acquiring: [usize; 3],
    pub next_seq: u64,
    pub jitter: u64,
}

pub(crate) struct Inner {
    pub parts: EngineParts,
    pub config: EngineConfig,
    pub rt: Handle,
    state: Mutex<State>,
    pub wake: [Notify; 3],
    pub progress: Arc<watch::Sender<u64>>,
    /// The cell buffers parts are received into (4.8).
    pub pool: CellPool,
    /// The dirty-byte cap before publish (4.8), in bytes.
    pub dirty: Gate,
}

impl Inner {
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn now(&self) -> u64 {
        self.parts.timer.now_ms()
    }

    /// Tells tests and metrics something changed.
    pub(crate) fn bump(&self) {
        self.progress.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Wakes the class loops in `mask` (bit `i` for class index `i`).
    pub(crate) fn wake_classes(&self, mask: u8) {
        for (i, n) in self.wake.iter().enumerate() {
            if mask & (1 << i) != 0 {
                n.notify_one();
            }
        }
    }

    /// Changes the stats under the lock and tells the waiters of progress.
    pub(crate) fn with_stats(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.lock().stats);
        self.bump();
    }

    /// The charge of a part from `source` at `class`.
    pub(crate) fn charge(class: Class, source: &Source) -> Charge {
        let devices = match &source.kind {
            SourceKind::Degraded { sources, .. } => sources.clone(),
            _ => vec![source.device],
        };
        Charge {
            class,
            parts: source.charge(),
            devices,
            node: Some(source.device.node()),
        }
    }
}

impl State {
    /// The fill of `pack`, made (idle) if there is none; none for a pack of more than
    /// `u32::MAX` cells.
    pub(crate) fn attach(&mut self, spec: &PackSpec) -> Option<&mut FillSched> {
        let cells = cell_count(spec.size)?;
        let next = self.next_seq + 1;
        let seq = *self.by_pack.entry(spec.id.clone()).or_insert(next);
        self.next_seq = self.next_seq.max(seq);
        Some(
            self.fills
                .entry(seq)
                .or_insert_with(|| FillSched::new(Arc::new(Fill::new(seq, spec.clone(), cells)))),
        )
    }

    /// Requeues the items of fill `seq` (none once it is forgotten); the classes that gained
    /// items, as a mask.
    pub(crate) fn refresh(&mut self, seq: u64, config: &EngineConfig) -> u8 {
        let mut by_class: [Vec<Queued>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut busy = [false; 3];
        for fs in self.fills.get_mut(&seq).into_iter() {
            for (c, q) in fs.items(config) {
                by_class[class_index(c)].push(q);
            }
            for c in fs.ops.values().chain(fs.dispatching.iter().map(|(_, c)| c)) {
                busy[class_index(*c)] = true;
            }
        }
        let mut mask = 0;
        for (i, items) in by_class.iter().enumerate() {
            self.queues[i].set_fill(seq, items);
            if items.is_empty() & !busy[i] {
                self.queues[i].deactivate(seq);
            }
            mask |= u8::from(!items.is_empty()) << i;
        }
        mask
    }

    /// Forgets fill `seq` when nothing of it is in progress or wanted.
    pub(crate) fn retire(&mut self, seq: u64) {
        let idle = self
            .fills
            .get(&seq)
            .is_some_and(|fs| fs.idle() && fs.phase != Phase::Lost);
        if idle {
            for fs in self.fills.remove(&seq).into_iter() {
                self.by_pack.remove(&fs.fill.spec.id);
            }
            for q in &mut self.queues {
                q.set_fill(seq, &[]);
                q.deactivate(seq);
            }
        }
    }

    /// The next jitter in `[0.5, 1.5)` of `ms`.
    pub(crate) fn jittered(&mut self, ms: u64) -> u64 {
        // splitmix64: deterministic from the configured seed.
        self.jitter = self.jitter.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.jitter;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let half = ms / 2;
        half + z % ms.max(1)
    }
}

/// The fetch engine of a build node.
pub struct FetchEngine {
    inner: Arc<Inner>,
    loops: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for FetchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchEngine").finish_non_exhaustive()
    }
}

impl FetchEngine {
    /// An engine on the runtime of `rt`, which runs its class loops, parts and timers.
    #[must_use]
    pub fn new(parts: EngineParts, config: EngineConfig, rt: &Handle) -> Self {
        let jitter = config.jitter_seed;
        let window = config.window_packs;
        let progress = Arc::new(watch::Sender::new(0));
        let pool = CellPool::new(config.buffer_bytes, Box::new(bumps(&progress)));
        let dirty_bytes = parts.cache_metrics.clone();
        let bump = bumps(&progress);
        let dirty = Gate::new(
            config.dirty_cap_bytes,
            Box::new(move |level: &Level| {
                for m in dirty_bytes.iter() {
                    // A gauge of bytes: exact up to 2^53.
                    m.dirty_bytes.set((), level.used as f64);
                }
                bump(level);
            }),
        );
        let inner = Arc::new(Inner {
            parts,
            config,
            rt: rt.clone(),
            state: Mutex::new(State {
                // `demand` is not held by the active-pack window (a waiting reader of a fourth
                // pack would otherwise wait for another pack to finish).
                queues: [
                    ClassQueue::new(usize::MAX),
                    ClassQueue::new(window),
                    ClassQueue::new(window),
                ],
                fills: BTreeMap::new(),
                by_pack: BTreeMap::new(),
                book: DeviceBook::default(),
                baselines: Baselines::default(),
                stats: Stats::default(),
                hedges: 0,
                stall_retry_at: BTreeMap::new(),
                acquiring: [0; 3],
                next_seq: 0,
                jitter,
            }),
            wake: [Notify::new(), Notify::new(), Notify::new()],
            progress,
            pool,
            dirty,
        });
        let loops = CLASSES
            .iter()
            .map(|c| rt.spawn(class_loop(inner.clone(), *c)))
            .collect();
        Self { inner, loops }
    }

    /// The counters now.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let mut s = self.inner.lock().stats.clone();
        s.memory(&self.inner.pool.level(), &self.inner.dirty.level());
        s
    }

    /// Waits until `f` holds of the counters: the progress event a test (or a caller that waits
    /// for quiescence) waits on instead of time.
    pub async fn until(&self, f: impl Fn(&Stats) -> bool) -> Stats {
        let mut rx = self.inner.progress.subscribe();
        loop {
            rx.borrow_and_update();
            let s = self.stats();
            if f(&s) {
                return s;
            }
            // The sender lives in `inner`, which `self` holds: `changed` never fails here.
            let _ = rx.changed().await;
        }
    }

    /// Takes in what a storage node said about one of its devices outside a part (its ping
    /// every 2 s, 10.6): the queue waits and drive states the copy choice and the hedge read.
    pub fn observe_signals(&self, device: DeviceRef, signals: DriveSignals) {
        self.inner.lock().book.signals(device, signals);
        self.inner.bump();
    }
}

/// A gate's observer that wakes the waiters of progress.
fn bumps(progress: &Arc<watch::Sender<u64>>) -> impl Fn(&Level) + Send + Sync + 'static {
    let progress = progress.clone();
    move |_: &Level| progress.send_modify(|v| *v = v.wrapping_add(1))
}

/// Dropping the engine stops its class loops, so no new part starts. Parts, timers and `LOCATE`s
/// already in flight run to their end on the runtime (each holds what it needs); a node shuts the
/// runtime down to stop them.
impl Drop for FetchEngine {
    fn drop(&mut self) {
        for l in &self.loops {
            l.abort();
        }
    }
}

#[async_trait::async_trait]
impl Engine for FetchEngine {
    fn ensure(&self, pack: &PackSpec, runs: &[CellRun], class: Class, job: JobId, rank: Rank) {
        let inner = &self.inner;
        let mut st = inner.lock();
        let Some(fs) = st.attach(pack) else {
            return;
        };
        let seq = fs.fill.seq;
        let start = fs.phase != Phase::Lost && {
            let w = Want { class, job, rank };
            for run in runs {
                fs.want(run.cells(), w);
            }
            for run in runs.first().into_iter() {
                fs.touch_len(run.first, run.count);
            }
            // Prepare of a pack whose cells are `recheck` or being evicted settles them too, as
            // a waiter would.
            for e in fs.fill.entry.get().cloned().iter() {
                let s = scan(fs, e.state(), runs.iter().flat_map(|r| r.cells()));
                nudge(inner, fs, e, &s);
            }
            fs.phase == Phase::Idle
        };
        let mask = if start {
            crate::resolve::start(inner, &mut st, seq);
            0
        } else {
            st.refresh(seq, &inner.config)
        };
        st.retire(seq);
        drop(st);
        inner.wake_classes(mask);
    }

    async fn wait(&self, pack: &PackSpec, range: ByteRange, job: JobId) -> WaitOutcome {
        if range.len == 0 {
            return WaitOutcome::Ready;
        }
        let first_cell = u32::try_from(range.offset >> CELL_SHIFT).unwrap_or(u32::MAX);
        let run = match (range.cells(CELL_SHIFT), cell_count(pack.size)) {
            (Some(run), Some(cells)) if run.first < cells => CellRun {
                first: run.first,
                count: run.count.min(cells - run.first),
            },
            _ => return WaitOutcome::Eio { cell: first_cell },
        };
        let (fill, since) = {
            let mut st = self.inner.lock();
            let Some(fs) = st.attach(pack) else {
                return WaitOutcome::Eio { cell: first_cell };
            };
            fs.waiters += 1;
            (fs.fill.clone(), fs.seq)
        };
        let guard = WaiterGuard {
            inner: self.inner.clone(),
            seq: fill.seq,
        };
        let mut rx = fill.changed.subscribe();
        loop {
            rx.borrow_and_update();
            if let Some(outcome) = check(&self.inner, &fill, run, job, since) {
                drop(guard);
                return outcome;
            }
            // The sender lives in the fill, which this wait holds: `changed` never fails here.
            let _ = rx.changed().await;
        }
    }
}

/// Drops a waiter of a fill, also when the wait's future is dropped.
struct WaiterGuard {
    inner: Arc<Inner>,
    seq: u64,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        for fs in st.fills.get_mut(&self.seq).into_iter() {
            fs.waiters -= 1;
        }
        st.retire(self.seq);
    }
}

/// What one look at some cells of a fill found.
#[derive(Default)]
pub(crate) struct Scan {
    /// Absent cells: to be wanted.
    need: Vec<u32>,
    /// `recheck` cells: to be re-hashed.
    recheck: Vec<u32>,
    /// A cell the evictor owns.
    evicting: bool,
    /// A cell an op of this engine is fetching.
    claimed: bool,
    /// The first cell a wait fails on: `bad`, an unknown byte, or claimed by no op of this
    /// engine (its publish failed).
    failed: Option<u32>,
}

pub(crate) fn scan(fs: &FillSched, view: &dyn StateView, cells: impl Iterator<Item = u32>) -> Scan {
    let mut s = Scan::default();
    for c in cells {
        match view.load(c).and_then(|b| b.state()) {
            Some(CellState::Present) => {}
            Some(CellState::Absent) => s.need.push(c),
            Some(CellState::Claimed) => {
                let owned = fs.owner.contains_key(&c);
                s.claimed |= owned;
                s.failed = s.failed.or((!owned).then_some(c));
            }
            Some(CellState::Recheck) => s.recheck.push(c),
            Some(CellState::Evicting) => s.evicting = true,
            Some(CellState::Bad) | None => s.failed = s.failed.or(Some(c)),
        }
    }
    s
}

/// Starts what cells a scan found need without a fetch: the re-hash of `recheck` cells, and a
/// second look at cells the evictor owns.
pub(crate) fn nudge(inner: &Arc<Inner>, fs: &mut FillSched, entry: &Arc<dyn Entry>, s: &Scan) {
    let poll = s.evicting & !fs.evict_poll;
    fs.evict_poll |= poll;
    if poll {
        crate::settle::evict_poll(inner, &fs.fill);
    }
    for &c in &s.recheck {
        if fs.rechecking.insert(c) {
            crate::settle::recheck(inner, &fs.fill, entry, c);
        }
    }
}

/// What a waiter does after one look.
enum Look {
    /// The wait ends.
    Done(WaitOutcome),
    /// It waits for the next change, after the fill starts or requeues.
    Pending { start: bool, refresh: bool },
}

fn look(inner: &Arc<Inner>, fs: &mut FillSched, run: CellRun, job: JobId, since: u64) -> Look {
    let entry = fs.fill.entry.get().cloned();
    let s = entry.as_ref().map_or_else(
        || Scan {
            need: run.cells().collect(),
            ..Scan::default()
        },
        |e| scan(fs, e.state(), run.cells()),
    );
    let ready = entry.is_some()
        & s.need.is_empty()
        & s.recheck.is_empty()
        & !s.evicting
        & !s.claimed
        & s.failed.is_none();
    if ready {
        return Look::Done(WaitOutcome::Ready);
    }
    if let Some(cell) = s.failed {
        return Look::Done(WaitOutcome::Eio { cell });
    }
    if fs.phase == Phase::Lost {
        return Look::Done(WaitOutcome::Lost);
    }
    if let Some(cell) = fs.failed_since(since, run.first, run.first + run.count) {
        return Look::Done(WaitOutcome::Eio { cell });
    }
    let mut changed = false;
    for &c in &s.need {
        let rank = fs.wants.get(&c).map_or(Rank::default(), |w| w.rank);
        let w = Want {
            class: Class::Demand,
            job,
            rank,
        };
        changed |= fs.want(std::iter::once(c), w);
    }
    fs.touch_len(run.first, run.count);
    for e in entry.iter() {
        nudge(inner, fs, e, &s);
    }
    Look::Pending {
        start: fs.phase == Phase::Idle,
        refresh: changed | !s.need.is_empty(),
    }
}

/// One look at the cells a wait is for: its outcome, or `None` (and the cells it needs wanted
/// at `demand`) to wait for the next change.
fn check(
    inner: &Arc<Inner>,
    fill: &Arc<Fill>,
    run: CellRun,
    job: JobId,
    since: u64,
) -> Option<WaitOutcome> {
    let mut guard = inner.lock();
    let st = &mut *guard;
    // A waiter keeps its fill: the fill is not forgotten while the wait runs.
    let found = st
        .fills
        .get_mut(&fill.seq)
        .map(|fs| look(inner, fs, run, job, since));
    let (start, refresh) = match found.unwrap_or(Look::Done(WaitOutcome::Lost)) {
        Look::Done(outcome) => return Some(outcome),
        Look::Pending { start, refresh } => (start, refresh),
    };
    let mut mask = 0;
    if start {
        crate::resolve::start(inner, st, fill.seq);
    } else if refresh {
        mask = st.refresh(fill.seq, &inner.config);
    }
    drop(guard);
    inner.wake_classes(mask);
    None
}

/// What a class loop hands the task that waits for a permit.
pub(crate) struct Dispatch {
    pub fill: Arc<Fill>,
    pub item: u64,
    pub class: Class,
    pub job: JobId,
    pub rank: Rank,
    pub life: u64,
    pub source: Option<Source>,
    pub charge: Charge,
}

/// One class's loop: picks the next item by deficit round robin and hands it to a task that
/// waits for its permit, with at most `acquire_slots` such tasks at once.
async fn class_loop(inner: Arc<Inner>, class: Class) {
    let i = class_index(class);
    loop {
        let notified = inner.wake[i].notified();
        match pick(&inner, class) {
            Some(d) => {
                inner.rt.spawn(acquire_and_run(inner.clone(), d));
            }
            None => notified.await,
        }
    }
}

fn pick(inner: &Arc<Inner>, class: Class) -> Option<Dispatch> {
    let i = class_index(class);
    let quantum = inner.parts.budget.part_size(u64::MAX);
    let mut st = inner.lock();
    let st = &mut *st;
    if st.acquiring[i] >= inner.config.acquire_slots {
        return None;
    }
    let q = st.queues[i].pick(quantum)?;
    let now = inner.now();
    let fs = st.fills.get_mut(&q.what.fill)?;
    fs.dispatching.push((q.what.item, class));
    st.acquiring[i] += 1;
    st.queues[i].activate(q.what.fill);
    let excluded = fs.excluded_indexes();
    let source = fs.location.as_ref().and_then(|location| {
        let routing = Routing {
            location,
            excluded: &excluded,
            book: &st.book,
            now,
        };
        routing
            .first(q.offset)
            .map(|ix| location.sources[ix].clone())
    });
    let charge = source.as_ref().map_or(
        Charge {
            class,
            parts: 1,
            devices: Vec::new(),
            node: None,
        },
        |s| Inner::charge(class, s),
    );
    let d = Dispatch {
        fill: fs.fill.clone(),
        item: q.what.item,
        class,
        job: q.job,
        rank: q.rank,
        life: fs.life,
        source,
        charge,
    };
    // The queue was made before this item was picked: what the fill queues now may differ.
    st.refresh(d.fill.seq, &inner.config);
    Some(d)
}

/// Waits for the item's permit, claims the cells it can still claim, and runs the op.
async fn acquire_and_run(inner: Arc<Inner>, d: Dispatch) {
    let permit = inner.parts.budget.acquire(d.charge.clone()).await;
    let claimed = claim(&inner, &d);
    inner.wake_classes(1 << class_index(d.class));
    inner.bump();
    if let Some(claimed) = claimed {
        let kind = [GetKind::GridPart, GetKind::DemandSub][usize::from(d.item & SUB_ITEM != 0)];
        crate::op::run(inner, d, claimed, kind, permit).await;
    }
}

/// What a dispatched item claimed: its op, the entry, the fill's pinned generation (the
/// `If-Match` of every part the op asks for), and the claim tokens.
pub(crate) struct Claimed {
    pub op: u64,
    pub entry: Arc<dyn Entry>,
    pub generation: Generation,
    pub tokens: BTreeMap<u32, ClaimedCell>,
}

/// After the permit: the item's still-pending cells, claimed, and the op that owns them.
fn claim(inner: &Arc<Inner>, d: &Dispatch) -> Option<Claimed> {
    let mut st = inner.lock();
    let st = &mut *st;
    let i = class_index(d.class);
    st.acquiring[i] -= 1;
    st.next_seq += 1;
    let op = st.next_seq;
    let mut claimed = None;
    // A dispatched item keeps its fill: `dispatching` is not empty until here.
    for fs in st.fills.get_mut(&d.fill.seq).into_iter() {
        fs.dispatching.retain(|x| *x != (d.item, d.class));
        // A fill leaves `Ready` only into a new life: an item of an older life claims nothing.
        let cells = match fs.life == d.life {
            true => fs.claimable(d.item),
            false => Vec::new(),
        };
        // A fill with claimable cells is ready, so its entry is open and its generation pinned.
        for (entry, generation) in d.fill.entry.get().zip(fs.pinned).into_iter() {
            let mut tokens = BTreeMap::new();
            for run in runs_of(&cells) {
                for t in entry.claim(run) {
                    fs.owner.insert(t.cell(), op);
                    tokens.insert(t.cell(), t);
                }
            }
            if !tokens.is_empty() {
                fs.ops.insert(op, d.class);
                claimed = Some(Claimed {
                    op,
                    entry: entry.clone(),
                    generation,
                    tokens,
                });
            }
        }
    }
    let mask = st.refresh(d.fill.seq, &inner.config);
    st.retire(d.fill.seq);
    inner.wake_classes(mask);
    claimed
}

/// The contiguous runs of ascending `cells`.
pub(crate) fn runs_of(cells: &[u32]) -> Vec<CellRun> {
    let mut runs: Vec<CellRun> = Vec::new();
    for &c in cells {
        match runs.last_mut() {
            Some(r) if r.first + r.count == c => r.count += 1,
            _ => runs.push(CellRun { first: c, count: 1 }),
        }
    }
    runs
}

/// A sleep on the engine's timer, counted in [`Stats::armed`] from its first poll until it
/// completes or is dropped (so a test that sees it armed knows it is registered with the clock).
pub(crate) struct Armed {
    inner: Arc<Inner>,
    kind: TimerKind,
    armed: bool,
    sleep: futures::future::BoxFuture<'static, ()>,
}

impl Armed {
    pub(crate) fn new(inner: &Arc<Inner>, kind: TimerKind, deadline_ms: u64) -> Self {
        Self {
            inner: inner.clone(),
            kind,
            armed: false,
            sleep: inner.parts.timer.sleep_until(deadline_ms),
        }
    }
}

impl std::future::Future for Armed {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let poll = self.sleep.poll_unpin(cx);
        if !self.armed {
            self.armed = true;
            let kind = self.kind;
            self.inner.with_stats(|s| s.arm(kind));
        }
        poll
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if self.armed {
            let kind = self.kind;
            self.inner.with_stats(|s| s.disarm(kind));
        }
    }
}
