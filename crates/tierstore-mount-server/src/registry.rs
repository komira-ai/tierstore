//! The server's holders and the packs they have open.
//!
//! A holder is a mount process (5.1): it is registered at its first handshake with the counter
//! memfd and the lifeline it sent, and it ends on the lifeline's hangup or on `release`, never
//! because a connection closed. Every connection that handshakes with the same holder id and the
//! same two descriptors serves the same holder. A handle names one pack a holder opened; handles
//! are numbered per server and are never reused within it.
//!
//! Lock order: a holder's state, then the handle table. Neither is held across I/O or an
//! `await`.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rustix::fs::{FileType, SealFlags, fcntl_get_seals, fstat};
use tierstore_fetch_api::engine::PackSpec;
use tierstore_fetch_api::entry::{Entry, EntryError, HolderCounters};
use tierstore_fetch_api::ids::{HolderId, TreeRef};
use tierstore_fetch_api::page::{CounterRegion, F_SEAL_SHRINK};
use tierstore_fetch_api::wire::PackHandle;

/// The seals (`F_GET_SEALS` bits, [`tierstore_fetch_api::page`]) a mount's counter memfd must
/// carry before it is sent at handshake: `F_SEAL_SHRINK`. The server maps the memfd; a mount that
/// shrank it under that mapping would make the server's next read of a counter past the new end
/// a `SIGBUS`. Growing stays allowed (5.1: the memfd grows as the mount opens packs).
pub const COUNTER_MEMFD_SEALS: u32 = F_SEAL_SHRINK;

/// Maps a holder's counter memfd for the server (the Linux entry store, T1, maps it read-only and
/// remaps it on [`HolderCounters::ensure_len`]).
///
/// [`CounterMapper::map`] does blocking work (`mmap`); the server calls it on a blocking thread.
pub trait CounterMapper: Send + Sync {
    /// The server's view of `holder`'s counter `memfd`, which carries [`COUNTER_MEMFD_SEALS`].
    ///
    /// # Errors
    /// The memfd cannot be mapped ([`EntryError::CountersMap`]).
    fn map(&self, holder: HolderId, memfd: OwnedFd) -> Result<Arc<dyn HolderCounters>, EntryError>;
}

/// Locks `m`; a thread that panicked while holding it has not left the maps half-changed (every
/// change under these locks is one insert or remove), so the state is used as it is.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Which file a descriptor is: its device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inode {
    dev: u64,
    ino: u64,
}

/// The file a descriptor is, and its type.
fn identify(fd: BorrowedFd<'_>) -> io::Result<(Inode, FileType)> {
    let st = fstat(fd)?;
    Ok((
        Inode {
            dev: st.st_dev,
            ino: st.st_ino,
        },
        FileType::from_raw_mode(st.st_mode),
    ))
}

/// Why the descriptors of a handshake are refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BadDescriptors {
    /// Fewer than two descriptors (the framing lets no such handshake through).
    #[error("a handshake without its two descriptors")]
    Missing,
    /// The counter descriptor is not a memfd (`F_GET_SEALS` fails).
    #[error("the counter descriptor is not a memfd: {0}")]
    NotMemfd(io::Error),
    /// The counter memfd is not sealed against shrinking.
    #[error("the counter memfd is not sealed against shrinking (seals {0:#x})")]
    Unsealed(u32),
    /// The lifeline is not a socket, or cannot be examined.
    #[error("the lifeline is not a socket")]
    LifelineNotSocket,
}

/// The identities of a handshake's counter memfd and lifeline, once both are what the protocol
/// asks for.
pub(crate) fn check_descriptors(
    counters: &OwnedFd,
    lifeline: &OwnedFd,
) -> Result<(Inode, Inode), BadDescriptors> {
    let seals = fcntl_get_seals(counters).map_err(|e| BadDescriptors::NotMemfd(e.into()))?;
    if !seals.contains(SealFlags::from_bits_retain(COUNTER_MEMFD_SEALS)) {
        return Err(BadDescriptors::Unsealed(seals.bits()));
    }
    // A memfd always has an inode: fstat of one does not fail.
    let (counters_inode, _) = identify(counters.as_fd()).map_err(BadDescriptors::NotMemfd)?;
    match identify(lifeline.as_fd()) {
        Ok((inode, FileType::Socket)) => Ok((counters_inode, inode)),
        _ => Err(BadDescriptors::LifelineNotSocket),
    }
}

/// A registered holder.
#[derive(Debug)]
pub(crate) struct Holder {
    pub(crate) id: HolderId,
    counters: Inode,
    lifeline: Inode,
    state: Mutex<HolderState>,
}

#[derive(Debug, Default)]
struct HolderState {
    ended: bool,
    opened: BTreeMap<(TreeRef, u32), PackHandle>,
}

impl Holder {
    pub(crate) fn new(id: HolderId, counters: Inode, lifeline: Inode) -> Self {
        Self {
            id,
            counters,
            lifeline,
            state: Mutex::new(HolderState::default()),
        }
    }

    /// Whether a handshake that sent descriptors of these identities is this holder's.
    pub(crate) fn same_descriptors(&self, counters: Inode, lifeline: Inode) -> bool {
        self.counters == counters && self.lifeline == lifeline
    }

    pub(crate) fn is_ended(&self) -> bool {
        lock(&self.state).ended
    }
}

/// A pack a holder opened.
pub(crate) struct OpenPack {
    pub(crate) holder: HolderId,
    pub(crate) spec: PackSpec,
    pub(crate) entry: Arc<dyn Entry>,
    pub(crate) region: CounterRegion,
}

/// Every live holder and every open pack.
#[derive(Default)]
pub(crate) struct Registry {
    holders: Mutex<BTreeMap<HolderId, Arc<Holder>>>,
    handles: Mutex<BTreeMap<PackHandle, Arc<OpenPack>>>,
    last_handle: AtomicU64,
}

impl Registry {
    /// The live holder of `id`.
    pub(crate) fn live(&self, id: HolderId) -> Option<Arc<Holder>> {
        lock(&self.holders).get(&id).cloned()
    }

    /// Registers `holder` (the caller has checked that none of its id is live).
    pub(crate) fn insert(&self, holder: Arc<Holder>) {
        lock(&self.holders).insert(holder.id, holder);
    }

    /// Ends `holder`: no request is served for it again, its handles are forgotten and it is no
    /// longer live. Whether it was not already ended.
    pub(crate) fn end(&self, holder: &Arc<Holder>) -> bool {
        let handles = {
            let mut state = lock(&holder.state);
            if state.ended {
                return false;
            }
            state.ended = true;
            std::mem::take(&mut state.opened)
        };
        {
            let mut table = lock(&self.handles);
            for handle in handles.values() {
                table.remove(handle);
            }
        }
        // A holder id is registered again only after its holder ended (both under the server's
        // holder lock), so the live holder of this id is `holder`.
        lock(&self.holders).remove(&holder.id);
        true
    }

    /// The pack `holder` already opened as `key`.
    pub(crate) fn known(
        &self,
        holder: &Holder,
        key: &(TreeRef, u32),
    ) -> Option<(PackHandle, Arc<OpenPack>)> {
        let state = lock(&holder.state);
        let handle = *state.opened.get(key)?;
        let open = lock(&self.handles).get(&handle).cloned()?;
        Some((handle, open))
    }

    /// Records that `holder` opened `open` as `key`, under a new handle; if a request that raced
    /// this one recorded it first, that one's. `None` when the holder has ended.
    pub(crate) fn add(
        &self,
        holder: &Holder,
        key: (TreeRef, u32),
        open: OpenPack,
    ) -> Option<(PackHandle, Arc<OpenPack>)> {
        let mut state = lock(&holder.state);
        if state.ended {
            return None;
        }
        let mut table = lock(&self.handles);
        if let Some(handle) = state.opened.get(&key) {
            return table.get(handle).map(|o| (*handle, o.clone()));
        }
        let handle = PackHandle(self.last_handle.fetch_add(1, Ordering::Relaxed) + 1);
        let open = Arc::new(open);
        table.insert(handle, open.clone());
        state.opened.insert(key, handle);
        Some((handle, open))
    }

    /// The open pack of `handle`, if `holder` opened it.
    pub(crate) fn open(&self, holder: HolderId, handle: PackHandle) -> Option<Arc<OpenPack>> {
        lock(&self.handles)
            .get(&handle)
            .filter(|o| o.holder == holder)
            .cloned()
    }
}
