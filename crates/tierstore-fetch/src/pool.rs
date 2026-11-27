//! The pool of 1 MiB cell buffers parts are received into (4.8): in-flight memory is bounded by
//! the pool, `ceiling + 32 MiB` by default ([`crate::config::buffer_bytes_for`]).
//!
//! An attempt takes a buffer before it reads the bytes of a cell, copies the body's bytes into
//! it, and hands the full cell, with the buffer's slot, to its op; the slot is given back when
//! the cell's publish returns, or when its bytes are dropped (a hedge's loser, a cell that failed
//! verification, a part cut short). So the cells the engine holds, received, hashing, waiting on
//! the dirty-byte cap or being published, are at most the pool's buffers, whatever the number of
//! parts in flight. The upstream's chunk a cell is copied from is the upstream's memory, released
//! once its bytes are copied.
//!
//! A buffer whose cell was published comes back to the pool's free list and is used again; the
//! list keeps at most the pool's size, so the pool allocates only up to its peak use.

use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::{Bytes, BytesMut};
use tierstore_fetch_api::ids::Class;

use crate::gate::{Gate, Level, Observer, Room};
use crate::plan::CELL_BYTES;

/// One buffer's slot of the pool: held by a cell from its first byte until its bytes are
/// dropped.
pub(crate) type Slot = Option<Room>;

/// A buffer taken from the pool, and its slot.
pub(crate) struct CellBuf {
    pub buf: BytesMut,
    pub slot: Slot,
}

/// The pool of cell buffers.
pub(crate) struct CellPool {
    slots: Gate,
    cells: usize,
    free: Mutex<Vec<BytesMut>>,
}

impl CellPool {
    /// A pool of `bytes` bytes of cells (at least one buffer).
    pub(crate) fn new(bytes: u64, observe: Observer) -> Self {
        let cells = (bytes / CELL_BYTES).max(1);
        Self {
            slots: Gate::new(cells, observe),
            cells: usize::try_from(cells).unwrap_or(usize::MAX),
            free: Mutex::new(Vec::new()),
        }
    }

    fn free(&self) -> MutexGuard<'_, Vec<BytesMut>> {
        self.free.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The pool's level now, in buffers.
    pub(crate) fn level(&self) -> Level {
        self.slots.level()
    }

    /// A buffer for a cell of a part of `class`: waits while every buffer is in use.
    pub(crate) async fn take(&self, class: Class) -> CellBuf {
        let (slot, _) = self.slots.take(class, 1).await;
        let reused = self.free().pop();
        // A cell is at most 1 MiB.
        let buf = reused.unwrap_or_else(|| BytesMut::with_capacity(CELL_BYTES as usize));
        CellBuf { buf, slot }
    }

    /// Takes back the buffer of a cell's bytes, when nothing else holds them.
    pub(crate) fn recycle(&self, bytes: Bytes) {
        let whole = bytes
            .try_into_mut()
            .ok()
            .filter(|b| b.capacity() as u64 >= CELL_BYTES);
        let mut free = self.free();
        let room = free.len() < self.cells;
        free.extend(whole.filter(|_| room).map(|mut b| {
            b.clear();
            b
        }));
    }
}
