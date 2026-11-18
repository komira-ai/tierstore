//! The crash-modelling file system the crash tests run on ([`MemFs`]): what a process crash and
//! a power loss keep, injected failures, the crash point and the counts.
//!
//! Catches: a fake that keeps unsynced bytes after a power loss with `Keep::Nothing` (the crash
//! tests would then pass on a journal that skips a sync), that loses synced bytes, that makes a
//! new name or a rename durable without a directory sync, that never tears a write, whose
//! process crash loses the page cache, whose faults or crash point have an effect, or whose
//! counts include failed calls.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::ErrorKind;

use tierstore_journal::fake::{Crash, Keep, MemFs, Op};
use tierstore_journal::{EntryFiles, FileKind};

#[test]
fn a_write_is_read_at_once_and_is_durable_only_after_its_sync_and_the_directory_s() {
    let fs = MemFs::new();
    assert_eq!(fs.read(FileKind::Journal).unwrap(), None);
    fs.write_at(FileKind::Journal, 2, b"ab").unwrap();
    assert_eq!(fs.read(FileKind::Journal).unwrap().unwrap(), b"\0\0ab");
    assert_eq!(fs.current(FileKind::Journal).unwrap(), b"\0\0ab");
    assert_eq!(
        fs.durable(FileKind::Journal),
        None,
        "the name is not durable yet"
    );
    assert!(!fs.is_durable(FileKind::Journal, 0, 4));
    fs.sync_data(FileKind::Journal).unwrap();
    assert_eq!(
        fs.durable(FileKind::Journal),
        None,
        "still no directory sync"
    );
    fs.sync_dir().unwrap();
    assert_eq!(fs.durable(FileKind::Journal).unwrap(), b"\0\0ab");
    assert!(fs.is_durable(FileKind::Journal, 0, 4));
    assert!(
        fs.is_durable(FileKind::Journal, 2, 10),
        "bytes past the end read as zeros"
    );
    fs.write_at(FileKind::Journal, 0, b"z").unwrap();
    assert!(!fs.is_durable(FileKind::Journal, 0, 1));
    assert!(fs.is_durable(FileKind::Journal, 1, 3));
    assert!(
        !fs.is_durable(FileKind::Data, 0, 1),
        "a missing file is not durable"
    );
}

#[test]
fn a_power_loss_keeps_what_was_synced_and_of_the_rest_what_keep_chooses() {
    let fs = MemFs::new();
    fs.write_at(FileKind::Data, 0, &[1; 8]).unwrap();
    fs.sync_data(FileKind::Data).unwrap();
    fs.sync_dir().unwrap();
    fs.write_at(FileKind::Data, 0, &[2; 4]).unwrap();
    fs.punch_data(6, 2).unwrap();
    fs.set_len(FileKind::Data, 10).unwrap();
    fs.write_at(FileKind::Journal, 0, b"new").unwrap();

    let none = fs.crash(Crash::Power(Keep::Nothing));
    assert_eq!(none.current(FileKind::Data).unwrap(), [1; 8]);
    assert_eq!(
        none.current(FileKind::Journal),
        None,
        "an unsynced name is lost"
    );

    let all = fs.crash(Crash::Power(Keep::Everything));
    assert_eq!(
        all.current(FileKind::Data).unwrap(),
        [2, 2, 2, 2, 1, 1, 0, 0, 0, 0]
    );
    assert_eq!(all.current(FileKind::Journal).unwrap(), b"new");
    // After the loss everything is durable: nothing more is lost.
    let again = all.crash(Crash::Power(Keep::Nothing));
    assert_eq!(again.current(FileKind::Data), all.current(FileKind::Data));

    // Seeded choices differ by seed, are the same for the same seed, and tear some writes.
    let mut outcomes = std::collections::BTreeSet::new();
    let mut torn = false;
    for seed in 0..64 {
        let a = fs
            .crash(Crash::Power(Keep::Seeded(seed)))
            .current(FileKind::Data);
        let b = fs
            .crash(Crash::Power(Keep::Seeded(seed)))
            .current(FileKind::Data);
        assert_eq!(a, b, "seed {seed}");
        let a = a.unwrap();
        torn |= a.len() >= 4 && a[..4].contains(&2) && a[..4].contains(&1);
        outcomes.insert(a);
    }
    assert!(outcomes.len() > 3, "{outcomes:?}");
    assert!(torn, "some seed keeps a torn prefix of the 4-byte write");
}

#[test]
fn a_rename_is_durable_only_after_the_directory_sync() {
    let fs = MemFs::new();
    fs.write_at(FileKind::Snapshot, 0, b"old").unwrap();
    fs.sync_data(FileKind::Snapshot).unwrap();
    fs.sync_dir().unwrap();
    fs.write_at(FileKind::SnapshotTemp, 0, b"new").unwrap();
    fs.sync_data(FileKind::SnapshotTemp).unwrap();
    fs.replace_snapshot().unwrap();
    assert_eq!(fs.current(FileKind::Snapshot).unwrap(), b"new");
    assert_eq!(fs.current(FileKind::SnapshotTemp), None);
    // The bytes of both files are synced, but the name now points at another inode than the
    // durable one: not durable until the directory sync.
    assert!(!fs.is_durable(FileKind::Snapshot, 0, 3));
    let lost = fs.crash(Crash::Power(Keep::Nothing));
    assert_eq!(lost.current(FileKind::Snapshot).unwrap(), b"old");
    let kept = fs.crash(Crash::Power(Keep::Everything));
    assert_eq!(kept.current(FileKind::Snapshot).unwrap(), b"new");
    assert_eq!(kept.current(FileKind::SnapshotTemp), None);
    fs.sync_dir().unwrap();
    assert!(fs.is_durable(FileKind::Snapshot, 0, 3));
    assert_eq!(
        fs.crash(Crash::Power(Keep::Nothing))
            .current(FileKind::Snapshot)
            .unwrap(),
        b"new"
    );
    // No temp to rename.
    assert_eq!(
        fs.replace_snapshot().unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[test]
fn a_process_crash_keeps_the_page_cache_and_what_it_does_not_make_durable() {
    let fs = MemFs::new();
    fs.write_at(FileKind::Data, 0, b"x").unwrap();
    let after = fs.crash(Crash::Process);
    assert_eq!(after.current(FileKind::Data).unwrap(), b"x");
    // A power loss after the process crash still loses the unsynced write.
    assert_eq!(
        after
            .crash(Crash::Power(Keep::Nothing))
            .current(FileKind::Data),
        None
    );
    // The restart's file system is separate from the dead process's.
    after.write_at(FileKind::Data, 0, b"y").unwrap();
    assert_eq!(fs.current(FileKind::Data).unwrap(), b"x");
}

#[test]
fn an_injected_failure_hits_the_nth_matching_call_once_and_has_no_effect() {
    let fs = MemFs::new();
    fs.fail(
        Op::WriteAt,
        Some(FileKind::Journal),
        1,
        ErrorKind::StorageFull,
    );
    fs.write_at(FileKind::Data, 0, b"d").unwrap(); // another file: not counted
    fs.write_at(FileKind::Journal, 0, b"a").unwrap();
    let err = fs.write_at(FileKind::Journal, 0, b"b").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::StorageFull);
    assert_eq!(fs.current(FileKind::Journal).unwrap(), b"a");
    fs.write_at(FileKind::Journal, 0, b"c").unwrap();
    assert_eq!(
        fs.count(Op::WriteAt, Some(FileKind::Journal)),
        2,
        "the failed call is not counted"
    );
    assert_eq!(fs.count(Op::WriteAt, Some(FileKind::Data)), 1);
    // A fault on any file.
    fs.fail(Op::Read, None, 0, ErrorKind::Interrupted);
    assert_eq!(
        fs.read(FileKind::Snapshot).unwrap_err().kind(),
        ErrorKind::Interrupted
    );
    assert_eq!(fs.read_range(FileKind::Data, 0, 3).unwrap(), b"d\0\0");
    assert_eq!(
        fs.read_range(FileKind::Snapshot, 0, 1).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    for op in [
        Op::SetLen,
        Op::SyncData,
        Op::PunchData,
        Op::ReplaceSnapshot,
        Op::SyncDir,
    ] {
        fs.fail(op, None, 0, ErrorKind::Other);
    }
    fs.write_at(FileKind::SnapshotTemp, 0, b"t").unwrap();
    assert!(fs.set_len(FileKind::Data, 9).is_err());
    assert!(fs.sync_data(FileKind::Data).is_err());
    assert!(fs.punch_data(0, 1).is_err());
    assert!(fs.replace_snapshot().is_err());
    assert!(fs.sync_dir().is_err());
    assert_eq!(fs.current(FileKind::Data).unwrap(), b"d");
    assert_eq!(fs.durable(FileKind::Data), None);
}

#[test]
fn after_the_crash_point_every_call_fails_with_no_effect() {
    let fs = MemFs::new();
    fs.die_after(2);
    fs.write_at(FileKind::Data, 0, b"1").unwrap();
    assert_eq!(
        fs.read(FileKind::Data).unwrap().unwrap(),
        b"1",
        "reads are no crash point"
    );
    fs.sync_data(FileKind::Data).unwrap();
    assert!(!fs.dead());
    assert!(fs.write_at(FileKind::Data, 0, b"2").is_err());
    assert!(fs.dead());
    assert!(
        fs.read(FileKind::Data).is_err(),
        "a dead process reads nothing either"
    );
    assert!(fs.sync_dir().is_err());
    assert_eq!(fs.mutations(), 2);
    assert_eq!(fs.current(FileKind::Data).unwrap(), b"1");
    let restarted = fs.crash(Crash::Process);
    assert!(!restarted.dead());
    assert_eq!(restarted.mutations(), 0);
    restarted.write_at(FileKind::Data, 0, b"3").unwrap();
}

#[test]
fn syncing_or_punching_a_missing_file_is_not_found() {
    let fs = MemFs::new();
    assert_eq!(
        fs.sync_data(FileKind::Journal).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(fs.punch_data(0, 1).unwrap_err().kind(), ErrorKind::NotFound);
    fs.set_len(FileKind::Data, 4).unwrap();
    fs.punch_data(2, 100).unwrap();
    fs.punch_data(9, 1).unwrap();
    assert_eq!(
        fs.current(FileKind::Data).unwrap(),
        [0; 4],
        "a punch keeps the size"
    );
}

#[test]
fn corruption_changes_the_durable_and_the_current_bytes_alike() {
    let fs = MemFs::new();
    fs.write_at(FileKind::Data, 0, &[0x0f; 2]).unwrap();
    fs.sync_data(FileKind::Data).unwrap();
    fs.sync_dir().unwrap();
    fs.corrupt(FileKind::Data, 1, 0xff);
    fs.corrupt(FileKind::Data, 5, 0xff); // past the end: nothing
    fs.corrupt(FileKind::Journal, 0, 0xff); // no such file: nothing
    assert_eq!(fs.current(FileKind::Data).unwrap(), [0x0f, 0xf0]);
    assert_eq!(fs.durable(FileKind::Data).unwrap(), [0x0f, 0xf0]);
    assert_eq!(fs.current(FileKind::Journal), None);
}

#[test]
fn only_a_read_is_not_a_mutation() {
    for op in [
        Op::WriteAt,
        Op::SetLen,
        Op::SyncData,
        Op::PunchData,
        Op::ReplaceSnapshot,
        Op::SyncDir,
    ] {
        assert!(op.mutates(), "{op:?}");
    }
    assert!(!Op::Read.mutates());
    assert_eq!(
        FileKind::ALL.map(FileKind::suffix),
        [".pack", ".jnl", ".meta", ".meta.tmp"]
    );
}

#[test]
fn a_torn_empty_write_keeps_nothing_and_breaks_nothing() {
    let fs = MemFs::new();
    fs.write_at(FileKind::Journal, 0, b"").unwrap();
    for seed in 0..16 {
        let after = fs.crash(Crash::Power(Keep::Seeded(seed)));
        assert!(
            after
                .current(FileKind::Journal)
                .is_none_or(|b| b.is_empty()),
            "seed {seed}"
        );
    }
}
