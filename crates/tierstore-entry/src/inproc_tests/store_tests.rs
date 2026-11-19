//! The in-process store: layout, `flock`, restart in the same and in another boot (section 8
//! through `tierstore-journal`), close, removal and the failures of an open.
//!
//! Section 14 rows: "Process crash, same boot: restored cells `present`, no recheck; different
//! boot or no boot_id: every restored cell `recheck`" (on the in-process store); "No eviction
//! before the grace window; an entry locked by a second process is never evicted" (the lock
//! half); "Eviction removes an unheld entry's path" (the in-process part).
//!
//! Catches: two openers of one data file, or an opener that changes the file before it wins the
//! lock; a restart that loses committed cells, restores uncommitted ones, or trusts cells of
//! another boot (or of an unknown boot) without a recheck; a close that leaves the journal to
//! replay; a removal that leaves a file behind, removes a held entry, or drops an entry whose
//! files could not all be unlinked; an open failure that does not name its step.

use std::sync::Arc;

use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EntryError, EntryStore, Settle, StateView};
use tierstore_fetch_api::ids::HolderId;
use tierstore_journal::FileKind;

use super::{
    BOOT, MIB, OTHER_BOOT, byte, cell_bytes, open, pack, publish, run, scratch, spec, state, store,
};
use crate::inproc::{InProcStore, PROBE_STEM};
use crate::layout::file_hash;
use crate::{PACKS_DIR, StoreError};

#[test]
fn an_entry_lives_at_packs_v2_aa_bb_hash_with_its_journal_and_snapshot_beside_it() {
    let dir = scratch("inproc-layout");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", 2 * MIB + 7);
    let hash = file_hash(&pack("p"));
    let base = dir.join(PACKS_DIR).join(&hash[0..2]).join(&hash[2..4]);
    assert_eq!(s.dir(), dir.join(PACKS_DIR));
    for kind in [FileKind::Data, FileKind::Journal, FileKind::Snapshot] {
        assert_eq!(e.path(kind), base.join(format!("{hash}{}", kind.suffix())));
        assert!(e.path(kind).is_file(), "{kind:?} exists after the open");
    }
    let data = std::fs::metadata(e.path(FileKind::Data)).unwrap();
    assert_eq!(data.len(), 2 * MIB + 7, "the data file has the pack's size");
    assert_eq!(e.counts().absent, 3);
    assert!(
        e.fds().is_none(),
        "an in-process entry passes no descriptor"
    );
    assert!(
        !s.dir().join(format!("{PROBE_STEM}.pack")).exists(),
        "the punch probe leaves no file"
    );
}

#[test]
fn a_second_store_cannot_open_a_locked_entry_and_changes_nothing_of_it() {
    let dir = scratch("inproc-flock");
    let first = store(&dir, Some(BOOT));
    let e = open(&first, "p", 2 * MIB);
    publish(&e, 1, 9);
    let second = store(&dir, Some(BOOT));
    let err = second.open(&spec("p", 2 * MIB)).err().unwrap();
    assert!(
        matches!(err, EntryError::Io { op: "flock", .. }),
        "the second opener loses the lock: {err:?}"
    );
    assert!(second.packs().is_empty());
    assert_eq!(e.read_cell(1).unwrap(), cell_bytes(&e, 1, 9));
    assert_eq!(
        std::fs::metadata(e.path(FileKind::Data)).unwrap().len(),
        2 * MIB
    );
    // Once the first store and its entry are gone, the lock is free.
    drop(e);
    drop(first);
    second.open(&spec("p", 2 * MIB)).unwrap();
}

#[test]
fn a_pack_too_large_is_refused_before_any_file_is_made() {
    let dir = scratch("inproc-too-large");
    let s = store(&dir, Some(BOOT));
    let size = (u64::from(u32::MAX) + 1) << 20;
    let err = s.open(&spec("huge", size)).err().unwrap();
    assert!(matches!(err, EntryError::TooLarge { size: got, .. } if got == size));
    let entries: Vec<_> = std::fs::read_dir(s.dir()).unwrap().collect();
    assert!(entries.is_empty(), "no directory or file was made");
}

#[test]
fn committed_cells_come_back_present_after_a_crash_in_the_same_boot_and_uncommitted_ones_absent() {
    let dir = scratch("inproc-same-boot");
    {
        let s = store(&dir, Some(BOOT));
        let e = open(&s, "p", 3 * MIB + 10);
        publish(&e, 0, 1);
        publish(&e, 3, 4);
        let report = e.commit_batch().unwrap();
        assert_eq!(
            (report.data_syncs, report.cells_durable, report.records),
            (1, 2, 2)
        );
        publish(&e, 1, 2);
        e.touch(5_000, 0);
        // Dropped without close: a process crash.
    }
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", 3 * MIB + 10);
    let durable_present = tierstore_fetch_api::cell::CellByte::new(CellState::Present, true);
    assert_eq!(e.load(0), Some(durable_present));
    assert_eq!(e.load(3), Some(durable_present));
    assert_eq!(state(&e, 1), CellState::Absent, "never committed");
    assert_eq!(state(&e, 2), CellState::Absent);
    assert_eq!(e.read_cell(0).unwrap(), cell_bytes(&e, 0, 1));
    assert_eq!(e.read_cell(3).unwrap(), cell_bytes(&e, 3, 4));
}

#[test]
fn cells_of_another_boot_or_of_no_boot_id_come_back_recheck_and_a_recheck_settles_durably() {
    for (name, boot) in [("other", Some(OTHER_BOOT)), ("unknown", None)] {
        let dir = scratch(&format!("inproc-boot-{name}"));
        {
            let s = store(&dir, Some(BOOT));
            let e = open(&s, "p", 2 * MIB);
            publish(&e, 0, 1);
            publish(&e, 1, 2);
            e.commit_batch().unwrap();
            s.close().unwrap();
        }
        {
            let s = store(&dir, boot);
            let e = open(&s, "p", 2 * MIB);
            assert_eq!(state(&e, 0), CellState::Recheck, "{name}");
            assert_eq!(state(&e, 1), CellState::Recheck, "{name}");
            assert_eq!(e.counts().recheck, 2);
            // The server re-hashes a recheck cell and settles it.
            assert_eq!(e.read_cell(0).unwrap(), cell_bytes(&e, 0, 1));
            assert!(e.settle(0, Settle::RecheckOk));
            assert!(e.settle(1, Settle::RecheckFailed));
            assert_eq!(e.commit_batch().unwrap().records, 1, "{name}");
            s.close().unwrap();
        }
        // In the boot that settled it, the cell is trusted again; the failed one stays absent.
        let s = store(&dir, boot);
        let e = open(&s, "p", 2 * MIB);
        if boot.is_some() {
            assert_eq!(state(&e, 0), CellState::Present, "{name}");
        } else {
            assert_eq!(
                state(&e, 0),
                CellState::Recheck,
                "no boot id trusts nothing"
            );
        }
        assert_eq!(state(&e, 1), CellState::Absent, "{name}");
    }
}

#[test]
fn a_closed_entry_keeps_its_last_use_and_a_reopen_finds_it_present() {
    let dir = scratch("inproc-close");
    {
        let s = store(&dir, Some(BOOT));
        let e = open(&s, "p", MIB);
        publish(&e, 0, 7);
        e.commit_batch().unwrap();
        e.touch(42_000, 0);
        s.close().unwrap();
    }
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", MIB);
    assert_eq!(e.last_use(), 42_000);
    assert_eq!(state(&e, 0), CellState::Present);
    assert_eq!(e.read_cell(0).unwrap(), cell_bytes(&e, 0, 7));
}

#[test]
fn opening_a_pack_again_gives_the_same_entry_and_another_size_is_refused() {
    let dir = scratch("inproc-reopen");
    let s = store(&dir, Some(BOOT));
    let a = s.open(&spec("p", MIB)).unwrap();
    let b = s.open(&spec("p", MIB)).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert!(s.get(&pack("p")).is_some());
    assert!(s.get(&pack("q")).is_none());
    let err = s.open(&spec("p", 2 * MIB)).err().unwrap();
    assert!(matches!(
        err,
        EntryError::SizeMismatch {
            size: s2,
            existing: MIB,
            ..
        } if s2 == 2 * MIB
    ));
}

#[test]
fn a_data_file_of_another_length_is_sized_to_the_pack_at_open() {
    let dir = scratch("inproc-resize");
    {
        let s = store(&dir, Some(BOOT));
        let e = open(&s, "p", 2 * MIB);
        // A power loss can lose an ftruncate that was never synced.
        std::fs::OpenOptions::new()
            .write(true)
            .open(e.path(FileKind::Data))
            .unwrap()
            .set_len(10)
            .unwrap();
        s.close().unwrap();
    }
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", 2 * MIB);
    assert_eq!(
        std::fs::metadata(e.path(FileKind::Data)).unwrap().len(),
        2 * MIB
    );
}

#[test]
fn removing_an_unheld_entry_unlinks_every_file_and_a_held_or_unknown_one_is_kept() {
    let dir = scratch("inproc-remove");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", MIB);
    publish(&e, 0, 3);
    e.commit_batch().unwrap();
    e.hold(HolderId(1));
    assert!(
        !s.remove(&pack("p")).unwrap(),
        "a held entry is not removed"
    );
    assert!(!s.remove(&pack("none")).unwrap());
    assert!(e.release(HolderId(1)));
    let paths: Vec<_> = FileKind::ALL.iter().map(|k| e.path(*k)).collect();
    assert!(s.remove(&pack("p")).unwrap());
    for p in &paths {
        assert!(!p.exists(), "{} is unlinked", p.display());
    }
    assert!(s.packs().is_empty());
    assert!(s.get(&pack("p")).is_none());
    drop(e);
    // The next open makes a new, empty entry.
    let again = open(&s, "p", MIB);
    assert_eq!(state(&again, 0), CellState::Absent);
}

#[test]
fn a_failed_unlink_keeps_the_entry_listed_and_a_retry_finishes_the_removal() {
    let dir = scratch("inproc-remove-fails");
    let s = store(&dir, Some(BOOT));
    let e = open(&s, "p", MIB);
    // A non-empty directory where the snapshot's temporary file would be cannot be unlinked.
    let blocker = e.path(FileKind::SnapshotTemp);
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("x"), b"x").unwrap();
    let err = s.remove(&pack("p")).err().unwrap();
    assert!(matches!(
        err,
        EntryError::Io {
            op: "unlink entry file",
            ..
        }
    ));
    assert_eq!(s.packs(), vec![pack("p")], "still listed");
    std::fs::remove_dir_all(&blocker).unwrap();
    assert!(s.remove(&pack("p")).unwrap());
    assert!(!e.path(FileKind::Data).exists());
}

#[test]
fn open_failures_name_their_step() {
    // The cache directory is a file: the store cannot make its entries' directory.
    let dir = scratch("inproc-open-fails");
    let file = dir.join("cache");
    std::fs::write(&file, b"").unwrap();
    let err = InProcStore::new(&file, Some(BOOT)).err().unwrap();
    assert!(matches!(err, StoreError::Dir { ref path, .. } if *path == file.join(PACKS_DIR)));

    // A file where the entry's `aa` directory would be.
    let s = store(&dir, Some(BOOT));
    let hash = file_hash(&pack("p"));
    std::fs::write(s.dir().join(&hash[0..2]), b"").unwrap();
    let err = s.open(&spec("p", MIB)).err().unwrap();
    assert!(matches!(
        err,
        EntryError::Io {
            op: "make entry directory",
            ..
        }
    ));
    std::fs::remove_file(s.dir().join(&hash[0..2])).unwrap();

    // A directory where the data file would be.
    let entry_dir = s.dir().join(&hash[0..2]).join(&hash[2..4]);
    std::fs::create_dir_all(entry_dir.join(format!("{hash}.pack"))).unwrap();
    let err = s.open(&spec("p", MIB)).err().unwrap();
    assert!(matches!(
        err,
        EntryError::Io {
            op: "open data file",
            ..
        }
    ));
    assert!(s.packs().is_empty());
}

#[test]
fn entries_and_packs_list_every_open_entry_in_pack_order() {
    let dir = scratch("inproc-entries");
    let s = store(&dir, Some(BOOT));
    let b = open(&s, "b", 2 * MIB);
    let a = open(&s, "a", MIB);
    publish(&b, 1, 1);
    b.hold(HolderId(4));
    a.touch(9_000, 0);
    assert_eq!(s.packs(), vec![pack("a"), pack("b")]);
    let infos = s.entries();
    assert_eq!(infos.len(), 2);
    assert_eq!(
        (infos[0].pack.clone(), infos[0].last_use),
        (pack("a"), 9_000)
    );
    assert_eq!(
        (infos[1].size, infos[1].holders, infos[1].counts.present),
        (2 * MIB, 1, 1)
    );
    // A claimed cell counts as used too.
    let won = b.claim(run(0, 1));
    assert_eq!(s.entries()[1].counts.used(), 2);
    b.abandon(won.into_iter().next().unwrap());
    assert_eq!(state(&b, 0), CellState::Absent);
    assert_eq!(byte(CellState::Absent), b.load(0).unwrap());
}

#[test]
fn a_close_that_fails_on_one_entry_still_closes_the_others_and_returns_the_error() {
    let dir = scratch("inproc-close-fails");
    {
        let s = store(&dir, Some(BOOT));
        let a = open(&s, "a", MIB);
        let b = open(&s, "b", MIB);
        b.touch(77_000, 0);
        // A non-empty directory where `a`'s new snapshot would be written: its fold fails.
        let blocker = a.path(FileKind::SnapshotTemp);
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("x"), b"x").unwrap();
        let err = s.close().err().unwrap();
        assert!(matches!(err, EntryError::Io { .. }), "{err:?}");
        std::fs::remove_dir_all(&blocker).unwrap();
    }
    // `b` was closed after `a` failed: only a fold records its last use.
    let s = store(&dir, Some(BOOT));
    assert_eq!(open(&s, "b", MIB).last_use(), 77_000);
}
