//! The entry store of the read path, as the engine, the evictor and the S3 front see it: the
//! in-process store of `tierstore-entry` with every publish told to the node's one committer
//! (section 8, "Batching"), which commits each dirty entry once a second or as soon as 32 MiB
//! are pending.
//!
//! [`Committed`] wraps the store and each entry it gives. A call is passed to the entry it wraps
//! and changes nothing; a publish that succeeds is also told to the committer with its bytes, and
//! when the committer says the pending bytes are reached, the committer's loop is woken
//! ([`Wake`]) to commit at once.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntryInfo, EntrySpec,
    EntryStore, EvictStep, HolderCounters, PunchReport, Settle, StateView,
};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_journal::Committer;

/// What wakes the loops of the read path: a round of the committer that is due now, and the stop.
#[derive(Debug, Default)]
pub(crate) struct Wake {
    state: Mutex<Woken>,
    changed: Condvar,
}

/// What [`Wake`] has been told.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Woken {
    /// A round of the committer is due now.
    pub commit: bool,
    /// The read path is stopping.
    pub stop: bool,
}

impl Wake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Woken> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A round of the committer is due now.
    pub(crate) fn commit(&self) {
        self.lock().commit = true;
        self.changed.notify_all();
    }

    /// The read path is stopping.
    pub(crate) fn stop(&self) {
        self.lock().stop = true;
        self.changed.notify_all();
    }

    /// Waits until a round is due or the stop is told, or `wait` has passed, and gives what was
    /// told, taking the round (the stop stays told).
    pub(crate) fn wait(&self, wait: std::time::Duration) -> Woken {
        let state = self.lock();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, wait, |s| !s.commit && !s.stop)
            .unwrap_or_else(PoisonError::into_inner);
        let woken = *state;
        state.commit = false;
        woken
    }
}

/// The store, or one of its entries, with the publishes told to the committer.
pub(crate) struct Committed<T: ?Sized> {
    inner: Arc<T>,
    committer: Arc<Committer>,
    wake: Arc<Wake>,
}

impl<T: ?Sized> Committed<T> {
    /// `inner`, whose publishes `committer` is told of, waking `wake` when a round is due.
    pub(crate) fn new(inner: Arc<T>, committer: Arc<Committer>, wake: Arc<Wake>) -> Self {
        Self {
            inner,
            committer,
            wake,
        }
    }

    /// `entry`, given by the store, with its publishes told as the store's are.
    fn entry(&self, entry: Arc<dyn Entry>) -> Arc<dyn Entry> {
        Arc::new(Committed::new(
            entry,
            self.committer.clone(),
            self.wake.clone(),
        ))
    }
}

impl<T: EntryStore + ?Sized> EntryStore for Committed<T> {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        Ok(self.entry(self.inner.open(spec)?))
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        self.inner.get(pack).map(|entry| self.entry(entry))
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

impl Entry for Committed<dyn Entry> {
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
        self.inner.publish(cell, bytes)?;
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.committer.published(&self.inner, len) {
            self.wake.commit();
        }
        Ok(())
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
