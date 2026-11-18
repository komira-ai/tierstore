//! One pack's fill (4.1): the shared part every task of the fill holds ([`Fill`]) and the
//! scheduling state the engine keeps under its lock ([`FillSched`]): who wants which cells at
//! which class and rank, the plan, which op owns which claimed cell, the location and the pinned
//! generation, failures for waiters, and the fill's phase.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use futures::future::{BoxFuture, Shared};
use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::engine::PackSpec;
use tierstore_fetch_api::entry::{Entry, StateView};
use tierstore_fetch_api::ids::{Class, JobId, PackRoot, Rank};
use tierstore_fetch_api::upstream::{Location, Source};
use tierstore_format::ids::Generation;
use tokio::sync::watch;

use crate::config::EngineConfig;
use tierstore_fetch_api::page::CELL_SHIFT;

use crate::plan::{CELL_BYTES, Piece};
use crate::queue::{ItemRef, Queued};

/// The bit of an item id that marks a demand sub-range (the low 32 bits are its first cell);
/// without it, the id is a piece index.
pub(crate) const SUB_ITEM: u64 = 1 << 32;

/// The part of a fill every task holds.
pub(crate) struct Fill {
    /// The fill's sequence number: its key in the engine's state.
    pub seq: u64,
    /// The pack.
    pub spec: PackSpec,
    /// Its cells.
    pub cells: u32,
    /// The entry, once opened.
    pub entry: OnceLock<Arc<dyn Entry>>,
    /// Bumped on every change a waiter may be waiting for.
    pub changed: watch::Sender<u64>,
}

impl Fill {
    pub(crate) fn new(seq: u64, spec: PackSpec, cells: u32) -> Self {
        Self {
            seq,
            spec,
            cells,
            entry: OnceLock::new(),
            changed: watch::Sender::new(0),
        }
    }

    /// Wakes the fill's waiters.
    pub(crate) fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// The byte range of `cell`: its offset and length (the last cell may be short).
    pub(crate) fn cell_range(&self, cell: u32) -> (u64, u64) {
        let start = u64::from(cell) << CELL_SHIFT;
        let end = (start + CELL_BYTES).min(self.spec.size);
        (start, end.saturating_sub(start))
    }

    /// The bytes of `cells`.
    pub(crate) fn bytes_of(&self, cells: &[u32]) -> u64 {
        cells.iter().map(|c| self.cell_range(*c).1).sum()
    }
}

/// Who wants a cell, at which class and rank (the best want wins: lower class, then lower rank).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Want {
    pub class: Class,
    pub job: JobId,
    pub rank: Rank,
}

/// Where a fill is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Nothing in progress: the next want opens the entry and asks `LOCATE`.
    Idle,
    /// Opening the entry and asking `LOCATE`.
    Starting,
    /// The location is known and parts can go.
    Ready,
    /// Lost (4.6): every waiter of an unsatisfied cell gets `lost`, until the engine forgets it.
    Lost,
}

/// Cells that failed with EIO for the waiters that were waiting when they failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Failure {
    pub seq: u64,
    pub first: u32,
    pub end: u32,
}

/// What [`crate::resolve`] ends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The location is (again) known.
    Found,
    /// The pack is lost.
    Lost,
    /// No copy of the pinned generation, the entry did not open, or the waits ran out with the
    /// location found: EIO, resumable.
    Failed,
}

/// The single-flight resolve of a fill.
pub(crate) type Resolving = Shared<BoxFuture<'static, Resolved>>;

/// The scheduling state of one fill (under the engine's lock).
pub(crate) struct FillSched {
    pub fill: Arc<Fill>,
    pub phase: Phase,
    /// Incremented when the fill fails or is lost: an op of an older life stops.
    pub life: u64,
    pub location: Option<Location>,
    /// The generation the fill takes: the pack id's (an S3-front object), else its location's.
    /// Set whenever the fill is ready.
    pub pinned: Option<Generation>,
    /// Copies that hold another generation or are gone (a promoted copy).
    pub excluded: Vec<Source>,
    pub pieces: Vec<Piece>,
    /// The first cell anyone asked for, and the cells of that request.
    pub first_touch: Option<(u32, u32)>,
    pub wants: BTreeMap<u32, Want>,
    /// The op that owns each claimed cell.
    pub owner: BTreeMap<u32, u64>,
    /// Ops in flight and their class.
    pub ops: BTreeMap<u64, Class>,
    /// Items picked and waiting for a permit, and their class.
    pub dispatching: Vec<(u64, Class)>,
    /// The spans of the demand sub-ranges last queued, by first cell.
    pub subs: BTreeMap<u32, u32>,
    pub failures: Vec<Failure>,
    /// The fill's event counter (a waiter sees only failures after it started).
    pub seq: u64,
    pub waiters: usize,
    pub resolving: Option<Resolving>,
    /// When a part first ran out of attempts with the location found, if none landed since.
    pub unavailable_since: Option<u64>,
    /// Verification failures per cell.
    pub mismatches: BTreeMap<u32, u32>,
    pub rechecking: BTreeSet<u32>,
    pub evict_poll: bool,
}

/// Failures kept per fill for waiters; older ones are dropped (their waiters have long seen
/// newer events).
const FAILURES_KEPT: usize = 64;

impl FillSched {
    pub(crate) fn new(fill: Arc<Fill>) -> Self {
        let pinned = match fill.spec.id.root {
            PackRoot::Etag(g) => Some(g),
            PackRoot::Digest(_) => None,
        };
        Self {
            fill,
            phase: Phase::Idle,
            life: 0,
            location: None,
            pinned,
            excluded: Vec::new(),
            pieces: Vec::new(),
            first_touch: None,
            wants: BTreeMap::new(),
            owner: BTreeMap::new(),
            ops: BTreeMap::new(),
            dispatching: Vec::new(),
            subs: BTreeMap::new(),
            failures: Vec::new(),
            seq: 0,
            waiters: 0,
            resolving: None,
            unavailable_since: None,
            mismatches: BTreeMap::new(),
            rechecking: BTreeSet::new(),
            evict_poll: false,
        }
    }

    /// Records that `job` wants `cells` at `class` and `rank`; whether any want changed.
    pub(crate) fn want(&mut self, cells: impl Iterator<Item = u32>, w: Want) -> bool {
        let mut changed = false;
        for c in cells.take_while(|c| *c < self.fill.cells) {
            self.first_touch.get_or_insert((c, 0));
            let better = self
                .wants
                .get(&c)
                .is_none_or(|old| (w.class, w.rank) < (old.class, old.rank));
            if better {
                self.wants.insert(c, w);
                changed = true;
            }
        }
        changed
    }

    /// Records the size of the first request, for the plan's first piece.
    pub(crate) fn touch_len(&mut self, first: u32, count: u32) {
        if let Some((f, n)) = self.first_touch.as_mut()
            && *f == first
            && *n == 0
        {
            *n = count;
        }
    }

    /// Fails `cells` with EIO for every waiter waiting now; their wants are dropped.
    pub(crate) fn fail(&mut self, first: u32, end: u32) {
        self.seq += 1;
        self.failures.push(Failure {
            seq: self.seq,
            first,
            end,
        });
        let excess = self.failures.len().saturating_sub(FAILURES_KEPT);
        self.failures.drain(..excess);
        let failed: Vec<u32> = self.wants.range(first..end).map(|(c, _)| *c).collect();
        for c in failed {
            self.wants.remove(&c);
        }
    }

    /// The first cell of `first..end` that failed after event `since`.
    pub(crate) fn failed_since(&self, since: u64, first: u32, end: u32) -> Option<u32> {
        self.failures
            .iter()
            .filter(|f| (f.seq > since) & (f.first < end) & (first < f.end))
            .map(|f| f.first.max(first))
            .min()
    }

    fn pending(&self, view: &dyn StateView, c: u32) -> bool {
        self.wants.contains_key(&c)
            && !self.owner.contains_key(&c)
            && view.load(c).and_then(|b| b.state()) == Some(CellState::Absent)
    }

    /// Drops the wants of cells that no longer need a fetch (present, or anything but absent and
    /// unowned that will not become absent by itself).
    fn prune(&mut self, view: &dyn StateView) {
        let done: Vec<u32> = self
            .wants
            .keys()
            .copied()
            .filter(|c| {
                matches!(
                    view.load(*c).and_then(|b| b.state()),
                    Some(CellState::Present | CellState::Bad) | None
                )
            })
            .collect();
        for c in done {
            self.wants.remove(&c);
        }
    }

    fn item(&self, class: Class, id: u64, cells: &[u32]) -> Queued {
        // Every cell of an item is wanted: the item is made from pending cells only.
        let first = cells.iter().copied().min().unwrap_or(0);
        let w = cells
            .iter()
            .filter_map(|c| self.wants.get(c))
            .find(|w| w.class == class)
            .copied()
            .unwrap_or(Want {
                class,
                job: JobId::default(),
                rank: Rank::default(),
            });
        Queued {
            job: w.job,
            rank: w.rank,
            offset: self.fill.cell_range(first).0,
            size: self.fill.bytes_of(cells),
            what: ItemRef {
                fill: self.fill.seq,
                item: id,
            },
        }
    }

    /// The items this fill would queue now, by class (4.5): for each piece with pending cells,
    /// a demand sub-range of its demanded cells when they span at most `sub_range_max_cells` and
    /// less than the whole piece,
    /// and the piece's other pending cells at their best class. Items being dispatched at the
    /// same or a higher class are left out. Nothing while the fill is not ready.
    pub(crate) fn items(&mut self, config: &EngineConfig) -> Vec<(Class, Queued)> {
        self.subs.clear();
        let Some(entry) = self.fill.entry.get().cloned() else {
            return Vec::new();
        };
        let view = entry.state();
        self.prune(view);
        if self.phase != Phase::Ready {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (i, piece) in self.pieces.iter().enumerate() {
            let pend: Vec<u32> = (piece.first..piece.end)
                .filter(|c| self.pending(view, *c))
                .collect();
            let demand: Vec<u32> = pend
                .iter()
                .copied()
                .filter(|c| self.wants.get(c).is_some_and(|w| w.class == Class::Demand))
                .collect();
            let mut rest = pend.clone();
            if let (Some(&lo), Some(&hi)) = (demand.first(), demand.last())
                && hi - lo < config.sub_range_max_cells
                && hi - lo + 1 < piece.cells()
            {
                let sub: Vec<u32> = pend
                    .iter()
                    .copied()
                    .filter(|c| (lo..=hi).contains(c))
                    .collect();
                rest.retain(|c| !(lo..=hi).contains(c));
                self.subs.insert(lo, hi + 1);
                out.push((
                    Class::Demand,
                    self.item(Class::Demand, SUB_ITEM | u64::from(lo), &sub),
                ));
            }
            let class = rest
                .iter()
                .filter_map(|c| self.wants.get(c))
                .map(|w| w.class)
                .min();
            if let Some(class) = class {
                out.push((class, self.item(class, i as u64, &rest)));
            }
        }
        out.retain(|(class, q)| {
            !self
                .dispatching
                .iter()
                .any(|(item, d)| *item == q.what.item && d <= class)
        });
        out
    }

    /// The cells an item would claim now: the pending cells of its piece, or of its sub-range.
    pub(crate) fn claimable(&self, item: u64) -> Vec<u32> {
        let (first, end) = if item & SUB_ITEM != 0 {
            // The low 32 bits of a sub-range's id are its first cell.
            let lo = (item & !SUB_ITEM) as u32;
            (lo, self.subs.get(&lo).copied().unwrap_or(lo + 1))
        } else {
            self.pieces
                .get(item as usize)
                .map_or((0, 0), |p| (p.first, p.end))
        };
        // An item is queued only once the entry is open.
        self.fill
            .entry
            .get()
            .map(|entry| {
                (first..end)
                    .filter(|c| self.pending(entry.state(), *c))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether nothing of the fill is in progress or wanted, so the engine may forget it.
    pub(crate) fn idle(&self) -> bool {
        (self.waiters == 0)
            & self.ops.is_empty()
            & self.dispatching.is_empty()
            & self.resolving.is_none()
            & self.wants.is_empty()
            & self.rechecking.is_empty()
            & (self.phase != Phase::Starting)
    }

    /// The location with the excluded copies, as indexes for [`crate::route::Routing`].
    pub(crate) fn excluded_indexes(&self) -> BTreeSet<usize> {
        self.location
            .iter()
            .flat_map(|l| l.sources.iter().enumerate())
            .filter(|(_, s)| self.excluded.contains(s))
            .map(|(i, _)| i)
            .collect()
    }
}
