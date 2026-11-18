//! [`Pack`]: an open pack as a reader uses it, the reader protocol of 5.1 ([`Pack::begin_read`])
//! and the waits of 5.4 ([`Pack::wait`]).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use arc_swap::ArcSwap;
use tierstore_fetch_api::cell::{CellByte, CellState};
use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::entry::{ReaderCounters, StateView};
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, PackId, Rank, TreeRef};
use tierstore_fetch_api::page::{CELL_SHIFT, StatePageLayout, groups_of};

use crate::coalesce::split;
use crate::counters::PackCounters;
use crate::driver::{Command, Commands};
use crate::error::ClientError;
use crate::ticket::{Answer, Ticket};
use crate::view::PackView;

/// What a mount names to open a pack: the tree and the pack's index in its list (as `open-pack`
/// names it), and the pack's identity and size from the same list. The table of open packs is
/// keyed by the identity, so two trees that name the same pack share one open pack, one counter
/// region and one hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSpec {
    /// The tree.
    pub tree: TreeRef,
    /// The pack's index in the tree's list.
    pub index: u32,
    /// The pack (`key`, `pack_root`) as the list names it.
    pub pack: PackId,
    /// Its size, from the list's record.
    pub size: u64,
}

/// An open pack. Its view (state page and data fd) is swapped for the server's new one when a
/// reconnect re-opens it; its counter region is the same for the mount's whole life.
#[derive(Debug)]
pub struct Pack {
    id: PackId,
    size: u64,
    layout: StatePageLayout,
    counters: PackCounters,
    view: ArcSwap<PackView>,
    /// Set once the pack can no longer be served: its re-open was refused, or the client ended.
    broken: OnceLock<ClientError>,
    /// The way to the client's runtime.
    commands: Commands,
}

impl Pack {
    pub(crate) fn new(
        id: PackId,
        layout: StatePageLayout,
        counters: PackCounters,
        view: PackView,
        commands: Commands,
    ) -> Self {
        Self {
            id,
            size: layout.pack_size,
            layout,
            counters,
            view: ArcSwap::from_pointee(view),
            broken: OnceLock::new(),
            commands,
        }
    }

    /// The pack.
    #[must_use]
    pub fn id(&self) -> &PackId {
        &self.id
    }

    /// Its size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The layout of its state page.
    #[must_use]
    pub fn layout(&self) -> StatePageLayout {
        self.layout
    }

    /// Its reader counters.
    #[must_use]
    pub fn counters(&self) -> &PackCounters {
        &self.counters
    }

    /// The current view: the state page and data fd of the last `open-pack`.
    #[must_use]
    pub fn view(&self) -> Arc<PackView> {
        self.view.load_full()
    }

    /// Why the pack can no longer be served, if it cannot.
    #[must_use]
    pub fn broken(&self) -> Option<&ClientError> {
        self.broken.get()
    }

    pub(crate) fn swap_view(&self, view: PackView) {
        self.view.store(Arc::new(view));
    }

    pub(crate) fn break_with(&self, err: ClientError) {
        let _ = self.broken.set(err);
    }

    /// The cells of `read` inside the pack (none for an empty read or one wholly past the end).
    fn cells(&self, read: ByteRange) -> Option<CellRun> {
        let end = read.end().unwrap_or(u64::MAX).min(self.size);
        let offset = read.offset.min(end);
        ByteRange {
            offset,
            len: end - offset,
        }
        .cells(CELL_SHIFT)
    }

    /// Steps 1 and 2 of the reader protocol (5.1) for `read`: `fetch_add` on the counter of every
    /// 16-cell group covering it, then a `SeqCst` load of every covered cell. When every cell is
    /// `present`, a [`ReadGuard`] that holds the counters until it is dropped (step 5, after the
    /// reply is committed) and offers the post-read recheck (step 4). Otherwise the counters are
    /// given back and the reader waits ([`Pack::wait`]), or answers EIO for a `bad` cell.
    pub fn begin_read(&self, read: ByteRange) -> Attempt<'_> {
        let view = self.view();
        let Some(cells) = self.cells(read) else {
            return Attempt::Ready(ReadGuard {
                pack: self,
                view,
                cells: None,
                groups: None,
            });
        };
        if self.broken().is_some() {
            return Attempt::NotReady { bad: None };
        }
        let groups = groups_of(cells);
        for g in groups.into_iter().flat_map(CellRun::cells) {
            self.counters.enter(g);
        }
        let guard = ReadGuard {
            pack: self,
            view,
            cells: Some(cells),
            groups,
        };
        match first_unready(guard.view.as_ref(), cells) {
            None => Attempt::Ready(guard),
            // Dropping the guard gives the counters back.
            Some(byte) => Attempt::NotReady {
                bad: byte
                    .filter(|(_, b)| b.state() == Some(CellState::Bad))
                    .map(|(c, _)| c),
            },
        }
    }

    /// Waits until the cells of `read` are ready (5.4). `span_end` is the end of the file's span
    /// in the pack: the wait covers the rest of the span up to the end of its 8-cell window, and
    /// joins a `wait` already outstanding for those cells. A read whose cells are all present now
    /// is answered at once with no request; an empty one, or one wholly past the end, is ready.
    pub fn wait(&self, read: ByteRange, span_end: u64, job: JobId) -> Ticket<WaitOutcome> {
        let Some(need) = self.cells(read) else {
            return Ticket::ready(Ok(WaitOutcome::Ready));
        };
        // A broken pack's view is not trusted: the client's runtime answers why it is broken.
        if self.broken().is_none() && first_unready(self.view().as_ref(), need).is_none() {
            self.commands.ready_local();
            return Ticket::ready(Ok(WaitOutcome::Ready));
        }
        let span_last = span_end.min(self.size).saturating_sub(1) >> CELL_SHIFT;
        // The cell numbers of a pack fit a u32.
        let span_last = u32::try_from(span_last).unwrap_or(u32::MAX);
        let parts = split(need, span_last);
        let (tx, ticket) = Ticket::new();
        let read = Arc::new(ReadWait::new(parts.len(), tx));
        self.commands.send(Command::Wait {
            pack: self.id.clone(),
            parts,
            job,
            read,
        });
        ticket
    }

    /// Asks the server to prefetch `runs` (5.4 `ensure`); returns at once. Dropped, and counted,
    /// when the client has no connection now: an `ensure` is a hint and is not re-sent.
    pub fn ensure(&self, runs: &[CellRun], class: Class, job: JobId, rank: Rank) {
        self.commands.send(Command::Ensure {
            pack: self.id.clone(),
            runs: runs.to_vec(),
            class,
            job,
            rank,
        });
    }
}

/// The first cell of `cells` that is not servable in `view`, and its byte (`None` when the cell
/// is outside the page); `None` when every cell is servable.
fn first_unready(view: &dyn StateView, cells: CellRun) -> Option<Option<(u32, CellByte)>> {
    cells
        .cells()
        .map(|c| (c, view.load(c)))
        .find(|(_, b)| !b.is_some_and(CellByte::servable))
        .map(|(c, b)| b.map(|b| (c, b)))
}

/// What [`Pack::begin_read`] found.
#[derive(Debug)]
pub enum Attempt<'a> {
    /// Every covered cell is present: read, recheck, reply, then drop the guard.
    Ready(ReadGuard<'a>),
    /// A covered cell is not present: wait, or answer EIO when `bad` names a `bad` cell.
    NotReady {
        /// The first covered cell, if it is `bad`.
        bad: Option<u32>,
    },
}

/// A read in progress: the reader counters of its groups are held until it is dropped (5.1 step
/// 5: drop it after the reply is committed).
#[derive(Debug)]
pub struct ReadGuard<'a> {
    pack: &'a Pack,
    view: Arc<PackView>,
    cells: Option<CellRun>,
    groups: Option<CellRun>,
}

impl ReadGuard<'_> {
    /// The view the cells were checked in: read from its data fd.
    #[must_use]
    pub fn view(&self) -> &PackView {
        &self.view
    }

    /// The post-read recheck (5.1 step 4): whether every covered cell is still `present`. When it
    /// is not, the reader discards the bytes it read and waits.
    #[must_use]
    pub fn recheck(&self) -> bool {
        self.cells
            .is_none_or(|cells| first_unready(self.view.as_ref(), cells).is_none())
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        for g in self.groups.into_iter().flat_map(CellRun::cells) {
            self.pack.counters.leave(g);
        }
    }
}

/// One read's wait: the parts (one per window) it was cut into, and the answer once each has
/// ended. The outcome is the worst of the parts': an error, then `lost`, then `eio` of the lowest
/// cell, then `ready`.
#[derive(Debug)]
pub(crate) struct ReadWait {
    state: Mutex<ReadState>,
}

#[derive(Debug)]
struct ReadState {
    remaining: usize,
    outcome: Result<WaitOutcome, ClientError>,
    answer: Option<Answer<WaitOutcome>>,
}

impl ReadWait {
    pub(crate) fn new(parts: usize, answer: Answer<WaitOutcome>) -> Self {
        Self {
            state: Mutex::new(ReadState {
                remaining: parts,
                outcome: Ok(WaitOutcome::Ready),
                answer: Some(answer),
            }),
        }
    }

    /// One part ended with `outcome`; the answer is sent when the last part ends.
    pub(crate) fn part_done(&self, outcome: Result<WaitOutcome, ClientError>) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.outcome = worse(
            std::mem::replace(&mut s.outcome, Ok(WaitOutcome::Ready)),
            outcome,
        );
        s.remaining = s.remaining.saturating_sub(1);
        if s.remaining == 0
            && let Some(answer) = s.answer.take()
        {
            // The reader may have dropped its ticket: nobody waits.
            let _ = answer.send(s.outcome.clone());
        }
    }
}

/// The worse of two outcomes.
fn worse(
    a: Result<WaitOutcome, ClientError>,
    b: Result<WaitOutcome, ClientError>,
) -> Result<WaitOutcome, ClientError> {
    fn rank(o: &Result<WaitOutcome, ClientError>) -> (u8, u32) {
        match o {
            Err(_) => (3, 0),
            Ok(WaitOutcome::Lost) => (2, 0),
            // A lower cell is worse: it is the first failed cell of the read.
            Ok(WaitOutcome::Eio { cell }) => (1, u32::MAX - cell),
            Ok(WaitOutcome::Ready) => (0, 0),
        }
    }
    if rank(&b) > rank(&a) { b } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_wait_answers_the_worst_outcome_once_its_parts_end() {
        let ready = Ok(WaitOutcome::Ready);
        type Answer = Result<WaitOutcome, ClientError>;
        let cases: [(Vec<Answer>, Answer); 5] = [
            (vec![ready.clone(), ready.clone()], ready.clone()),
            (
                vec![
                    Ok(WaitOutcome::Eio { cell: 9 }),
                    Ok(WaitOutcome::Eio { cell: 3 }),
                ],
                Ok(WaitOutcome::Eio { cell: 3 }),
            ),
            (
                vec![
                    Ok(WaitOutcome::Eio { cell: 3 }),
                    Ok(WaitOutcome::Eio { cell: 9 }),
                ],
                Ok(WaitOutcome::Eio { cell: 3 }),
            ),
            (
                vec![
                    Ok(WaitOutcome::Lost),
                    Ok(WaitOutcome::Eio { cell: 1 }),
                    ready.clone(),
                ],
                Ok(WaitOutcome::Lost),
            ),
            (
                vec![
                    Ok(WaitOutcome::Lost),
                    Err(ClientError::Released),
                    Err(ClientError::Stopped),
                ],
                Err(ClientError::Released),
            ),
        ];
        for (parts, want) in cases {
            let (tx, mut ticket) = Ticket::new();
            let read = ReadWait::new(parts.len(), tx);
            let n = parts.len();
            for (i, p) in parts.into_iter().enumerate() {
                assert_eq!(ticket.try_take(), None, "answered before part {i} of {n}");
                read.part_done(p);
            }
            assert_eq!(ticket.try_take(), Some(want));
            // A part past the last changes nothing.
            read.part_done(Ok(WaitOutcome::Lost));
        }
    }
}
