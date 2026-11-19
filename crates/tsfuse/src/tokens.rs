//! The `unique -> token` table, registered at read time (5.2).

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;

use crate::reply::{Errno, Replier, ReplyDevice, encode_out_header};

/// The state of one registered request: answered or not, cancelled or not.
pub(crate) struct Slot {
    answered: AtomicBool,
    cancelled: AtomicBool,
    waker: AtomicWaker,
    table: Weak<Table>,
    unique: u64,
    /// Set once the session is handed over: a replier dropped then writes nothing.
    handed_over: Arc<AtomicBool>,
}

impl Slot {
    /// The atomic take: whether this caller is the one that answers the request. The winner
    /// removes the entry from the table before it writes (so a reused `unique` is never removed by
    /// a late completion of an older request).
    pub(crate) fn take(self: &Arc<Self>) -> bool {
        if self.answered.swap(true, Ordering::AcqRel) {
            return false;
        }
        if let Some(table) = self.table.upgrade() {
            let mut inner = table.lock();
            if inner
                .map
                .get(&self.unique)
                .is_some_and(|e| Arc::ptr_eq(&e.slot, self))
            {
                inner.map.remove(&self.unique);
            }
        }
        true
    }

    /// Whether the session was handed over ([`Tokens::hand_over`]).
    pub(crate) fn handed_over(&self) -> bool {
        self.handed_over.load(Ordering::Acquire)
    }
}

struct Entry {
    slot: Arc<Slot>,
    device: Arc<dyn ReplyDevice>,
}

/// The most INTERRUPTs held for ring requests not delivered yet (io_uring mode). Past it the
/// oldest is dropped: the kernel's uniques only grow (`fuse_get_unique`), so the oldest is the
/// one most likely to be of a request already answered, which no later request can match.
pub(crate) const MAX_HELD: usize = 1024;

#[derive(Default)]
struct Inner {
    map: HashMap<u64, Entry>,
    /// io_uring mode: the targets of INTERRUPTs that arrived before their request (5.2 item 4).
    held: BTreeSet<u64>,
}

#[derive(Default)]
struct Table {
    inner: Mutex<Inner>,
    /// Whether the session serves on io_uring: an INTERRUPT whose target no device holds is then
    /// held until the target's ring entry delivers it, never answered `EAGAIN`.
    uring: AtomicBool,
}

impl Table {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The cancellation of one request, as the operation that parked it sees it.
#[derive(Clone)]
pub struct CancelToken {
    pub(crate) slot: Arc<Slot>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    /// Whether an INTERRUPT answered the request with `EINTR`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.slot.cancelled.load(Ordering::Acquire)
    }

    /// A future that completes once the request is cancelled.
    pub fn cancelled(&self) -> Cancelled {
        Cancelled {
            slot: self.slot.clone(),
        }
    }
}

/// The future of [`CancelToken::cancelled`].
#[must_use = "a future does nothing unless awaited"]
pub struct Cancelled {
    slot: Arc<Slot>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Register first, then look: a cancel after the look finds the waker.
        self.slot.waker.register(cx.waker());
        if self.slot.cancelled.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// What an INTERRUPT did.
#[derive(Debug)]
pub enum Interrupted {
    /// The request was registered and not answered: `EINTR` was written on the request's own
    /// originating device and its token is cancelled.
    Cancelled,
    /// The request is not registered, or is already answered or being answered (the INTERRUPT
    /// arrived first, or the reply already went):
    /// `EAGAIN` was written on the device given for the INTERRUPT (the session gives every device
    /// in turn, since only the one holding the request takes it).
    Unknown,
    /// The device refused the `EINTR` or `EAGAIN` write.
    DeviceError(std::io::Error),
}

/// The table of registered requests of one session, shared by every reader (every clone).
#[derive(Clone, Default)]
pub struct Tokens {
    table: Arc<Table>,
    handed_over: Arc<AtomicBool>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("registered", &self.registered())
            .finish()
    }
}

impl Tokens {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers request `unique`, read from `device`, before it is dispatched; the replier that
    /// answers it on that device. A `unique` still registered (the kernel reuses an id only after
    /// its reply) is replaced. In io_uring mode, a request whose INTERRUPT arrived first (and was
    /// held) is answered `EINTR` on `device` at once, and its replier is already answered.
    pub fn register(&self, unique: u64, device: Arc<dyn ReplyDevice>) -> Replier {
        let slot = Arc::new(Slot {
            answered: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            waker: AtomicWaker::new(),
            table: Arc::downgrade(&self.table),
            unique,
            handed_over: self.handed_over.clone(),
        });
        let interrupted = {
            let mut inner = self.table.lock();
            let interrupted = inner.held.remove(&unique);
            if !interrupted {
                inner.map.insert(
                    unique,
                    Entry {
                        slot: slot.clone(),
                        device: device.clone(),
                    },
                );
            }
            interrupted
        };
        if interrupted {
            slot.answered.store(true, Ordering::Release);
            slot.cancelled.store(true, Ordering::Release);
            let h = encode_out_header(unique, Some(Errno::EINTR), 0);
            let _ = device.send(&[&h]);
        }
        Replier {
            unique,
            device,
            slot,
        }
    }

    /// Handles `FUSE_INTERRUPT` of `target`, read as request `interrupt_unique`; an `EAGAIN` for
    /// a target not registered is written to `interrupt_device`, which must reach the clone
    /// holding the target (the kernel refuses it on any other with `ENOENT`).
    pub fn interrupt(
        &self,
        target: u64,
        interrupt_unique: u64,
        interrupt_device: &dyn ReplyDevice,
    ) -> Interrupted {
        match self.cancel(target) {
            Some(cancelled) => cancelled,
            None => {
                let h = encode_out_header(interrupt_unique, Some(Errno::EAGAIN), 0);
                match interrupt_device.send(&[&h]) {
                    Ok(()) => Interrupted::Unknown,
                    Err(e) => Interrupted::DeviceError(e),
                }
            }
        }
    }

    /// [`Self::interrupt`] as the readers route an INTERRUPT; `None` when it is held. On
    /// `/dev/fuse` it is the same as [`Self::interrupt`]. In
    /// io_uring mode an `EAGAIN` that no device takes means the target is on no device: it is a
    /// ring request not delivered yet (or one already answered), so the INTERRUPT is held, and
    /// the target, once its ring entry delivers it, is answered `EINTR` as it registers
    /// ([`Self::register`]). The kernel refuses an `EAGAIN` for a ring request (`ENOENT`), so
    /// that answer would lose the INTERRUPT (K11).
    pub(crate) fn interrupt_routed(
        &self,
        target: u64,
        interrupt_unique: u64,
        interrupt_device: &dyn ReplyDevice,
    ) -> Option<Interrupted> {
        let refused = match self.interrupt(target, interrupt_unique, interrupt_device) {
            Interrupted::DeviceError(e) if self.table.uring.load(Ordering::Acquire) => e,
            other => return Some(other),
        };
        let mut inner = self.table.lock();
        if !inner.map.contains_key(&target) {
            inner.held.insert(target);
            if inner.held.len() > MAX_HELD {
                inner.held.pop_first();
            }
            return None;
        }
        // Registered since the look above: cancel it now. If it was answered meanwhile, the
        // INTERRUPT has nothing left to do.
        drop(inner);
        drop(refused);
        Some(self.cancel(target).unwrap_or(Interrupted::Unknown))
    }

    /// Cancels `target` if it is registered and not answered: `EINTR` on its own device.
    /// `None` if it is not registered, or is already answered or being answered.
    fn cancel(&self, target: u64) -> Option<Interrupted> {
        let found = self
            .table
            .lock()
            .map
            .get(&target)
            .map(|e| (e.slot.clone(), e.device.clone()));
        // Winning the take is what makes the request ours to answer; a request whose reply is
        // being written right now is as good as answered.
        let (slot, device) = found.filter(|(slot, _)| slot.take())?;
        slot.cancelled.store(true, Ordering::Release);
        slot.waker.wake();
        let h = encode_out_header(target, Some(Errno::EINTR), 0);
        Some(match device.send(&[&h]) {
            Ok(()) => Interrupted::Cancelled,
            Err(e) => Interrupted::DeviceError(e),
        })
    }

    /// Sets or clears io_uring mode (see [`Self::interrupt_routed`]).
    pub(crate) fn set_uring(&self, on: bool) {
        self.table.uring.store(on, Ordering::Release);
    }

    /// The INTERRUPTs held for ring requests not delivered yet.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.table.lock().held.len()
    }

    /// Marks the session handed over to another server (5.2, item 5): from now on a replier
    /// dropped without being completed writes nothing, and its request stays on its device's
    /// processing queue for the next server's RESEND. A replier completed explicitly still
    /// writes (before that RESEND it answers the request, after it the kernel refuses it).
    pub(crate) fn hand_over(&self) {
        self.handed_over.store(true, Ordering::Release);
    }

    /// The number of requests registered and not answered.
    #[must_use]
    pub fn registered(&self) -> usize {
        self.table.lock().map.len()
    }
}
