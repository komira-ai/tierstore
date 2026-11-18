//! A read of a large object served from the cells of its pack's entry (section 9): the cells are
//! ensured at `demand` class a window ahead of the read, and each is waited for, read and checked
//! again after the read, in order (the reader protocol of 5.1). The entry is held while the read
//! runs, and one cell is in memory at a time.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream::{self, StreamExt as _};
use tierstore_fetch_api::cell::CellByte;
use tierstore_fetch_api::engine::{PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::Entry;
use tierstore_fetch_api::ids::{ByteRange as PackRange, CellRun, Class, JobId, Rank};

use super::{AHEAD_CELLS, CELL, CELL_SHIFT, LargeObjects, READ_TRIES, bump};
use crate::{Error, ReadBody};

/// The state of one read.
pub(super) struct Cells {
    large: Arc<LargeObjects>,
    spec: PackSpec,
    job: JobId,
    /// The next byte to give.
    next: u64,
    /// The end of the read (exclusive).
    end: u64,
    /// The cells before this one are ensured (or before the read).
    ensured: u32,
    /// The cell past the read's last.
    last: u32,
    /// The entry, held by the front's holder, once a wait has found it open.
    entry: Option<Arc<dyn Entry>>,
}

/// The cell of byte `offset`. An offset inside a pack whose cells the engine counts fits.
fn cell_of(offset: u64) -> u32 {
    u32::try_from(offset >> CELL_SHIFT).unwrap_or(u32::MAX)
}

impl Cells {
    /// The read `start..end` (not empty) of `spec`, for `job`: its first window is ensured now.
    pub fn new(
        large: Arc<LargeObjects>,
        spec: PackSpec,
        (start, end): (u64, u64),
        job: JobId,
    ) -> Self {
        let first = cell_of(start);
        let mut cells = Self {
            large,
            spec,
            job,
            next: start,
            end,
            ensured: first,
            last: cell_of(end - 1).saturating_add(1),
            entry: None,
        };
        cells.ensure_ahead(first);
        cells
    }

    /// The read as a body.
    pub fn stream(self) -> ReadBody {
        stream::unfold(self, |mut cells| async move {
            let piece = cells.piece().await?;
            Some((piece, cells))
        })
        .boxed()
    }

    /// Asks the engine for the cells up to [`AHEAD_CELLS`] past `cell` once the read is within
    /// half a window of the cells it asked for.
    fn ensure_ahead(&mut self, cell: u32) {
        if cell.saturating_add(AHEAD_CELLS / 2) < self.ensured {
            return;
        }
        let from = self.ensured.max(cell);
        let to = cell.saturating_add(AHEAD_CELLS).min(self.last);
        if from < to {
            let run = CellRun {
                first: from,
                count: to - from,
            };
            let engine = &self.large.parts.engine;
            engine.ensure(&self.spec, &[run], Class::Demand, self.job, Rank(0));
            self.ensured = to;
        }
    }

    /// The bytes of `cell` in the pack.
    fn range_of(&self, cell: u32) -> PackRange {
        let offset = u64::from(cell) << CELL_SHIFT;
        PackRange {
            offset,
            len: CELL.min(self.spec.size - offset),
        }
    }

    /// The next piece of the read: the part of one cell it wants, or the error that ends it.
    async fn piece(&mut self) -> Option<Result<Bytes, Error>> {
        if self.next >= self.end {
            return None;
        }
        let cell = cell_of(self.next);
        self.ensure_ahead(cell);
        let range = self.range_of(cell);
        for _ in 0..READ_TRIES {
            let engine = &self.large.parts.engine;
            match engine.wait(&self.spec, range, self.job).await {
                WaitOutcome::Ready => {}
                WaitOutcome::Eio { cell } => {
                    return Some(Err(self.failed(&format!("cell {cell} failed"))));
                }
                WaitOutcome::Lost => return Some(Err(self.failed("the fill is lost"))),
            }
            let Some(bytes) = self.read(cell, range.len).await else {
                continue;
            };
            // Both are inside the cell, whose length is at most 1 MiB.
            let from = (self.next - range.offset) as usize;
            let to = (self.end.min(range.offset + range.len) - range.offset) as usize;
            self.next = range.offset + to as u64;
            return Some(Ok(bytes.slice(from..to)));
        }
        Some(Err(self.failed(&format!(
            "cell {cell} went away {READ_TRIES} times"
        ))))
    }

    /// The bytes of `cell` (`len` of them), if it is `present` before and after they are read;
    /// none if the entry is not open or the cell went away.
    #[expect(
        clippy::expect_used,
        reason = "a blocking task's panic is a bug, passed on"
    )]
    async fn read(&mut self, cell: u32, len: u64) -> Option<Bytes> {
        let entry = match &self.entry {
            Some(entry) => entry.clone(),
            None => {
                let entry = self.large.parts.store.get(&self.spec.id)?;
                entry.hold(self.large.parts.holder);
                self.entry = Some(entry.clone());
                entry
            }
        };
        let reading = tokio::task::spawn_blocking(move || {
            let bytes = entry.read_cell(cell).ok()?;
            let still = entry.state().load(cell).is_some_and(CellByte::servable);
            (still && bytes.len() as u64 == len).then_some(bytes)
        });
        let bytes = reading.await.expect("a read of a cell does not panic")?;
        Some(Bytes::from(bytes))
    }

    /// Ends the read: the book forgets the object, so the next read asks what it is again.
    fn failed(&mut self, what: &str) -> Error {
        self.next = self.end;
        let key = &self.spec.id.key.0;
        self.large.forget(key);
        bump(&self.large.counters.fill_failures);
        Error::Unavailable(format!("{key}: {what}"))
    }
}

impl Drop for Cells {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            entry.release(self.large.parts.holder);
        }
    }
}
