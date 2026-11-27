//! An entry store whose publishes the test holds on a gate, or fails: the in-memory store of
//! tierstore-fetch-api behind a wrapper, so a test sees how many publishes are in flight and in
//! which order they started, and lets them return when it chooses. A held publish blocks its
//! thread of the blocking pool, as a slow `pwrite` does; nothing here reads a clock.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntryInfo, EntrySpec,
    EntryStore, EvictStep, HolderCounters, PunchReport, Settle, StateView,
};
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey};
use tierstore_fetch_api::page::CounterRegion;
use tokio::sync::watch;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the publishes did so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Publishes {
    /// Every publish that started, in order: its pack and cell.
    pub started: Vec<(PackKey, u32)>,
    /// Publishes started and not returned.
    pub in_flight: u32,
    /// Publishes returned.
    pub returned: u32,
}

#[derive(Default)]
struct GateState {
    publishes: Publishes,
    /// Every held publish may return.
    open: bool,
    /// Held publishes that may return before the gate opens.
    permits: u32,
    /// Packs whose publishes are not held.
    passing: BTreeSet<PackKey>,
    /// Cells whose publish fails at once.
    failing: BTreeSet<(PackKey, u32)>,
}

/// The gate the publishes of a [`GatedStore`] wait on.
pub struct PublishGate {
    state: Mutex<GateState>,
    cv: Condvar,
    changed: watch::Sender<u64>,
}

impl PublishGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            cv: Condvar::new(),
            changed: watch::Sender::new(0),
        }
    }

    fn changed(&self) {
        self.cv.notify_all();
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Lets `n` more held publishes return.
    pub fn release(&self, n: u32) {
        lock(&self.state).permits += n;
        self.changed();
    }

    /// Lets every held publish return, now and later.
    pub fn open(&self) {
        lock(&self.state).open = true;
        self.changed();
    }

    /// The publishes of `key` are not held.
    pub fn pass(&self, key: &PackKey) {
        lock(&self.state).passing.insert(key.clone());
    }

    /// The publish of `cell` of `key` fails at once (its claim is given up first, as the
    /// in-memory store does for a publish of the wrong length).
    pub fn fail(&self, key: &PackKey, cell: u32) {
        lock(&self.state).failing.insert((key.clone(), cell));
    }

    /// What the publishes did so far.
    #[must_use]
    pub fn publishes(&self) -> Publishes {
        lock(&self.state).publishes.clone()
    }

    /// Waits until `f` holds of the publishes.
    pub async fn until(&self, f: impl Fn(&Publishes) -> bool) -> Publishes {
        let mut rx = self.changed.subscribe();
        loop {
            rx.borrow_and_update();
            let p = self.publishes();
            if f(&p) {
                return p;
            }
            // The sender lives in `self`: `changed` never fails here.
            let _ = rx.changed().await;
        }
    }

    fn publish(
        &self,
        inner: &dyn Entry,
        cell: ClaimedCell,
        bytes: &[u8],
    ) -> Result<(), EntryError> {
        let key = inner.pack().key.clone();
        let c = cell.cell();
        let (fails, held) = {
            let mut st = lock(&self.state);
            st.publishes.started.push((key.clone(), c));
            st.publishes.in_flight += 1;
            (
                st.failing.contains(&(key.clone(), c)),
                !st.passing.contains(&key),
            )
        };
        self.changed();
        let result = if fails {
            inner.abandon(cell);
            Err(EntryError::NotClaimed {
                pack: inner.pack().clone(),
                cell: c,
            })
        } else {
            let mut st = lock(&self.state);
            while held && !st.open && st.permits == 0 {
                st = self.cv.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
            if held && !st.open {
                st.permits -= 1;
            }
            drop(st);
            inner.publish(cell, bytes)
        };
        {
            let mut st = lock(&self.state);
            st.publishes.in_flight -= 1;
            st.publishes.returned += 1;
        }
        self.changed();
        result
    }
}

/// The in-memory store with its publishes on a [`PublishGate`].
pub struct GatedStore {
    inner: Arc<MemEntryStore>,
    pub gate: Arc<PublishGate>,
}

impl GatedStore {
    /// `inner` with every publish held until the gate lets it return.
    #[must_use]
    pub fn new(inner: Arc<MemEntryStore>) -> Self {
        Self {
            inner,
            gate: Arc::new(PublishGate::new()),
        }
    }

    fn wrap(&self, entry: Arc<dyn Entry>) -> Arc<dyn Entry> {
        Arc::new(GatedEntry {
            inner: entry,
            gate: self.gate.clone(),
        })
    }
}

impl EntryStore for GatedStore {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        self.inner.open(spec).map(|e| self.wrap(e))
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        self.inner.get(pack).map(|e| self.wrap(e))
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        self.inner.remove(pack)
    }

    fn packs(&self) -> Vec<PackId> {
        self.inner.packs()
    }

    fn entries(&self) -> Vec<EntryInfo> {
        self.inner.entries()
    }

    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        self.inner.register_holder(holder, counters)
    }

    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        self.inner.attach(pack, holder, region)
    }

    fn drop_holder(&self, holder: HolderId) -> u32 {
        self.inner.drop_holder(holder)
    }
}

struct GatedEntry {
    inner: Arc<dyn Entry>,
    gate: Arc<PublishGate>,
}

impl Entry for GatedEntry {
    fn pack(&self) -> &PackId {
        self.inner.pack()
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn state(&self) -> &dyn StateView {
        self.inner.state()
    }

    fn claim(&self, run: CellRun) -> Vec<ClaimedCell> {
        self.inner.claim(run)
    }

    fn abandon(&self, cell: ClaimedCell) {
        self.inner.abandon(cell);
    }

    fn publish(&self, cell: ClaimedCell, bytes: &[u8]) -> Result<(), EntryError> {
        self.gate.publish(&*self.inner, cell, bytes)
    }

    fn mark_bad(&self, cell: ClaimedCell) {
        self.inner.mark_bad(cell);
    }

    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool {
        self.inner.evict_cell_cas(cell, step)
    }

    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        self.inner.punch(runs)
    }

    fn settle(&self, cell: u32, how: Settle) -> bool {
        self.inner.settle(cell, how)
    }

    fn group_readers(&self, group: u32) -> u64 {
        self.inner.group_readers(group)
    }

    fn counts(&self) -> CellCounts {
        self.inner.counts()
    }

    fn last_use(&self) -> u64 {
        self.inner.last_use()
    }

    fn touch(&self, now_ms: u64, group: u32) {
        self.inner.touch(now_ms, group);
    }

    fn touch_epoch(&self, group: u32) -> Option<u32> {
        self.inner.touch_epoch(group)
    }

    fn read_cell(&self, cell: u32) -> Result<Vec<u8>, EntryError> {
        self.inner.read_cell(cell)
    }

    fn fds(&self) -> Option<EntryFds<'_>> {
        self.inner.fds()
    }

    fn hold(&self, holder: HolderId) {
        self.inner.hold(holder);
    }

    fn release(&self, holder: HolderId) -> bool {
        self.inner.release(holder)
    }

    fn holders(&self) -> u32 {
        self.inner.holders()
    }

    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        self.inner.commit_batch()
    }
}
