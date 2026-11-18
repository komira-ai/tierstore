//! The loom model of the reader/evictor protocol of 5.1 and of the claim and publish of a cell.
//!
//! [`check`] runs a [`Scenario`] (the actors and the size of the pack) under a [`Protocol`] (the
//! design's protocol, or one with a rule switched off) inside `loom::model`, which explores the
//! interleavings of every atomic operation of every actor, and returns a [`Report`]. Violations of
//! the properties are recorded, not panicked, so a test can assert both that the design's protocol
//! has none and that a protocol without a rule has the matching one.
//!
//! # What is modelled
//!
//! Per cell: a state byte (loom `AtomicU8`; every transition is a compare-exchange whose new byte
//! is computed by the frozen [`Transition::apply`] on the loaded byte, so the CAS rule, durable
//! bit included, is the one of `tierstore-fetch-api`), a data tag (nonzero: the published bytes,
//! zero: punched or never written) and a `punch_begun` flag the evictor sets when it starts the
//! punch, after journaling. Per holder and group: a reader counter (loom `AtomicUsize`). The group
//! size and the number of holders are parameters of the [`Scenario`] (the real group is 16 cells;
//! the scenarios use 1 or 2 so a straddling reader is cheap).
//!
//! The actors:
//!
//! - a reader over a cell range ([`ReaderSpec`]): 5.1 reader steps 1 to 5 (count the groups, load
//!   the states, read the data, recheck, commit, release). Its read of a cell is tainted when the
//!   cell's data is zero or `punch_begun` is set when the read is done; committing a tainted
//!   read is the violation [`Violation::StaleBytesCommitted`];
//! - the evictor ([`EvictorSpec`]): steps 1 to 4: CAS `present -> evicting` on each candidate,
//!   remembering the won cells; poll the summed counters of each won cell's group (with
//!   `loom::thread::yield_now`, at most `wait_polls` re-polls); on a timeout CAS that group's won
//!   cells back to `present` and count the timeout; otherwise journal (counted), set
//!   `punch_begun`, zero the data, CAS `evicting -> absent`;
//! - a filler per neighbouring cell: `absent -> claimed`, write the data, `claimed -> present`;
//! - a forced loss: one store of zero over a holder's group counter (a later release saturates
//!   at zero);
//! - a stuck holder: a counter that starts at 1 and is never released;
//! - a hedge on one cell: the cell is claimed once and two deliverers race for the token slot;
//!   whichever takes the token writes the data and publishes, the other writes nothing.
//!
//! loom treats a `SeqCst` access as acquire/release only (it supports `fence(SeqCst)`), which
//! would allow the store-buffering outcome the Dekker argument of 5.1 excludes. The model puts a
//! `SeqCst` fence between each store of the pair and the loads after it (after the reader's
//! `fetch_add`, after the evictor's step 1): the guarantee the design takes from `SeqCst` on both
//! sides.
//!
//! # Properties
//!
//! A reader never commits a tainted read; the evictor never changes a `claimed` cell
//! ([`Violation::ClaimedCellChanged`]); on a timeout nothing is journaled yet
//! ([`Violation::JournaledBeforeTimeout`]) and every won cell of the group is `present` again at
//! the end ([`Violation::WonCellNotRestored`]); the hedge's data is written once and published
//! once ([`Violation::CellWrittenNotOnce`], [`Violation::CellPublishedNotOnce`]).
//!
//! # Bounds
//!
//! loom runs with the preemption bound of [`Scenario::preemption_bound`] (3 from
//! [`Scenario::new`]), at most [`MAX_BRANCHES`] scheduling points per execution, and at most 4
//! spawned actors (loom allows 5 threads with the main thread). The scenarios of the tests have
//! at most 4 cells, 2 holders and a `wait_polls` of 1 or 2.
//!
//! # Panics
//!
//! [`check`] panics when a [`Scenario`] names a cell, group or holder outside its sizes or has a
//! reader of no cells (a bug of the test), or when an execution exceeds [`MAX_BRANCHES`] (a
//! scenario too large).

use std::collections::BTreeSet;
use std::ops::{Range, RangeInclusive};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};

use loom::sync::Mutex as LoomMutex;
use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize as LoomUsize, Ordering::SeqCst, fence};
use loom::thread::{self, JoinHandle};
use tierstore_fetch_api::cell::{CellByte, CellState, Transition};
use tierstore_fetch_api::entry::ClaimedCell;

/// The most scheduling points loom explores in one execution before it panics.
pub const MAX_BRANCHES: usize = 20_000;

/// The rules of the protocol; all `true` is the design's protocol (5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protocol {
    /// Reader step 4: reload the states after the read and discard the bytes if any cell is not
    /// `present`.
    pub post_read_recheck: bool,
    /// Evictor steps 2 and 3: wait for the group counters to sum to zero before the punch.
    pub evictor_waits_for_readers: bool,
    /// Reader step 1 before step 2: count the groups, then load the states (the Dekker order).
    pub reader_counts_before_load: bool,
    /// Evictor step 1: CAS from `present` on each cell. When `false`, one plain store of
    /// `evicting` over every candidate.
    pub evictor_per_cell_cas: bool,
    /// Evictor step 3 then 4: wait first, journal second. When `false`, the batch is journaled
    /// before the wait.
    pub wait_before_journal: bool,
    /// A hedge and its original share one token slot. When `false`, each has its own token.
    pub hedge_shares_token: bool,
}

impl Protocol {
    /// The design's protocol: every rule on.
    #[must_use]
    pub const fn design() -> Self {
        Self {
            post_read_recheck: true,
            evictor_waits_for_readers: true,
            reader_counts_before_load: true,
            evictor_per_cell_cas: true,
            wait_before_journal: true,
            hedge_shares_token: true,
        }
    }
}

/// A reader of the cells `first .. first + len` for holder `holder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderSpec {
    /// The holder whose counters the reader uses.
    pub holder: usize,
    /// The first cell of the range.
    pub first: usize,
    /// The number of cells of the range (at least 1).
    pub len: usize,
}

/// The evictor: candidate cells and how long it waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictorSpec {
    /// The cells of the scan, in order.
    pub candidates: Vec<usize>,
    /// How many times it re-polls a group's counters (after the first look) before the timeout.
    pub wait_polls: usize,
}

/// The actors and sizes of one model run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// The number of cells.
    pub cells: usize,
    /// The cells per counter group.
    pub group_size: usize,
    /// The number of holders (each has its own counter per group).
    pub holders: usize,
    /// The cells that start `present` with data; every other cell starts `absent`.
    pub present: Vec<usize>,
    /// Whether the initial `present` cells carry the durable bit.
    pub durable: bool,
    /// The readers.
    pub readers: Vec<ReaderSpec>,
    /// The evictor, if any.
    pub evictor: Option<EvictorSpec>,
    /// The cells a filler claims, writes and publishes (one filler each).
    pub fillers: Vec<usize>,
    /// A forced loss: `(holder, group)` whose counter is stored to zero once.
    pub forced_clear: Option<(usize, usize)>,
    /// Holders that never release: `(holder, group)` counters that start at 1.
    pub stuck: Vec<(usize, usize)>,
    /// A cell claimed once and delivered by two racing deliverers (it must start `absent`).
    pub hedge: Option<usize>,
    /// loom's preemption bound.
    pub preemption_bound: usize,
}

impl Scenario {
    /// A scenario of `cells` cells in groups of `group_size` for `holders` holders, with no cell
    /// present and no actor; the preemption bound is 3.
    #[must_use]
    pub fn new(cells: usize, group_size: usize, holders: usize) -> Self {
        Self {
            cells,
            group_size,
            holders,
            present: Vec::new(),
            durable: false,
            readers: Vec::new(),
            evictor: None,
            fillers: Vec::new(),
            forced_clear: None,
            stuck: Vec::new(),
            hedge: None,
            preemption_bound: 3,
        }
    }
}

/// A property the model found violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Violation {
    /// A reader committed bytes of a cell whose data was zero or whose punch had begun.
    StaleBytesCommitted,
    /// The evictor's step 1 overwrote a `claimed` cell.
    ClaimedCellChanged,
    /// The batch was journaled before a group's timeout.
    JournaledBeforeTimeout,
    /// A won cell of a timed-out group was not `present` at the end.
    WonCellNotRestored,
    /// The hedged cell's data was not written exactly once.
    CellWrittenNotOnce,
    /// The hedged cell was not published exactly once.
    CellPublishedNotOnce,
}

/// The result of [`check`]. The counts are sums over every execution loom explored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The executions (interleavings) explored.
    pub executions: usize,
    /// The violations found, each once, in order.
    pub violations: Vec<Violation>,
    /// The `evict_timeouts_total` increments (one per timed-out group).
    pub timeouts: usize,
    /// The `PUNCH` records journaled.
    pub journal_records: usize,
    /// The readers that committed.
    pub commits: usize,
    /// The readers that discarded their bytes at the post-read recheck.
    pub discards: usize,
    /// The readers that found a cell not `present` at step 2 and went to wait.
    pub waits: usize,
}

#[derive(Default)]
struct Shared {
    violations: Mutex<BTreeSet<Violation>>,
    executions: AtomicUsize,
    timeouts: AtomicUsize,
    journal: AtomicUsize,
    commits: AtomicUsize,
    discards: AtomicUsize,
    waits: AtomicUsize,
}

/// One execution's cells and counters.
struct Model {
    group_size: usize,
    states: Vec<AtomicU8>,
    data: Vec<LoomUsize>,
    punch_begun: Vec<AtomicBool>,
    counters: Vec<Vec<LoomUsize>>,
    // Bookkeeping that is no part of the protocol: plain atomics add no scheduling points.
    writes: AtomicUsize,
    published: AtomicUsize,
    journaled_now: AtomicUsize,
    shared: Arc<Shared>,
}

/// Run the scenario under the protocol and report what loom found.
#[must_use]
pub fn check(scenario: &Scenario, protocol: &Protocol) -> Report {
    let shared = Arc::new(Shared::default());
    let (sc, pr, sh) = (Arc::new(scenario.clone()), *protocol, Arc::clone(&shared));
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(scenario.preemption_bound);
    builder.max_branches = MAX_BRANCHES;
    builder.check(move || run_once(&sc, pr, &sh));
    let violations = shared
        .violations
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .copied()
        .collect();
    Report {
        executions: shared.executions.load(Relaxed),
        violations,
        timeouts: shared.timeouts.load(Relaxed),
        journal_records: shared.journal.load(Relaxed),
        commits: shared.commits.load(Relaxed),
        discards: shared.discards.load(Relaxed),
        waits: shared.waits.load(Relaxed),
    }
}

impl Model {
    fn new(sc: &Scenario, shared: &Arc<Shared>) -> Self {
        let present = CellByte::new(CellState::Present, sc.durable).0;
        let initial = |c: usize| sc.present.contains(&c);
        let counters = (0..sc.holders)
            .map(|h| {
                (0..sc.cells.div_ceil(sc.group_size))
                    .map(|g| LoomUsize::new(usize::from(sc.stuck.contains(&(h, g)))))
                    .collect()
            })
            .collect();
        Self {
            group_size: sc.group_size,
            states: (0..sc.cells)
                .map(|c| AtomicU8::new(if initial(c) { present } else { 0 }))
                .collect(),
            data: (0..sc.cells)
                .map(|c| LoomUsize::new(usize::from(initial(c))))
                .collect(),
            punch_begun: (0..sc.cells).map(|_| AtomicBool::new(false)).collect(),
            counters,
            writes: AtomicUsize::new(0),
            published: AtomicUsize::new(0),
            journaled_now: AtomicUsize::new(0),
            shared: Arc::clone(shared),
        }
    }

    /// Record `violation` unless `ok`. Every property goes through here.
    fn ensure(&self, ok: bool, violation: Violation) {
        if !ok {
            self.shared
                .violations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(violation);
        }
    }

    fn group(&self, cell: usize) -> usize {
        cell / self.group_size
    }

    /// One CAS of the cell byte by the frozen rule; `false` when the cell is not in the
    /// transition's predecessor state.
    fn cas(&self, cell: usize, t: Transition) -> bool {
        let state = &self.states[cell];
        loop {
            let cur = state.load(SeqCst);
            let Some(new) = t.apply(CellByte(cur)) else {
                return false;
            };
            if state.compare_exchange(cur, new.0, SeqCst, SeqCst).is_ok() {
                return true;
            }
        }
    }

    fn servable(&self, cell: usize) -> bool {
        CellByte(self.states[cell].load(SeqCst)).servable()
    }

    /// Load the cells of the range in order and say whether all were `present` (the load stops at
    /// the first that is not).
    fn all_servable(&self, cells: Range<usize>) -> bool {
        {
            let mut cells = cells;
            cells.all(|c| self.servable(c))
        }
    }

    /// Count the groups up (`fetch_add`) or down; a release saturates at zero, since a forced
    /// loss may already have zeroed the counter.
    fn count(&self, holder: usize, groups: RangeInclusive<usize>, up: bool) {
        for g in groups {
            let counter = &self.counters[holder][g];
            if up {
                counter.fetch_add(1, SeqCst);
                // See the module docs: loom needs the fence for the Dekker pair.
                fence(SeqCst);
            } else {
                // A failed update means the counter was already zero: nothing to release.
                let _ = counter.fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1));
            }
        }
    }

    fn sum(&self, group: usize) -> usize {
        self.counters.iter().map(|h| h[group].load(SeqCst)).sum()
    }

    fn write_and_publish(&self, token: &ClaimedCell) {
        let cell = token.cell() as usize;
        self.writes.fetch_add(1, Relaxed);
        self.data[cell].store(1, SeqCst);
        let won = self.cas(cell, Transition::Publish);
        self.published.fetch_add(usize::from(won), Relaxed);
    }
}

/// The token of a cell, only for a won `absent -> claimed`.
fn token(model: &Model, cell: usize) -> Option<ClaimedCell> {
    model
        .cas(cell, Transition::Claim)
        .then(|| ClaimedCell::won(u32::try_from(cell).unwrap_or(u32::MAX)))
}

/// 5.1 reader steps 1 to 5 over one range.
fn reader(m: &Model, p: &Protocol, r: &ReaderSpec) {
    let cells = r.first..r.first + r.len;
    let groups = m.group(r.first)..=m.group(r.first + r.len - 1);
    if p.reader_counts_before_load {
        m.count(r.holder, groups.clone(), true);
    }
    let ready = m.all_servable(cells.clone());
    if !p.reader_counts_before_load && ready {
        m.count(r.holder, groups.clone(), true);
    }
    if !ready {
        if p.reader_counts_before_load {
            m.count(r.holder, groups, false);
        }
        m.shared.waits.fetch_add(1, Relaxed);
        return;
    }
    let mut tainted = false;
    for c in cells.clone() {
        // `punch_begun` is set before the data is zeroed and never cleared, so a read that
        // overlapped the punch sees the flag set after it (or zero data).
        let tag = m.data[c].load(SeqCst);
        let begun = m.punch_begun[c].load(SeqCst);
        tainted |= begun || tag == 0;
    }
    if p.post_read_recheck && !m.all_servable(cells) {
        m.count(r.holder, groups, false);
        m.shared.discards.fetch_add(1, Relaxed);
        return;
    }
    m.ensure(!tainted, Violation::StaleBytesCommitted);
    m.shared.commits.fetch_add(1, Relaxed);
    m.count(r.holder, groups, false);
}

/// Poll the group's counters until they sum to zero (`true`) or `polls` re-polls have found them
/// busy (`false`).
fn wait_clear(m: &Model, group: usize, polls: usize) -> bool {
    let mut seen = 0;
    loop {
        if m.sum(group) == 0 {
            return true;
        }
        if seen >= polls {
            return false;
        }
        seen += 1;
        thread::yield_now();
    }
}

fn journal(m: &Model, records: usize) {
    m.journaled_now.fetch_add(records, Relaxed);
    m.shared.journal.fetch_add(records, Relaxed);
}

/// 5.1 evictor steps 1 to 4; returns the won cells of the groups that timed out.
fn evictor(m: &Model, p: &Protocol, e: &EvictorSpec) -> Vec<usize> {
    let evicting = CellByte::new(CellState::Evicting, false).0;
    let won: Vec<usize> = e
        .candidates
        .iter()
        .copied()
        .filter(|&c| {
            if p.evictor_per_cell_cas {
                m.cas(c, Transition::BeginEvict)
            } else {
                let old = CellByte(m.states[c].swap(evicting, SeqCst));
                m.ensure(
                    old.state() != Some(CellState::Claimed),
                    Violation::ClaimedCellChanged,
                );
                true
            }
        })
        .collect();
    // The store side of the Dekker pair is done: loom needs the fence before the counter loads.
    fence(SeqCst);
    if !p.wait_before_journal {
        journal(m, won.len());
    }
    let groups: BTreeSet<usize> = won.iter().map(|&c| m.group(c)).collect();
    let mut timed_out = BTreeSet::new();
    for &g in &groups {
        if p.evictor_waits_for_readers && !wait_clear(m, g, e.wait_polls) {
            timed_out.insert(g);
        }
    }
    for _ in &timed_out {
        m.shared.timeouts.fetch_add(1, Relaxed);
        m.ensure(
            m.journaled_now.load(Relaxed) == 0,
            Violation::JournaledBeforeTimeout,
        );
    }
    let (late, clear): (Vec<usize>, Vec<usize>) =
        won.iter().partition(|&&c| timed_out.contains(&m.group(c)));
    for &c in &late {
        m.cas(c, Transition::AbortEvict);
    }
    if p.wait_before_journal {
        journal(m, clear.len());
    }
    for &c in &clear {
        m.punch_begun[c].store(true, SeqCst);
        m.data[c].store(0, SeqCst);
        m.cas(c, Transition::FinishEvict);
    }
    late
}

fn filler(m: &Model, cell: usize) {
    if let Some(t) = token(m, cell) {
        m.write_and_publish(&t);
    }
}

fn go(handles: &mut Vec<JoinHandle<()>>, f: impl FnOnce() + Send + 'static) {
    handles.push(thread::spawn(f));
}

fn run_once(sc: &Arc<Scenario>, p: Protocol, shared: &Arc<Shared>) {
    shared.executions.fetch_add(1, Relaxed);
    let m = Arc::new(Model::new(sc, shared));
    let mut handles = Vec::new();
    for r in &sc.readers {
        let (m, r) = (Arc::clone(&m), r.clone());
        go(&mut handles, move || reader(&m, &p, &r));
    }
    for &c in &sc.fillers {
        let m = Arc::clone(&m);
        go(&mut handles, move || filler(&m, c));
    }
    if let Some((h, g)) = sc.forced_clear {
        let m = Arc::clone(&m);
        go(&mut handles, move || m.counters[h][g].store(0, SeqCst));
    }
    // The hedge: the cell is claimed once; the token sits in the slot both deliverers read (or,
    // without the shared slot, each has a token of its own).
    let slots: Arc<[LoomMutex<Option<ClaimedCell>>; 2]> = Arc::new([
        LoomMutex::new(sc.hedge.and_then(|c| token(&m, c))),
        LoomMutex::new(
            sc.hedge
                .filter(|_| !p.hedge_shares_token)
                .map(|c| ClaimedCell::won(u32::try_from(c).unwrap_or(u32::MAX))),
        ),
    ]);
    if sc.hedge.is_some() {
        for d in 0..2 {
            let (m, slots) = (Arc::clone(&m), Arc::clone(&slots));
            let slot = d * usize::from(!p.hedge_shares_token);
            go(&mut handles, move || {
                let taken = slots[slot].lock().map(|mut s| s.take()).ok().flatten();
                if let Some(t) = taken {
                    m.write_and_publish(&t);
                }
            });
        }
    }
    let evict = sc.evictor.clone().map(|e| {
        let m = Arc::clone(&m);
        thread::spawn(move || evictor(&m, &p, &e))
    });
    for h in handles {
        let _ = h.join();
    }
    let restored = evict
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    for c in restored {
        m.ensure(m.servable(c), Violation::WonCellNotRestored);
    }
    if sc.hedge.is_some() {
        m.ensure(m.writes.load(Relaxed) == 1, Violation::CellWrittenNotOnce);
        m.ensure(
            m.published.load(Relaxed) == 1,
            Violation::CellPublishedNotOnce,
        );
    }
}
