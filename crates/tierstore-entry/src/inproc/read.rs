//! The read path of the in-process front (5.6: the NFS `read` handler): the reader protocol of
//! 5.1 against an [`InProcEntry`]'s own counters, and a read that waits on the engine for cells
//! that are not ready.

use std::io;
use std::os::unix::fs::FileExt;

use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::engine::{Engine, PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::{Entry, EntryError, ReaderCounters, StateView};
use tierstore_fetch_api::ids::{ByteRange, CellRun, JobId, PackId};
use tierstore_fetch_api::page::{CELL_SHIFT, groups_of};

use super::InProcEntry;
use super::entry::io_err;

/// What one [`InProcEntry::read_at`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The bytes were read, and every cell they cover was still `present` after the read.
    Read,
    /// A covered cell was not `present` before or after the read; the bytes read, if any, are not
    /// to be served. The reader waits for the cells ([`Engine::wait`]) and reads again.
    NotReady {
        /// The first covered cell that was not `present`.
        cell: u32,
        /// Whether that cell is `bad`: EIO for the read, not a wait (5.1 reader step 2).
        bad: bool,
    },
}

/// Why [`read`] gave no bytes.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The entry refused the read (a range outside the pack) or its data file failed.
    #[error(transparent)]
    Entry(#[from] EntryError),
    /// A covered cell is `bad`, or failed on every copy (4.6): EIO.
    #[error("{pack:?}: cell {cell} failed")]
    Eio {
        /// The pack.
        pack: PackId,
        /// The first failed cell.
        cell: u32,
    },
    /// The fill is lost (4.6): EIO, and the front marks the tree lost.
    #[error("{pack:?}: lost")]
    Lost {
        /// The pack.
        pack: PackId,
    },
    /// The engine answered `Ready` [`MAX_ROUNDS`] times and each read after it found a covered
    /// cell gone again (evicted between the wait and the read): the read gives up rather than
    /// loop without end.
    #[error("{pack:?}: {rounds} waits answered ready and the cells were gone each time")]
    Unsettled {
        /// The pack.
        pack: PackId,
        /// The waits made.
        rounds: u32,
    },
}

/// The waits [`read`] makes before it gives up with [`ReadError::Unsettled`]. A wait answers
/// `Ready` only when every covered cell is `present`, so a read after it finds a cell gone only
/// when the evictor took the cell in between; the evictor takes the groups touched longest ago
/// (7), and a read touches its groups, so more than a few rounds is a defect, not load.
pub const MAX_ROUNDS: u32 = 8;

impl InProcEntry {
    /// Reads `buf.len()` bytes of the pack at `offset` under the reader protocol of 5.1: raises
    /// the reader counter of every 16-cell group covering the range (step 1), loads every covered
    /// cell (step 2), and only if each is `present` reads the bytes with `pread` (step 3), then
    /// loads every covered cell again (step 4: the post-read recheck), and lowers the counters
    /// (step 5). Every load and counter update is `SeqCst`, so the evictor, which CASes a cell to
    /// `evicting` before it sums the counters, either sees this reader or is seen by it.
    ///
    /// An empty `buf` reads nothing ([`ReadOutcome::Read`]); a range that leaves the pack is
    /// [`EntryError::CellOutOfRange`] naming the cell past the pack's last; a failed `pread` is
    /// [`EntryError::Io`]. The `pread` blocks: call it where a blocking read is allowed (5.4: ready
    /// reads run inline on the reader's own thread).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<ReadOutcome, EntryError> {
        self.read_with(offset, buf, |data, buf| data.read_exact_at(buf, offset))
    }

    /// [`InProcEntry::read_at`] with the `pread` (step 3) given, so a test can change the cells
    /// while a read is in flight.
    pub(crate) fn read_with(
        &self,
        offset: u64,
        buf: &mut [u8],
        pread: impl FnOnce(&std::fs::File, &mut [u8]) -> io::Result<()>,
    ) -> Result<ReadOutcome, EntryError> {
        let Some(run) = self.cells_of(offset, buf.len() as u64)? else {
            return Ok(ReadOutcome::Read);
        };
        // A non-empty run inside the pack has groups.
        let groups = groups_of(run).unwrap_or(CellRun { first: 0, count: 0 });
        groups.cells().for_each(|g| self.enter(g));
        let outcome = match self.unready(run) {
            Some(not_ready) => Ok(not_ready),
            None => pread(self.data(), buf)
                .map_err(io_err(self.pack(), "pread"))
                .map(|()| self.unready(run).unwrap_or(ReadOutcome::Read)),
        };
        groups.cells().for_each(|g| self.leave(g));
        outcome
    }

    /// The cells covering `len` bytes at `offset`: `None` for no byte, an error for a range that
    /// leaves the pack.
    fn cells_of(&self, offset: u64, len: u64) -> Result<Option<CellRun>, EntryError> {
        if len == 0 {
            return Ok(None);
        }
        let end = offset.checked_add(len).filter(|end| *end <= self.size());
        let Some(end) = end else {
            let cells = self.cell_count();
            return Err(EntryError::CellOutOfRange {
                pack: self.pack().clone(),
                cell: cells,
                cells,
            });
        };
        // Both ends are inside a pack of at most u32::MAX cells.
        let first = (offset >> CELL_SHIFT) as u32;
        let last = ((end - 1) >> CELL_SHIFT) as u32;
        Ok(Some(CellRun {
            first,
            count: last - first + 1,
        }))
    }

    /// The first cell of `run` that is not `present`, as the outcome a reader reports.
    fn unready(&self, run: CellRun) -> Option<ReadOutcome> {
        run.cells().find_map(|cell| {
            let byte = self.load(cell).unwrap_or(CellByte(0));
            (!byte.servable()).then(|| ReadOutcome::NotReady {
                cell,
                bad: byte.state() == Some(CellState::Bad),
            })
        })
    }
}

/// Reads `len` bytes of the pack `spec` at `offset` from `entry`, the read of the NFS front
/// (5.6): [`InProcEntry::read_at`], and while a covered cell is not ready, [`Engine::wait`] on the
/// range (a wait on cells nobody ensured is a demand of them) and read again. On a read, every
/// covered group is touched at `clock`'s now (7: the evictor takes the groups touched longest
/// ago). A `bad` cell, or a wait that ends `Eio`, is [`ReadError::Eio`]; a lost fill
/// [`ReadError::Lost`]; [`MAX_ROUNDS`] waits each followed by a cell gone again,
/// [`ReadError::Unsettled`].
///
/// Cancel-safe: dropping the future between waits leaves no counter raised (each
/// [`InProcEntry::read_at`] lowers its own) and cancels nothing of the fill. The `pread`s block
/// the task that polls it.
pub async fn read(
    entry: &InProcEntry,
    engine: &dyn Engine,
    spec: &PackSpec,
    clock: &dyn Clock,
    offset: u64,
    len: usize,
    job: JobId,
) -> Result<Vec<u8>, ReadError> {
    let pack = entry.pack();
    let mut buf = vec![0u8; len];
    for _ in 0..MAX_ROUNDS {
        match entry.read_at(offset, &mut buf)? {
            ReadOutcome::Read => {
                if let Some(run) = entry.cells_of(offset, len as u64)?.and_then(groups_of) {
                    let now = clock.now_ms();
                    run.cells().for_each(|g| entry.touch(now, g));
                }
                return Ok(buf);
            }
            ReadOutcome::NotReady { cell, bad: true } => {
                return Err(ReadError::Eio {
                    pack: pack.clone(),
                    cell,
                });
            }
            ReadOutcome::NotReady { bad: false, .. } => {}
        }
        let range = ByteRange {
            offset,
            len: len as u64,
        };
        match engine.wait(spec, range, job).await {
            WaitOutcome::Ready => {}
            WaitOutcome::Eio { cell } => {
                return Err(ReadError::Eio {
                    pack: pack.clone(),
                    cell,
                });
            }
            WaitOutcome::Lost => return Err(ReadError::Lost { pack: pack.clone() }),
        }
    }
    Err(ReadError::Unsettled {
        pack: pack.clone(),
        rounds: MAX_ROUNDS,
    })
}
