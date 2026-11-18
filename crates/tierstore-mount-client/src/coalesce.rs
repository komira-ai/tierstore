//! Wait coalescing (5.4): **at most one outstanding `wait` per 8 cells** of a pack.
//!
//! A pack's cells are cut into aligned windows of [`WINDOW_CELLS`] cells. A read that needs cells
//! it found not ready is cut into one *part* per window it touches; a part has the cells the read
//! needs in that window (`need`) and the cells its wait would cover (`cover`): from the first
//! needed cell to the end of the file's span, but not past the window ("a `wait` covers the
//! remaining span up to the next 8 cells"), so a reader that goes on through the file finds the
//! next cells ready, or already waited for.
//!
//! A window has at most one wait outstanding. A part whose `need` is inside that wait's cells
//! **joins** it; any other part is **queued** for the window's next wait. When the outstanding
//! wait ends:
//! - `ready`: every joined part is ready; `lost`: every joined part is lost;
//! - `eio(cell)` of a cell of the wait: a joined part that needs `cell` ends with it; one that
//!   does not is queued again with its `need` alone as its cover, since the wait ended before its
//!   cells did. An `eio` of a cell outside the wait's cells ends every joined part with it (the
//!   server answered for the range as a whole);
//! - the queued parts are returned, and the caller starts the next wait with them (after it drops
//!   any it finds ready in the state page). The next wait's cells are the first queued part's
//!   cover, never a hull of several: the parts it covers join it and the rest stay queued. So a
//!   part re-queued on its need alone is never waited for again over a cell it does not need,
//!   and a bad cell between two re-queued parts cannot send the same wait round after round.
//!
//! The module is pure: no I/O and no clock. `J` is what a part reports to (the read's waiter).

use std::collections::BTreeMap;

use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::CellRun;

/// The cells of a coalescing window.
pub(crate) const WINDOW_CELLS: u32 = 8;

/// The part of one read inside one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part<J> {
    /// The window.
    pub(crate) window: u32,
    /// The cells the read needs in the window.
    pub(crate) need: CellRun,
    /// The cells a wait for this part covers: `need` and the rest of the span in the window.
    pub(crate) cover: CellRun,
    /// Who the part's outcome goes to.
    pub(crate) joiner: J,
}

/// The parts of a read of cells `need` in a file whose span's last cell is `span_last`: one per
/// window the read touches, in ascending order: `(window, need, cover)`. Empty for no cells, and
/// for a run whose end is past `u32::MAX`.
pub(crate) fn split(need: CellRun, span_last: u32) -> Vec<(u32, CellRun, CellRun)> {
    let Some(last) = need
        .count
        .checked_sub(1)
        .and_then(|n| need.first.checked_add(n))
    else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    let mut first = need.first;
    loop {
        let window = first / WINDOW_CELLS;
        let window_last = window
            .saturating_mul(WINDOW_CELLS)
            .saturating_add(WINDOW_CELLS - 1);
        let need_last = last.min(window_last);
        let cover_last = span_last.max(need_last).min(window_last);
        parts.push((window, run(first, need_last), run(first, cover_last)));
        if need_last == last {
            return parts;
        }
        first = need_last + 1;
    }
}

/// The run of cells `first..=last` (`first <= last`).
fn run(first: u32, last: u32) -> CellRun {
    CellRun {
        first,
        count: last - first + 1,
    }
}

/// The last cell of a non-empty run.
fn last(r: CellRun) -> u32 {
    r.first + (r.count - 1)
}

/// Whether `inner` is inside `outer`.
fn contains(outer: CellRun, inner: CellRun) -> bool {
    inner.first >= outer.first && last(inner) <= last(outer)
}

/// What [`Windows::add`] did with a part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Added {
    /// The window had no wait: a wait of these cells is to be sent.
    New(CellRun),
    /// The part joined the outstanding wait.
    Joined,
    /// The part waits for the window's next wait.
    Queued,
}

/// One window's outstanding wait.
#[derive(Debug)]
pub(crate) struct Window<J> {
    /// The cells of the outstanding wait.
    pub(crate) cells: CellRun,
    /// The request that carries it on the current connection; `None` until it is sent (after a
    /// reconnect: until the pack is re-opened).
    pub(crate) request: Option<u64>,
    /// The parts that joined it, each with its `need`.
    joined: Vec<(CellRun, J)>,
    /// The parts queued for the next wait.
    queued: Vec<Part<J>>,
}

/// The windows of one pack that have a wait outstanding.
#[derive(Debug)]
pub(crate) struct Windows<J> {
    map: BTreeMap<u32, Window<J>>,
}

impl<J> Default for Windows<J> {
    fn default() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }
}

/// What the end of a window's wait gives back.
#[derive(Debug)]
pub(crate) struct Ended<J> {
    /// Each joined part that ended, with its outcome.
    pub(crate) done: Vec<(J, WaitOutcome)>,
    /// The parts for the window's next wait (none: the window has no wait any more).
    pub(crate) next: Vec<Part<J>>,
}

impl<J> Window<J> {
    /// The first part that joined the wait: its job is the wait's (4.5 fairness is per job).
    pub(crate) fn first(&self) -> Option<&J> {
        self.joined.first().map(|(_, j)| j)
    }
}

impl<J> Windows<J> {
    /// Adds `part`: it joins the window's wait if that covers its need, is queued if the window
    /// has another wait, or starts the window's wait.
    pub(crate) fn add(&mut self, part: Part<J>) -> Added {
        match self.map.get_mut(&part.window) {
            Some(w) if contains(w.cells, part.need) => {
                w.joined.push((part.need, part.joiner));
                Added::Joined
            }
            Some(w) => {
                w.queued.push(part);
                Added::Queued
            }
            None => {
                let cells = part.cover;
                self.map.insert(
                    part.window,
                    Window {
                        cells,
                        request: None,
                        joined: vec![(part.need, part.joiner)],
                        queued: Vec::new(),
                    },
                );
                Added::New(cells)
            }
        }
    }

    /// Starts the window's wait for `parts` (the queued parts of its last wait, all of one
    /// window): its cells are the first part's cover; every part whose need is inside them joins
    /// it, and the others are queued for the wait after it. `None`, and nothing changes, when
    /// `parts` is empty.
    pub(crate) fn restart(&mut self, parts: Vec<Part<J>>) -> Option<(u32, CellRun)> {
        let first = parts.first()?;
        let (window, cells) = (first.window, first.cover);
        let (joined, queued): (Vec<_>, Vec<_>) =
            parts.into_iter().partition(|p| contains(cells, p.need));
        self.map.insert(
            window,
            Window {
                cells,
                request: None,
                joined: joined.into_iter().map(|p| (p.need, p.joiner)).collect(),
                queued,
            },
        );
        Some((window, cells))
    }

    /// The window's wait, if it has one.
    pub(crate) fn get_mut(&mut self, window: u32) -> Option<&mut Window<J>> {
        self.map.get_mut(&window)
    }

    /// Every window's wait, in window order.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (u32, &mut Window<J>)> {
        self.map.iter_mut().map(|(w, win)| (*w, win))
    }

    /// The number of windows with a wait outstanding.
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// The window's wait ended with `outcome`; the window has no wait until the caller restarts
    /// it with [`Ended::next`]. `None` when the window had no wait.
    pub(crate) fn end(&mut self, window: u32, outcome: WaitOutcome) -> Option<Ended<J>> {
        let w = self.map.remove(&window)?;
        let mut done = Vec::new();
        let mut next = Vec::new();
        for (need, joiner) in w.joined {
            match outcome {
                WaitOutcome::Eio { cell }
                    if contains(w.cells, run(cell, cell)) && !contains(need, run(cell, cell)) =>
                {
                    next.push(Part {
                        window,
                        need,
                        cover: need,
                        joiner,
                    });
                }
                outcome => done.push((joiner, outcome)),
            }
        }
        next.extend(w.queued);
        Some(Ended { done, next })
    }

    /// Every part of every window, the windows forgotten: the pack can no longer be waited on.
    pub(crate) fn drain(&mut self) -> Vec<J> {
        std::mem::take(&mut self.map)
            .into_values()
            .flat_map(|w| {
                w.joined
                    .into_iter()
                    .map(|(_, j)| j)
                    .chain(w.queued.into_iter().map(|p| p.joiner))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(first: u32, count: u32) -> CellRun {
        CellRun { first, count }
    }

    fn part(window: u32, need: CellRun, cover: CellRun, joiner: u32) -> Part<u32> {
        Part {
            window,
            need,
            cover,
            joiner,
        }
    }

    #[test]
    fn a_read_is_cut_per_window_and_covers_the_span_to_the_window_end() {
        // One cell, span to cell 20: covers to the end of window 0.
        assert_eq!(split(r(3, 1), 20), vec![(0, r(3, 1), r(3, 5))]);
        // A span that ends inside the window.
        assert_eq!(split(r(3, 1), 5), vec![(0, r(3, 1), r(3, 3))]);
        // A span that ends before the read's last cell covers the need.
        assert_eq!(split(r(3, 2), 0), vec![(0, r(3, 2), r(3, 2))]);
        // Three windows.
        assert_eq!(
            split(r(6, 12), 40),
            vec![
                (0, r(6, 2), r(6, 2)),
                (1, r(8, 8), r(8, 8)),
                (2, r(16, 2), r(16, 8)),
            ]
        );
        // No cells.
        assert!(split(r(4, 0), 9).is_empty());
        // The last window of u32 cells.
        assert_eq!(
            split(r(u32::MAX - 1, 2), u32::MAX),
            vec![(u32::MAX / 8, r(u32::MAX - 1, 2), r(u32::MAX - 1, 2))]
        );
        // A run whose end overflows is no cells.
        assert!(split(r(u32::MAX, 2), u32::MAX).is_empty());
    }

    #[test]
    fn a_covered_part_joins_and_another_waits_for_the_next() {
        let mut w = Windows::default();
        assert_eq!(w.add(part(0, r(2, 1), r(2, 6), 1)), Added::New(r(2, 6)));
        assert_eq!(w.add(part(0, r(4, 2), r(4, 4), 2)), Added::Joined);
        // Cell 1 is not in the outstanding wait's cells: queued.
        assert_eq!(w.add(part(0, r(1, 1), r(1, 7), 3)), Added::Queued);
        assert_eq!(w.add(part(0, r(0, 1), r(0, 1), 4)), Added::Queued);
        // Another window starts its own wait.
        assert_eq!(w.add(part(1, r(8, 1), r(8, 1), 5)), Added::New(r(8, 1)));
        assert_eq!(w.len(), 2);

        let ended = w.end(0, WaitOutcome::Ready).unwrap();
        assert_eq!(
            ended.done,
            vec![(1, WaitOutcome::Ready), (2, WaitOutcome::Ready)]
        );
        assert_eq!(
            ended.next,
            vec![part(0, r(1, 1), r(1, 7), 3), part(0, r(0, 1), r(0, 1), 4)]
        );
        assert_eq!(w.len(), 1);
        // The next wait covers the first queued part's cover; the other stays queued.
        assert_eq!(w.restart(ended.next), Some((0, r(1, 7))));
        assert_eq!(w.add(part(0, r(7, 1), r(7, 1), 6)), Added::Joined);
        let ended = w.end(0, WaitOutcome::Lost).unwrap();
        assert_eq!(
            ended.done,
            vec![(3, WaitOutcome::Lost), (6, WaitOutcome::Lost)]
        );
        assert_eq!(ended.next, vec![part(0, r(0, 1), r(0, 1), 4)]);
        assert_eq!(w.restart(ended.next), Some((0, r(0, 1))));
        let ended = w.end(0, WaitOutcome::Ready).unwrap();
        assert_eq!(ended.done, vec![(4, WaitOutcome::Ready)]);
        assert!(ended.next.is_empty());
        assert_eq!(w.restart(Vec::new()), None);
        assert!(w.end(0, WaitOutcome::Ready).is_none());
    }

    #[test]
    fn an_eio_outside_a_part_need_queues_it_again_on_its_need_alone() {
        let mut w = Windows::default();
        assert_eq!(w.add(part(0, r(1, 1), r(1, 7), 1)), Added::New(r(1, 7)));
        assert_eq!(w.add(part(0, r(5, 2), r(5, 3), 2)), Added::Joined);
        let ended = w.end(0, WaitOutcome::Eio { cell: 6 }).unwrap();
        assert_eq!(ended.done, vec![(2, WaitOutcome::Eio { cell: 6 })]);
        assert_eq!(ended.next, vec![part(0, r(1, 1), r(1, 1), 1)]);

        // The re-queued part's wait is its need alone: an eio outside it ends it.
        assert_eq!(w.restart(ended.next), Some((0, r(1, 1))));
        let ended = w.end(0, WaitOutcome::Eio { cell: 6 }).unwrap();
        assert_eq!(ended.done, vec![(1, WaitOutcome::Eio { cell: 6 })]);
        assert!(ended.next.is_empty());
    }

    #[test]
    fn a_bad_cell_between_two_requeued_parts_is_never_waited_for_again() {
        let mut w = Windows::default();
        // A read of cell 0 in a file to the window's end; a read of cell 7 joins.
        assert_eq!(w.add(part(0, r(0, 1), r(0, 8), 1)), Added::New(r(0, 8)));
        assert_eq!(w.add(part(0, r(7, 1), r(7, 1), 2)), Added::Joined);
        // Cell 3, which neither needs, is bad: both are re-queued on their own cell.
        let ended = w.end(0, WaitOutcome::Eio { cell: 3 }).unwrap();
        assert!(ended.done.is_empty());
        // The next wait is cell 0 alone (not 0 to 7 again); cell 7 waits for the one after.
        assert_eq!(w.restart(ended.next), Some((0, r(0, 1))));
        let ended = w.end(0, WaitOutcome::Ready).unwrap();
        assert_eq!(ended.done, vec![(1, WaitOutcome::Ready)]);
        assert_eq!(w.restart(ended.next), Some((0, r(7, 1))));
        let ended = w.end(0, WaitOutcome::Ready).unwrap();
        assert_eq!(ended.done, vec![(2, WaitOutcome::Ready)]);
        assert!(ended.next.is_empty());
    }

    #[test]
    fn requests_and_drain() {
        let mut w = Windows::default();
        w.add(part(0, r(0, 1), r(0, 1), 1));
        w.add(part(0, r(3, 1), r(3, 1), 2));
        w.add(part(2, r(16, 1), r(16, 1), 3));
        w.get_mut(0).unwrap().request = Some(9);
        assert!(w.get_mut(1).is_none());
        let requests: Vec<(u32, Option<u64>)> = w.iter_mut().map(|(i, x)| (i, x.request)).collect();
        assert_eq!(requests, vec![(0, Some(9)), (2, None)]);
        let mut all = w.drain();
        all.sort_unstable();
        assert_eq!(all, vec![1, 2, 3]);
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn contains_is_inclusive_at_both_ends() {
        assert!(contains(r(2, 5), r(2, 5)));
        assert!(contains(r(2, 5), r(3, 2)));
        assert!(!contains(r(2, 5), r(1, 2)));
        assert!(!contains(r(2, 5), r(5, 3)));
    }
}
