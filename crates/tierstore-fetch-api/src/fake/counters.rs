//! A holder's counters in memory: what the server maps of a mount's counter memfd.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};

use super::lock;
use crate::entry::{EntryError, HolderCounters};

/// The most bytes [`MemCounters::new`] lets the mapping grow to.
pub const DEFAULT_LIMIT: u64 = 1 << 26;

/// The counters of one holder: a growable run of `u16` behind a lock. `ensure_len` grows it, as
/// the server's remap does, and counts the growths ([`MemCounters::remaps`]); a test plays the
/// mount with [`MemCounters::set`] and [`MemCounters::add`].
#[derive(Debug)]
pub struct MemCounters {
    counters: Mutex<Vec<AtomicU16>>,
    remaps: Mutex<u32>,
    limit: u64,
}

impl MemCounters {
    /// Counters mapped over `len` bytes (rounded up to a whole `u16`), growing to at most
    /// [`DEFAULT_LIMIT`].
    #[must_use]
    pub fn new(len: u64) -> Self {
        Self::with_limit(len, DEFAULT_LIMIT)
    }

    /// Counters mapped over `len` bytes that refuse to grow past `limit` bytes with
    /// [`EntryError::CountersMap`], as a remap that fails does.
    #[must_use]
    pub fn with_limit(len: u64, limit: u64) -> Self {
        let counters = Self {
            counters: Mutex::new(Vec::new()),
            remaps: Mutex::new(0),
            limit,
        };
        grow(&mut lock(&counters.counters), len);
        counters
    }

    /// The number of times `ensure_len` grew the mapping.
    #[must_use]
    pub fn remaps(&self) -> u32 {
        *lock(&self.remaps)
    }

    /// Stores `value` at byte `offset` (the mount's side); whether `offset` is a counter inside
    /// the mapping.
    pub fn set(&self, offset: u64, value: u16) -> bool {
        self.with(offset, |c| c.store(value, Ordering::SeqCst))
    }

    /// Adds `delta` (wrapping) to the counter at byte `offset`; whether it is inside the mapping.
    pub fn add(&self, offset: u64, delta: u16) -> bool {
        self.with(offset, |c| {
            c.fetch_add(delta, Ordering::SeqCst);
        })
    }

    fn with(&self, offset: u64, f: impl FnOnce(&AtomicU16)) -> bool {
        index(offset)
            .and_then(|i| lock(&self.counters).get(i).map(f))
            .is_some()
    }
}

/// The index of the counter at an even byte `offset`.
fn index(offset: u64) -> Option<usize> {
    offset.is_multiple_of(2).then_some((offset / 2) as usize)
}

/// Grows `counters` to cover `len` bytes.
fn grow(counters: &mut Vec<AtomicU16>, len: u64) {
    let want = len.div_ceil(2) as usize;
    if want > counters.len() {
        counters.resize_with(want, || AtomicU16::new(0));
    }
}

impl HolderCounters for MemCounters {
    fn len(&self) -> u64 {
        2 * lock(&self.counters).len() as u64
    }

    fn ensure_len(&self, len: u64) -> Result<(), EntryError> {
        if len > self.limit {
            return Err(EntryError::CountersMap {
                len,
                source: std::io::Error::from(std::io::ErrorKind::OutOfMemory),
            });
        }
        let mut counters = lock(&self.counters);
        if len > 2 * counters.len() as u64 {
            grow(&mut counters, len);
            *lock(&self.remaps) += 1;
        }
        Ok(())
    }

    fn load(&self, offset: u64) -> Option<u16> {
        index(offset).and_then(|i| {
            lock(&self.counters)
                .get(i)
                .map(|c| c.load(Ordering::SeqCst))
        })
    }
}
