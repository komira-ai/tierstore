//! The `unique -> token` table, registered at read time (5.2).

use std::collections::HashMap;
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
            let mut map = table.lock();
            if map
                .get(&self.unique)
                .is_some_and(|e| Arc::ptr_eq(&e.slot, self))
            {
                map.remove(&self.unique);
            }
        }
        true
    }
}

struct Entry {
    slot: Arc<Slot>,
    device: Arc<dyn ReplyDevice>,
}

#[derive(Default)]
struct Table {
    map: Mutex<HashMap<u64, Entry>>,
}

impl Table {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Entry>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// its reply) is replaced.
    pub fn register(&self, unique: u64, device: Arc<dyn ReplyDevice>) -> Replier {
        let slot = Arc::new(Slot {
            answered: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            waker: AtomicWaker::new(),
            table: Arc::downgrade(&self.table),
            unique,
        });
        self.table.lock().insert(
            unique,
            Entry {
                slot: slot.clone(),
                device: device.clone(),
            },
        );
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
        let found = self
            .table
            .lock()
            .get(&target)
            .map(|e| (e.slot.clone(), e.device.clone()));
        // Winning the take is what makes the request ours to answer; a request whose reply is
        // being written right now is as good as answered.
        let won = found.filter(|(slot, _)| slot.take());
        let Some((slot, device)) = won else {
            let h = encode_out_header(interrupt_unique, Some(Errno::EAGAIN), 0);
            return match interrupt_device.send(&[&h]) {
                Ok(()) => Interrupted::Unknown,
                Err(e) => Interrupted::DeviceError(e),
            };
        };
        slot.cancelled.store(true, Ordering::Release);
        slot.waker.wake();
        let h = encode_out_header(target, Some(Errno::EINTR), 0);
        match device.send(&[&h]) {
            Ok(()) => Interrupted::Cancelled,
            Err(e) => Interrupted::DeviceError(e),
        }
    }

    /// The number of requests registered and not answered.
    #[must_use]
    pub fn registered(&self) -> usize {
        self.table.lock().len()
    }
}
