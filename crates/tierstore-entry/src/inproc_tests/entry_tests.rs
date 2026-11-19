//! The in-process entry beyond the conformance suite: the journaled punch on a volume that
//! punches and the refusal on one that does not (5.6 (b)), the punch probe, the commit of a cell
//! whose eviction was aborted, and the in-process reader counters the evictor sums.
//!
//! Section 14 rows: "Crash after a batched PUNCH fsync and before/after the punch: replay never
//! marks a punched cell `present`; one fsync per batch" (on the in-process store); "Held-only
//! cache: punch eviction admits a new job" (the punch half; the fallback refuses instead).
//!
//! Catches: a punch that does not release the blocks, that changes the size, or whose cell a
//! restart resurrects; a store that cannot punch but journals, punches or changes a cell anyway,
//! or refuses an empty batch; a probe that says a volume punches when it does not, or leaves its
//! file; an aborted eviction whose cell is never made durable; an evictor that misses an
//! in-process reader.

use std::os::unix::fs::MetadataExt;

use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EntryError, EvictStep, ReaderCounters, StateView};
use tierstore_journal::FileKind;

use super::{BOOT, MIB, cell_bytes, journal_len, open, publish, run, scratch, state, store};
use crate::inproc::{PROBE_STEM, punches_inside_a_file};

/// The blocks of the entry's data file (512-byte units).
fn blocks(e: &crate::inproc::InProcEntry) -> u64 {
    std::fs::metadata(e.path(FileKind::Data)).unwrap().blocks()
}

#[cfg(target_os = "linux")]
#[test]
fn a_punch_journals_once_releases_the_blocks_and_a_restart_never_restores_the_cells() {
    let dir = scratch("inproc-punch");
    {
        let s = store(&dir, Some(BOOT));
        assert!(s.can_punch(), "a Linux scratch volume punches");
        let e = open(&s, "p", 4 * MIB);
        assert!(e.can_punch());
        for c in 0..4 {
            publish(&e, c, c as u8 + 1);
        }
        e.commit_batch().unwrap();
        let before = blocks(&e);
        for c in [0, 2, 3] {
            assert!(e.evict_cell_cas(c, EvictStep::Begin));
        }
        let report = e.punch(&[run(0, 1), run(2, 2), run(1, 0)]).unwrap();
        assert_eq!(
            (report.journal_syncs, report.records, report.bytes_punched),
            (1, 2, 3 * MIB)
        );
        assert!(blocks(&e) < before, "the punched blocks are released");
        assert_eq!(
            std::fs::metadata(e.path(FileKind::Data)).unwrap().len(),
            4 * MIB
        );
        for c in [0, 2, 3] {
            assert!(e.evict_cell_cas(c, EvictStep::Finish));
        }
        // Dropped without close: the punch is replayed from the journal.
    }
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", 4 * MIB);
    assert_eq!(
        (0..4).map(|c| state(&e, c)).collect::<Vec<_>>(),
        [
            CellState::Absent,
            CellState::Present,
            CellState::Absent,
            CellState::Absent
        ]
    );
    assert_eq!(e.read_cell(1).unwrap(), cell_bytes(&e, 1, 2));
}

#[test]
fn a_store_that_cannot_punch_refuses_a_batch_before_any_effect_and_the_cells_abort_to_present() {
    let dir = scratch("inproc-no-punch");
    let s = store(&dir, Some(BOOT)).without_punch();
    assert!(!s.can_punch());
    let e = open(&s, "p", 3 * MIB);
    assert!(!e.can_punch());
    publish(&e, 0, 5);
    publish(&e, 1, 6);
    e.commit_batch().unwrap();
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    let journal = journal_len(&e);
    let blocks_before = blocks(&e);

    // The cells are checked first, as on a volume that punches.
    assert!(matches!(
        e.punch(&[run(0, 2)]),
        Err(EntryError::NotEvicting { cell: 1, .. })
    ));
    assert!(matches!(
        e.punch(&[run(3, 1)]),
        Err(EntryError::CellOutOfRange {
            cell: 3,
            cells: 3,
            ..
        })
    ));
    // Nothing to punch is a report of zeros.
    assert_eq!(e.punch(&[run(0, 0)]).unwrap(), Default::default());
    assert_eq!(e.punch(&[]).unwrap(), Default::default());
    // Then a real batch is refused.
    let err = e.punch(&[run(0, 1)]).err().unwrap();
    assert!(
        matches!(&err, EntryError::Io { op: "punch", source, .. }
            if source.kind() == std::io::ErrorKind::Unsupported),
        "{err:?}"
    );
    assert_eq!(journal_len(&e), journal, "nothing journaled");
    assert_eq!(blocks(&e), blocks_before, "nothing released");
    assert_eq!(state(&e, 0), CellState::Evicting);
    // The evictor aborts and evicts whole entries instead.
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(e.read_cell(0).unwrap(), cell_bytes(&e, 0, 5));
}

#[test]
fn an_aborted_eviction_of_a_cell_not_yet_durable_is_made_durable_by_the_next_commit() {
    let dir = scratch("inproc-abort-commit");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", 2 * MIB);
    publish(&e, 0, 1);
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    // The commit meets the cell evicting: not durable yet.
    assert_eq!(e.commit_batch().unwrap().cells_durable, 0);
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    let report = e.commit_batch().unwrap();
    assert_eq!((report.cells_durable, report.records), (1, 1));
    assert!(e.load(0).unwrap().durable());
    // A durable cell aborted back is not committed twice.
    assert!(e.evict_cell_cas(0, EvictStep::Begin));
    assert!(e.evict_cell_cas(0, EvictStep::Abort));
    assert_eq!(e.commit_batch().unwrap().records, 0);
    // An evict step that loses its CAS tells the journal nothing.
    assert!(!e.evict_cell_cas(1, EvictStep::Abort));
    assert_eq!(e.commit_batch().unwrap().records, 0);
}

#[test]
fn the_punch_probe_says_whether_the_volume_punches_inside_a_file_and_leaves_no_file() {
    let dir = scratch("inproc-probe");
    let punches = punches_inside_a_file(dir.path());
    assert_eq!(punches, cfg!(target_os = "linux"));
    assert!(!dir.join(format!("{PROBE_STEM}.pack")).exists());
    // A directory that does not exist cannot be probed: no punch.
    assert!(!punches_inside_a_file(&dir.join("absent")));
}

#[test]
fn group_readers_count_the_entrys_own_readers_and_ignore_groups_outside_the_pack() {
    let dir = scratch("inproc-readers");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", 20 * MIB);
    e.enter(1);
    e.enter(1);
    e.enter(9);
    e.leave(9);
    assert_eq!(e.group_readers(1), 2);
    assert_eq!(e.group_readers(0), 0);
    assert_eq!(e.group_readers(9), 0, "a group outside the pack counts 0");
    e.leave(1);
    e.leave(1);
    assert_eq!(e.group_readers(1), 0);
    assert_eq!(e.group_count(), 2);
}
