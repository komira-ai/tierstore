//! What keeps a placement, and what ends one: leases, attachments, reads, release, and the loop
//! that sweeps what has ended.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::Notify;

use super::super::PinAnswer;
use super::super::evict::joined;
use super::TreeStore;
use super::files::{READY, placement_file, tree_dir};
use super::location::{Location, PlacementId};
use super::state::{Placement, Shared, State};
use crate::TreeDigest;

/// Why a call on a placement was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LifeError {
    /// The store has no such placement: it was never made, it was released, or it ended.
    #[error("no such placement")]
    UnknownPlacement,
    /// The cache does not hold the lock of its directory, or has not read it since it took it.
    #[error("this node does not hold the lock of its cache directory")]
    ReadOnly,
    /// The tree is not on this node.
    #[error("no such tree")]
    UnknownTree,
}

/// What a sweep took away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The placements that ended.
    pub placements: usize,
    /// The trees nothing reached any longer.
    pub trees: usize,
}

/// What the store knows of one placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementInfo {
    /// The tree it places.
    pub tree: TreeDigest,
    /// Where the tree appears.
    pub locations: Vec<Location>,
    /// When it was first made, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// When its lease ends.
    pub expires_ms: u64,
    /// Whether it was released: it appears nowhere and lasts for its holders.
    pub released: bool,
    /// The runs of the mount that hold it.
    pub attachments: usize,
    /// The reads open on it.
    pub reads: usize,
    /// Whether every range of its tree was in the cache and pinned.
    pub ready: bool,
}

/// A read open on a placement: while it is held the placement is not taken away, even if its
/// lease ended or it was released. Dropping it ends the hold.
pub struct ReadHold {
    shared: Arc<Shared>,
    id: PlacementId,
}

impl std::fmt::Debug for ReadHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReadHold({})", self.id)
    }
}

impl Drop for ReadHold {
    fn drop(&mut self) {
        let mut guard = self.shared.lock();
        let state = &mut *guard;
        if let Some(placement) = state.placements.get_mut(&self.id) {
            placement.reads = placement.reads.saturating_sub(1);
        }
        let now = self.shared.now();
        self.shared.purge(state, now);
    }
}

impl Shared {
    /// Takes out of `state` the placements that nothing holds any longer, and then the trees that
    /// nothing reaches; gives how many of each. A store that may not change the directory takes
    /// nothing away.
    pub(super) fn purge(&self, state: &mut State, now: u64) -> SweepReport {
        if !self.writable(state) {
            return SweepReport::default();
        }
        let ended: Vec<PlacementId> = state
            .placements
            .values()
            .filter(|placement| !placement.retained(now))
            .map(|placement| placement.id.clone())
            .collect();
        for id in &ended {
            if let Some(placement) = state.placements.remove(id) {
                self.forget_placement(state, &placement);
            }
        }
        let before = state.trees.len();
        self.drop_unreached(state);
        SweepReport {
            placements: ended.len(),
            trees: before - state.trees.len(),
        }
    }

    /// Takes the locations and the file of `placement` away, if it was not released already.
    fn forget_placement(&self, state: &mut State, placement: &Placement) {
        if placement.released {
            return;
        }
        for location in &placement.locations {
            state.table.remove(&location.key());
        }
        let _ = std::fs::remove_file(placement_file(&self.root, &placement.id));
    }
}

impl TreeStore {
    /// Takes away the placements whose lease ended and that nothing holds, and the trees nothing
    /// reaches then. A store that may not change the directory takes nothing away.
    pub fn sweep(&self) -> SweepReport {
        let mut guard = self.shared.lock();
        let now = self.shared.now();
        self.shared.purge(&mut guard, now)
    }

    /// Removes the locations of the placement: its keys and paths stop appearing at once. It lasts
    /// for the attachments and reads that hold it, and ends with the last of them.
    ///
    /// # Errors
    /// [`LifeError::UnknownPlacement`] for a placement that is not alive, or is released;
    /// [`LifeError::ReadOnly`] if the cache holds no lock.
    pub fn release(&self, id: &PlacementId) -> Result<(), LifeError> {
        let shared = &self.shared;
        let mut guard = shared.lock();
        let state = &mut *guard;
        if !shared.writable(state) {
            return Err(LifeError::ReadOnly);
        }
        let now = shared.now();
        let alive = state.placements.get(id).filter(|p| p.locatable(now));
        let Some(placement) = alive else {
            return Err(LifeError::UnknownPlacement);
        };
        for location in &placement.locations {
            state.table.remove(&location.key());
        }
        let _ = std::fs::remove_file(placement_file(&shared.root, id));
        if let Some(placement) = state.placements.get_mut(id) {
            placement.released = true;
        }
        shared.purge(state, now);
        Ok(())
    }

    /// The placement `id`, held by the run `run` of the mount until [`detach`](Self::detach). A run
    /// that attaches twice holds it once.
    ///
    /// # Errors
    /// [`LifeError::UnknownPlacement`].
    pub fn attach(&self, id: &PlacementId, run: &str) -> Result<(), LifeError> {
        let mut state = self.shared.lock();
        let now = self.shared.now();
        let found = state.placements.get_mut(id).filter(|p| p.locatable(now));
        let placement = found.ok_or(LifeError::UnknownPlacement)?;
        placement.attachments.insert(run.to_owned());
        Ok(())
    }

    /// Ends the hold of the run `run`; false if it held none.
    pub fn detach(&self, id: &PlacementId, run: &str) -> bool {
        let mut guard = self.shared.lock();
        let state = &mut *guard;
        let was = state
            .placements
            .get_mut(id)
            .is_some_and(|placement| placement.attachments.remove(run));
        let now = self.shared.now();
        self.shared.purge(state, now);
        was
    }

    /// Opens a read on the placement: it is held until the guard is dropped.
    ///
    /// # Errors
    /// [`LifeError::UnknownPlacement`].
    pub fn open_read(&self, id: &PlacementId) -> Result<ReadHold, LifeError> {
        let mut state = self.shared.lock();
        let now = self.shared.now();
        let found = state.placements.get_mut(id).filter(|p| p.locatable(now));
        let placement = found.ok_or(LifeError::UnknownPlacement)?;
        placement.reads += 1;
        Ok(ReadHold {
            shared: Arc::clone(&self.shared),
            id: id.clone(),
        })
    }

    /// What the store knows of the placement, if it is alive.
    #[must_use]
    pub fn placement(&self, id: &PlacementId) -> Option<PlacementInfo> {
        let state = self.shared.lock();
        let now = self.shared.now();
        let placement = state.placements.get(id).filter(|p| p.retained(now))?;
        let ready = state
            .trees
            .get(&placement.tree)
            .is_some_and(|info| info.ready.load(Ordering::SeqCst));
        Some(PlacementInfo {
            tree: placement.tree,
            locations: placement.locations.clone(),
            created_ms: placement.created_ms,
            expires_ms: placement.expires_ms,
            released: placement.released,
            attachments: placement.attachments.len(),
            reads: placement.reads,
            ready,
        })
    }

    /// The placements and the trees the store holds.
    #[must_use]
    pub fn sizes(&self) -> (usize, usize) {
        let state = self.shared.lock();
        (state.placements.len(), state.trees.len())
    }

    /// Pins the packs of the tree for it, as far as the cache holds them and the limit allows: a
    /// tree whose files were fetched after it was made asks again.
    ///
    /// # Errors
    /// [`LifeError::UnknownTree`].
    pub fn pin(&self, tree: &TreeDigest) -> Result<PinAnswer, LifeError> {
        let state = self.shared.lock();
        let info = state.trees.get(tree).ok_or(LifeError::UnknownTree)?;
        Ok(self.shared.pin(info))
    }

    /// Says that every range of the tree is in the cache: a start then drops the tree if the
    /// cache loses one. Pins what the cache holds, as [`pin`](Self::pin) does.
    ///
    /// # Errors
    /// [`LifeError::UnknownTree`], [`LifeError::ReadOnly`] if the cache holds no lock.
    pub fn mark_ready(&self, tree: &TreeDigest) -> Result<PinAnswer, LifeError> {
        let state = self.shared.lock();
        if !self.shared.writable(&state) {
            return Err(LifeError::ReadOnly);
        }
        let info = state.trees.get(tree).ok_or(LifeError::UnknownTree)?;
        if !info.ready.swap(true, Ordering::SeqCst) {
            let marker = tree_dir(&self.shared.root, tree).join(READY);
            if let Err(why) = std::fs::write(marker, b"") {
                tracing::warn!("tierstore: trees: {tree} was not marked ready: {why}");
            }
        }
        Ok(self.shared.pin(info))
    }

    /// One step of the loop: reads the directory again if the cache took the lock since the store
    /// read it, and otherwise sweeps.
    pub(super) fn step(&self) {
        if !self.shared.config.enabled {
            return;
        }
        let (owned, generation) = {
            let state = self.shared.lock();
            (state.owned, state.generation)
        };
        let cache = &self.shared.cache;
        if cache.inner.read_write() && !(owned && generation == cache.inner.generation()) {
            if let Err(why) = self.load() {
                tracing::warn!("tierstore: trees: the directory was not read: {why}");
            }
        } else {
            let _ = self.sweep();
        }
    }

    /// Runs a [`step`](Self::step) every `interval` until `halt` is notified. A store that is off
    /// does nothing.
    pub async fn maintain(&self, interval: Duration, halt: Arc<Notify>) {
        loop {
            tokio::select! {
                biased;
                () = halt.notified() => break,
                () = tokio::time::sleep(interval) => {}
            }
            let store = self.clone();
            let stepping = tokio::task::spawn_blocking(move || store.step());
            let _ = joined(stepping.await, "a step of the tree store");
        }
    }
}
