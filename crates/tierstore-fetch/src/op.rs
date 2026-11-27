//! One op: the claimed cells of one dispatched item and the attempts that fetch them (4.2, 4.6).
//!
//! The op owns the claim tokens of its cells (the "one slot" of a claim that a hedge and its
//! original share): whichever attempt first delivers a verifying cell publishes it, and the
//! other's bytes are dropped, never written. Attempts run as tasks and report to the op on a
//! bounded channel, so a cell waiting on the dirty-byte cap slows the receive (4.8's
//! back-pressure).
//!
//! - **Dirty-byte cap (4.8):** a verified cell waits before its publish while the cells being
//!   published hold the cap; cells of `demand` ops go first, so a demand cell is never queued
//!   behind prepare cells. Its publish then runs on the blocking pool while the op takes the next
//!   cell, and gives its share of the cap back when it returns, published or failed. Only a cell
//!   whose claim token the op still holds reaches the cap: a hedge's loser never does. The op
//!   ends only after its publishes have returned.
//!
//! - **Stall:** before the first byte, a GET that has not answered for the pre-first-byte timeout
//!   (restarted when the headers said `x-ts-queue-wait-ms`: a queued drive is not a stall); after
//!   it, no bytes for the stall timeout; or the drive said `stalled`. The attempt ends and the op
//!   retries on the alternative (the other hot replica, or the degraded read when the stalled
//!   device is the data shard's drive, else the same source on a fresh connection), at most one
//!   stall retry per drive per window.
//! - **Retries:** up to `max_attempts`; a `503` waits its `Retry-After` (and a jittered backoff),
//!   a transport error or a `stalled`/`indexing` answer retries at once. A copy of another
//!   generation (412, or another `ETag`) or one that answered 404 is excluded from the fill and the
//!   part goes to another copy. A 404 from a durable copy asks `LOCATE` again (the lost path); so
//!   do exhausted attempts, while the location keeps being found, for up to `unavailable_wait`,
//!   then the cells fail with EIO. The first retry after an op's `LOCATE` goes at once; after a
//!   second one every retry backs off, so a copy that keeps refusing is not asked in a tight loop.
//! - **Hedge:** only for `demand`, when no first byte came within the hedge trigger and the rules
//!   of [`crate::route`] allow it, with `hedge_permits` (one) hedge in flight per engine, which is
//!   one per build node.
//! - **Verification failure:** the cell is refetched once, at `demand`, from the alternative
//!   source; a second failure marks it `bad` for `bad_cell_ms`. The serving node is told
//!   (`x-ts-report: bitrot`) each time.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::future::OptionFuture;
use futures::stream;
use tierstore_fetch_api::budget::Permit;
use tierstore_fetch_api::entry::{ClaimedCell, Entry};
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, Rank};
use tierstore_fetch_api::upstream::{Report, Source, SourceKind};
use tierstore_fetch_api::verify::Verdict;
use tierstore_format::ids::Generation;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::attempt::{Attempt, Event, Fail, FailKind};
use crate::baseline::timeout;
use crate::engine::{ALL_CLASSES, Armed, Claimed, Dispatch, Inner, runs_of};
use crate::fill::{Fill, Phase, Resolved};
use crate::plan::{CELL_BYTES, size_class};
use crate::pool::Slot;
use crate::route::Routing;
use crate::settle::bad_expiry;
use crate::stats::{GetKind, TimerKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// Every cell published (or marked bad).
    Done,
    /// The remaining cells fail with EIO for their waiters.
    Eio,
    /// The fill failed or was lost while the op ran: nothing more to do.
    Stale,
}

struct Running {
    task: JoinHandle<()>,
    source: Source,
    cells: BTreeSet<u32>,
    hedge: bool,
    first_byte: bool,
}

struct Op {
    inner: Arc<Inner>,
    fill: Arc<Fill>,
    entry: Arc<dyn Entry>,
    id: u64,
    life: u64,
    /// The fill's pinned generation when the item was claimed: the `If-Match` of every part.
    generation: Generation,
    class: Class,
    job: JobId,
    rank: Rank,
    tokens: BTreeMap<u32, ClaimedCell>,
    running: BTreeMap<u32, Running>,
    next: u32,
    used: u32,
    hedged: bool,
    /// How often this op asked `LOCATE` again: the first retry after it goes at once, later ones
    /// back off, and so does every retry a failure would otherwise make at once.
    relocations: u32,
    tx: mpsc::Sender<Event>,
    timers: Vec<JoinHandle<()>>,
    /// Publishes handed to the blocking pool and not yet settled.
    publishes: Vec<JoinHandle<()>>,
}

/// What a publish of `cell` (`len` bytes) of `fill` leaves: the cell is no longer the op's, and
/// is published (`ok`) or fails for its waiters.
fn published(inner: &Arc<Inner>, fill: &Arc<Fill>, cell: u32, len: u64, ok: bool) {
    {
        let mut st = inner.lock();
        for fs in st.fills.get_mut(&fill.seq).into_iter() {
            fs.owner.remove(&cell);
            fs.mismatches.remove(&cell);
            fs.wants.remove(&cell);
            fs.unavailable_since = None;
            // A cell whose publish failed fails for its waiters (EIO); a later wait sees a
            // claimed cell no op owns and fails too, until the entry settles it.
            if !ok {
                fs.fail(cell, cell + 1);
            }
        }
        if ok {
            st.stats.cells_published += 1;
            st.stats.bytes_published += len;
        } else {
            st.stats.bytes_wasted += len;
        }
    }
    fill.bump();
    inner.bump();
}

/// Runs the op of a dispatched item whose cells were `claimed`, under `permit`.
pub(crate) async fn run(
    inner: Arc<Inner>,
    d: Dispatch,
    claimed: Claimed,
    kind: GetKind,
    permit: Permit,
) {
    let Claimed {
        op: id,
        entry,
        generation,
        tokens,
    } = claimed;
    let (tx, rx) = mpsc::channel(2);
    let mut op = Op {
        inner,
        fill: d.fill,
        entry,
        id,
        life: d.life,
        generation,
        class: d.class,
        job: d.job,
        rank: d.rank,
        tokens,
        running: BTreeMap::new(),
        next: 0,
        used: 0,
        hedged: false,
        relocations: 0,
        tx,
        timers: Vec::new(),
        publishes: Vec::new(),
    };
    let end = op.start(d.source, kind, permit, rx).await;
    // Nothing reads the attempts' events any more: they stop now, not after the publishes.
    op.stop();
    op.settle().await;
    op.finish(end);
}

impl Op {
    async fn start(
        &mut self,
        source: Option<Source>,
        kind: GetKind,
        permit: Permit,
        rx: mpsc::Receiver<Event>,
    ) -> End {
        let cells: Vec<u32> = self.tokens.keys().copied().collect();
        let offset = self.fill.cell_range(cells[0]).0;
        let source = source
            .filter(|s| !self.excluded(s))
            .or_else(|| self.route(|r| r.first(offset)));
        let Some(source) = source else {
            return self.no_copy();
        };
        self.used = 1;
        self.launch(cells, source, kind, self.class, Some(permit), false);
        if self.class == Class::Demand {
            self.arm_hedge();
        }
        self.drive(rx).await
    }

    fn excluded(&self, s: &Source) -> bool {
        let st = self.inner.lock();
        st.fills
            .get(&self.fill.seq)
            .is_none_or(|fs| fs.excluded.contains(s))
    }

    /// A source chosen by `f` from the fill's current location, if the fill is still this op's.
    fn route(&self, f: impl FnOnce(&Routing<'_>) -> Option<usize>) -> Option<Source> {
        let now = self.inner.now();
        let st = self.inner.lock();
        st.fills
            .get(&self.fill.seq)
            .filter(|fs| fs.life == self.life)
            .and_then(|fs| fs.location.as_ref().map(|location| (fs, location)))
            .and_then(|(fs, location)| {
                let excluded = fs.excluded_indexes();
                let routing = Routing {
                    location,
                    excluded: &excluded,
                    book: &st.book,
                    now,
                };
                f(&routing).map(|i| location.sources[i].clone())
            })
    }

    fn current(&self) -> bool {
        let st = self.inner.lock();
        st.fills
            .get(&self.fill.seq)
            .is_some_and(|fs| (fs.life == self.life) & (fs.phase == Phase::Ready))
    }

    fn launch(
        &mut self,
        cells: Vec<u32>,
        source: Source,
        kind: GetKind,
        class: Class,
        permit: Option<Permit>,
        hedge: bool,
    ) {
        let attempt = self.next;
        self.next += 1;
        let spec = Attempt {
            inner: self.inner.clone(),
            fill: self.fill.clone(),
            attempt,
            cells: cells.clone(),
            source: source.clone(),
            generation: self.generation,
            kind,
            class,
            job: self.job,
            rank: self.rank,
            permit,
            tx: self.tx.clone(),
        };
        let task = self.inner.rt.spawn(spec.run());
        self.running.insert(
            attempt,
            Running {
                task,
                source,
                cells: cells.into_iter().collect(),
                hedge,
                first_byte: false,
            },
        );
    }

    fn arm_hedge(&mut self) {
        let size_class = size_class(self.fill.spec.size);
        let p50 = self.inner.lock().baselines.first_byte_p50(size_class);
        let config = &self.inner.config;
        let after = timeout(config.hedge_floor_ms, config.hedge_factor, p50);
        self.timer(TimerKind::Hedge, after, Event::HedgeDue);
    }

    fn timer(&mut self, kind: TimerKind, after: u64, event: Event) {
        let sleep = Armed::new(&self.inner, kind, self.inner.now().saturating_add(after));
        let tx = self.tx.clone();
        self.timers.push(self.inner.rt.spawn(async move {
            sleep.await;
            // The op may have ended; then nobody needs the event.
            let _ = tx.send(event).await;
        }));
    }

    async fn drive(&mut self, mut rx: mpsc::Receiver<Event>) -> End {
        // The op holds a sender, so the channel never closes while it runs.
        let mut events = stream::poll_fn(move |cx| rx.poll_recv(cx)).fuse();
        while !self.tokens.is_empty() {
            if !self.current() {
                return End::Stale;
            }
            let end = match events.select_next_some().await {
                Event::FirstByte(a) => {
                    for r in self.running.get_mut(&a).into_iter() {
                        r.first_byte = true;
                    }
                    None
                }
                Event::Cell {
                    source,
                    cell,
                    bytes,
                    verdict,
                    slot,
                } => {
                    self.cell(&source, cell, bytes, verdict, slot).await;
                    None
                }
                Event::Ended {
                    attempt,
                    failure,
                    source,
                } => self.ended(attempt, failure, source).await,
                Event::HedgeDue => {
                    self.hedge();
                    None
                }
                Event::RetryDue { cells, avoid } => self.retry(&cells, &avoid),
            };
            if let Some(end) = end {
                return end;
            }
        }
        End::Done
    }

    async fn cell(
        &mut self,
        source: &Source,
        cell: u32,
        bytes: Bytes,
        verdict: Option<Verdict>,
        slot: Slot,
    ) {
        let len = bytes.len() as u64;
        let token = self.tokens.remove(&cell);
        let token = match (token, verdict) {
            (Some(token), Some(Verdict::Mismatch)) => {
                if self.mismatch(cell, token, source) {
                    self.refetch(cell, source);
                }
                None
            }
            (token, _) => token,
        };
        let Some(token) = token else {
            self.inner.with_stats(|s| s.bytes_wasted += len);
            self.inner.pool.recycle(bytes);
            return;
        };
        let t0 = self.inner.now();
        let (room, waited) = self.inner.dirty.take(self.class, len).await;
        let stalled_ms = self.inner.now() - t0;
        let metrics = self.inner.parts.cache_metrics.iter();
        for m in metrics.filter(|_| waited) {
            m.writeback_stall_seconds.observe_ms((), stalled_ms);
        }
        let entry = self.entry.clone();
        let inner = self.inner.clone();
        let publish = tokio::task::spawn_blocking(move || {
            let ok = entry.publish(token, &bytes).is_ok();
            // The cap's share comes back as soon as the publish returns, published or not.
            drop(room);
            inner.pool.recycle(bytes);
            drop(slot);
            ok
        });
        let inner = self.inner.clone();
        let fill = self.fill.clone();
        self.publishes.push(self.inner.rt.spawn(async move {
            let ok = matches!(publish.await, Ok(true));
            published(&inner, &fill, cell, len, ok);
        }));
    }

    /// Waits for the publishes this op handed over.
    async fn settle(&mut self) {
        for p in std::mem::take(&mut self.publishes) {
            // A publish task is never aborted.
            let _ = p.await;
        }
    }

    /// A verification failure of `cell` from `source`: reported, counted, and the token kept
    /// for a refetch (`true`) or the cell marked bad on its second failure (`false`).
    fn mismatch(&mut self, cell: u32, token: ClaimedCell, source: &Source) -> bool {
        let (offset, len) = self.fill.cell_range(cell);
        let upstream = self.inner.parts.upstream.clone();
        let key = self.fill.spec.id.key.clone();
        let reported = source.clone();
        self.inner.rt.spawn(async move {
            upstream
                .report(&key, &reported, ByteRange { offset, len }, Report::Bitrot)
                .await;
        });
        let count = {
            let mut st = self.inner.lock();
            st.stats.cells_mismatched += 1;
            let count = st.fills.get_mut(&self.fill.seq).map_or(2, |fs| {
                let n = fs.mismatches.entry(cell).or_default();
                *n += 1;
                *n
            });
            if count >= 2 {
                st.stats.cells_bad += 1;
                for fs in st.fills.get_mut(&self.fill.seq).into_iter() {
                    fs.owner.remove(&cell);
                }
            }
            count
        };
        if count >= 2 {
            self.entry.mark_bad(token);
            bad_expiry(&self.inner, &self.fill, &self.entry, cell);
            self.fill.bump();
            return false;
        }
        self.tokens.insert(cell, token);
        true
    }

    /// The refetch of a cell that failed verification once: a demand sub-range from the
    /// alternative source.
    fn refetch(&mut self, cell: u32, failed: &Source) {
        let offset = self.fill.cell_range(cell).0;
        let source = self.route(|r| r.alternative(offset, failed.device));
        // With every copy excluded the token stays; the op's end fails the cell.
        for source in source.into_iter() {
            self.launch(
                vec![cell],
                source,
                GetKind::Retry,
                Class::Demand,
                None,
                false,
            );
        }
    }

    async fn ended(&mut self, attempt: u32, failure: Option<Fail>, source: Source) -> Option<End> {
        let hedge = self.running.remove(&attempt).is_some_and(|r| r.hedge);
        self.inner.lock().hedges -= usize::from(hedge);
        // An attempt that ended without a failure but left cells undelivered (a range cut short
        // by its source's extent) is a transport failure for them.
        let failure = failure.unwrap_or(Fail {
            kind: FailKind::Transport,
            source,
        });
        self.note(&failure);
        let covered: BTreeSet<u32> = self
            .running
            .values()
            .flat_map(|r| r.cells.iter().copied())
            .collect();
        let uncovered: Vec<u32> = self
            .tokens
            .keys()
            .copied()
            .filter(|c| !covered.contains(c))
            .collect();
        if uncovered.is_empty() {
            return None;
        }
        self.recover(uncovered, failure).await
    }

    /// What a failure says about the copy and the device, before any retry.
    fn note(&mut self, f: &Fail) {
        let now = self.inner.now();
        let mut guard = self.inner.lock();
        let st = &mut *guard;
        let exclude = match f.kind {
            FailKind::OtherGeneration => {
                st.stats.copy_generation_mismatch += 1;
                true
            }
            // A copy that answered 404 is gone for this fill: a `LOCATE` that names it again
            // (a stale index) does not send the part back to it.
            FailKind::NotFound => true,
            FailKind::Busy(_) => {
                st.book
                    .busy(f.source.device, now + self.inner.config.busy_window_ms);
                false
            }
            _ => false,
        };
        let fills = st.fills.get_mut(&self.fill.seq).into_iter();
        for fs in fills.filter(|fs| exclude & !fs.excluded.contains(&f.source)) {
            fs.excluded.push(f.source.clone());
        }
        drop(guard);
        if let FailKind::Corrupt(offset) = f.kind {
            // The serving node found the cell bad: a verification failure of that cell. A first
            // failure keeps the token for the retry of the remaining cells.
            let cell = u32::try_from(offset / CELL_BYTES).unwrap_or(u32::MAX);
            let token = self.tokens.remove(&cell);
            for token in token.into_iter() {
                self.mismatch(cell, token, &f.source);
            }
        }
    }

    async fn recover(&mut self, cells: Vec<u32>, f: Fail) -> Option<End> {
        let gone =
            f.kind == FailKind::NotFound && !matches!(f.source.kind, SourceKind::HotClean { .. });
        if gone {
            return self.relocate(&cells, &f.source).await;
        }
        if self.used >= self.inner.config.max_attempts {
            return self.exhausted(&cells, &f.source).await;
        }
        let delay = self.delay(&f);
        if delay == 0 {
            return self.retry(&cells, &f.source);
        }
        self.timer(
            TimerKind::Backoff,
            delay,
            Event::RetryDue {
                cells,
                avoid: f.source,
            },
        );
        None
    }

    /// How long a retry after `f` waits.
    fn delay(&self, f: &Fail) -> u64 {
        let config = &self.inner.config;
        let now = self.inner.now();
        let mut st = self.inner.lock();
        let exp = config
            .backoff_base_ms
            .saturating_mul(1u64 << self.used.min(16))
            .min(config.backoff_max_ms);
        match f.kind {
            FailKind::Busy(retry_after) => retry_after.max(st.jittered(exp)),
            FailKind::DriveStalled => {
                st.stats.stall_retries += 1;
                self.again(&mut st, exp)
            }
            FailKind::Stall => {
                st.stats.stall_retries += 1;
                let window = config.stall_retry_window_ms;
                // At most one stall retry per drive per window; and while the drive is in a
                // decrease state a stall (unlike a transport failure or a `stalled` drive) waits
                // for the state to clear (4.6).
                let next = st
                    .stall_retry_at
                    .get(&f.source.device)
                    .map_or(now, |last| (last + window).max(now))
                    .max(st.book.get(f.source.device).busy_until);
                st.stall_retry_at.insert(f.source.device, next);
                next - now
            }
            _ => self.again(&mut st, exp),
        }
    }

    /// The wait of a retry the failure itself allows at once: none, unless this op already asked
    /// `LOCATE` again, so a source that keeps refusing at once is not asked in a tight loop.
    fn again(&self, st: &mut crate::engine::State, exp: u64) -> u64 {
        let backoff = st.jittered(exp);
        [0, backoff][usize::from(self.relocations > 0)]
    }

    /// A retry of `cells` (those still claimed and not being fetched) on the alternative to
    /// `avoid`.
    fn retry(&mut self, cells: &[u32], avoid: &Source) -> Option<End> {
        let covered: BTreeSet<u32> = self
            .running
            .values()
            .flat_map(|r| r.cells.iter().copied())
            .collect();
        // Cells published or being fetched by another attempt since the retry was planned are
        // left out; an attempt with no cell left ends at once.
        let cells: Vec<u32> = cells
            .iter()
            .copied()
            .filter(|c| self.tokens.contains_key(c) & !covered.contains(c))
            .collect();
        let offset = cells.first().map_or(0, |c| self.fill.cell_range(*c).0);
        let Some(source) = self.route(|r| r.alternative(offset, avoid.device)) else {
            return Some(self.no_copy());
        };
        self.used += 1;
        self.launch(cells, source, GetKind::Retry, self.class, None, false);
        None
    }

    /// No source of the pinned generation is left: the fill fails (EIO), resumable.
    fn no_copy(&self) -> End {
        let mut guard = self.inner.lock();
        let st = &mut *guard;
        let fills = st.fills.get_mut(&self.fill.seq).into_iter();
        for fs in fills.filter(|fs| fs.life == self.life) {
            crate::resolve::fail_life(fs);
            st.stats.fills_failed += 1;
        }
        drop(guard);
        self.fill.bump();
        End::Stale
    }

    async fn exhausted(&mut self, cells: &[u32], failed: &Source) -> Option<End> {
        let now = self.inner.now();
        let expired = {
            let mut st = self.inner.lock();
            st.fills.get_mut(&self.fill.seq).is_none_or(|fs| {
                let since = *fs.unavailable_since.get_or_insert(now);
                now - since >= self.inner.config.unavailable_wait_ms
            })
        };
        if expired {
            return Some(End::Eio);
        }
        self.relocate(cells, failed).await
    }

    /// Asks `LOCATE` again (single flight per fill) and retries on what it finds.
    async fn relocate(&mut self, cells: &[u32], failed: &Source) -> Option<End> {
        // A fill lost or failed by another op's resolve is not asked about again.
        if !self.current() {
            return Some(End::Stale);
        }
        let resolving = {
            let mut st = self.inner.lock();
            let fs = st.fills.get_mut(&self.fill.seq);
            fs.map(|fs| crate::resolve::resolving(&self.inner, fs))
        };
        let resolved = OptionFuture::from(resolving).await;
        if resolved != Some(Resolved::Found) {
            return Some(End::Stale);
        }
        self.used = 0;
        self.relocations += 1;
        if self.relocations == 1 {
            return self.retry(cells, failed);
        }
        // A location that keeps being found while its copies keep failing: retry at the pace of
        // a backoff, until `unavailable_wait` ends it.
        let config = &self.inner.config;
        let exp = config
            .backoff_base_ms
            .saturating_mul(1u64 << self.relocations.min(16))
            .min(config.backoff_max_ms);
        let delay = self.inner.lock().jittered(exp);
        self.timer(
            TimerKind::Backoff,
            delay,
            Event::RetryDue {
                cells: cells.to_vec(),
                avoid: failed.clone(),
            },
        );
        None
    }

    fn hedge(&mut self) {
        let started = self.hedged | self.running.values().any(|r| r.first_byte);
        let current = self.running.values().next().filter(|_| !started);
        let Some(current) = current.map(|r| r.source.clone()) else {
            return;
        };
        let cells: Vec<u32> = self.tokens.keys().copied().collect();
        let offset = self.fill.cell_range(cells[0]).0;
        let len = self.fill.bytes_of(&cells);
        let degraded_max = self.inner.config.degraded_hedge_max;
        let Some(source) = self.route(|r| r.hedge(offset, len, &current, degraded_max)) else {
            return;
        };
        {
            let mut st = self.inner.lock();
            if st.hedges >= self.inner.config.hedge_permits {
                return;
            }
            st.hedges += 1;
        }
        self.hedged = true;
        self.launch(cells, source, GetKind::Hedge, Class::Demand, None, true);
    }

    /// Aborts the running attempts and the timers.
    fn stop(&self) {
        for r in self.running.values() {
            r.task.abort();
        }
        for t in &self.timers {
            t.abort();
        }
    }

    fn finish(mut self, end: End) {
        let hedges = self.running.values().filter(|r| r.hedge).count();
        let tokens = std::mem::take(&mut self.tokens);
        let left: Vec<u32> = tokens.keys().copied().collect();
        for token in tokens.into_values() {
            self.entry.abandon(token);
        }
        let mut st = self.inner.lock();
        st.hedges -= hedges;
        for fs in st.fills.get_mut(&self.fill.seq).into_iter() {
            for cell in &left {
                fs.owner.remove(cell);
            }
            if end == End::Eio {
                for run in runs_of(&left) {
                    fs.fail(run.first, run.first + run.count);
                }
            }
            fs.ops.remove(&self.id);
        }
        st.refresh(self.fill.seq, &self.inner.config);
        st.retire(self.fill.seq);
        drop(st);
        // The op may have freed a slot of the active-pack window, which other fills' items wait
        // for: every class looks again.
        self.inner.wake_classes(ALL_CLASSES);
        self.fill.bump();
        self.inner.bump();
    }
}
