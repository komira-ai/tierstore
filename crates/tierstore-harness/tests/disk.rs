//! The crash semantics of the fake disk, its fault injection and accounting, and the crash-point
//! driver on a small commit protocol (section 14: "Crash between pwrite and journal append",
//! "Crash after a batched PUNCH fsync", "Destage commit is atomic under a crash at every step",
//! "Pack write on a cold drive: crash before rename leaves only .up").

use tierstore_harness::disk::{CrashPoint, DiskError, FakeDisk, Fault, OpKind, crash_points};

fn disk_with(path: &str, bytes: &[u8]) -> FakeDisk {
    let setup = || -> Result<FakeDisk, DiskError> {
        let d = FakeDisk::new();
        d.create(path)?;
        d.write_at(path, 0, bytes)?;
        d.fdatasync(path)?;
        d.fsync_dir(path.rsplit_once('/').map_or("", |(dir, _)| dir))?;
        Ok(d)
    };
    setup().unwrap_or_else(|e| panic!("setup of the starting disk failed: {e}"))
}

/// Reads, writes and sizes work on volatile contents: writes past the end pad with zeros, reads
/// past the end are short, `set_len` cuts and pads.
///
/// Catches: a write that does not extend, or a read that panics at the end of the file.
#[test]
fn reads_and_writes_see_volatile_contents() {
    let d = FakeDisk::new();
    d.create("seg/a").unwrap();
    d.write_at("seg/a", 2, b"xy").unwrap();
    assert_eq!(d.read_all("seg/a").unwrap(), b"\0\0xy");
    assert_eq!(d.len("seg/a").unwrap(), 4);
    d.write_at("seg/a", 1, b"ab").unwrap();
    assert_eq!(d.read_all("seg/a").unwrap(), b"\0aby");
    assert_eq!(d.read_at("seg/a", 1, 2).unwrap(), b"ab");
    assert_eq!(d.read_at("seg/a", 3, 10).unwrap(), b"y");
    assert_eq!(d.read_at("seg/a", 9, 2).unwrap(), b"");
    d.set_len("seg/a", 2).unwrap();
    assert_eq!(d.read_all("seg/a").unwrap(), b"\0a");
    d.set_len("seg/a", 4).unwrap();
    assert_eq!(d.read_all("seg/a").unwrap(), b"\0a\0\0");
}

/// A punched hole reads as zeros and keeps the size; a range past the end is clamped.
#[test]
fn punch_hole_zeroes_and_keeps_the_size() {
    let d = FakeDisk::new();
    d.create("f").unwrap();
    d.write_at("f", 0, b"abcdef").unwrap();
    d.punch_hole("f", 1, 2).unwrap();
    assert_eq!(d.read_all("f").unwrap(), b"a\0\0def");
    d.punch_hole("f", 4, 100).unwrap();
    assert_eq!(d.read_all("f").unwrap(), b"a\0\0d\0\0");
    d.punch_hole("f", 50, 1).unwrap();
    assert_eq!(d.len("f").unwrap(), 6);
}

/// Missing and existing paths fail with their errors, and each error converts to an io error
/// (EIO to raw OS error 5).
#[test]
fn path_errors_and_io_error_conversion() {
    let d = FakeDisk::new();
    assert_eq!(d.len("nope"), Err(DiskError::NotFound("nope".into())));
    assert_eq!(
        d.unlink("a/nope"),
        Err(DiskError::NotFound("a/nope".into()))
    );
    assert_eq!(
        d.rename("nope", "x", false),
        Err(DiskError::NotFound("nope".into()))
    );
    assert!(!d.exists("nope").unwrap());
    d.create("a/f").unwrap();
    assert!(d.exists("a/f").unwrap());
    assert_eq!(d.create("a/f"), Err(DiskError::Exists("a/f".into())));

    let io: std::io::Error = DiskError::Eio {
        op: OpKind::WriteAt,
        path: "p".into(),
    }
    .into();
    assert_eq!(io.raw_os_error(), Some(5));
    let io: std::io::Error = DiskError::NotFound("p".into()).into();
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    let io: std::io::Error = DiskError::Exists("p".into()).into();
    assert_eq!(io.kind(), std::io::ErrorKind::AlreadyExists);
    let io: std::io::Error = DiskError::Crashed.into();
    assert_eq!(io.to_string(), "the disk has crashed");
    assert_eq!(
        DiskError::Eio {
            op: OpKind::Fdatasync,
            path: "p".into()
        }
        .to_string(),
        "EIO in Fdatasync on \"p\""
    );
}

/// `list` returns the sorted paths under a prefix, root files and nested directories alike.
#[test]
fn list_returns_sorted_paths_under_a_prefix() {
    let d = FakeDisk::new();
    for p in ["seg/2", "seg/1", "top", "other/x"] {
        d.create(p).unwrap();
    }
    assert_eq!(d.list("seg/").unwrap(), ["seg/1", "seg/2"]);
    assert_eq!(d.list("").unwrap(), ["other/x", "seg/1", "seg/2", "top"]);
    assert_eq!(d.list("t").unwrap(), ["top"]);
}

/// Written data that was never synced is lost by a crash and restart.
///
/// Catches: a disk whose restart keeps volatile data.
#[test]
fn an_unsynced_write_is_lost() {
    let d = disk_with("f", b"old");
    d.write_at("f", 0, b"new").unwrap();
    d.crash();
    assert!(d.is_crashed());
    assert_eq!(d.read_all("f"), Err(DiskError::Crashed));
    d.restart();
    assert!(!d.is_crashed());
    assert_eq!(d.read_all("f").unwrap(), b"old");
}

/// Data synced into a file whose directory entry was never synced is gone after a restart; once
/// the directory is synced the file stays.
#[test]
fn synced_data_of_an_unsynced_create_is_lost() {
    let d = FakeDisk::new();
    d.create("seg/f").unwrap();
    d.write_at("seg/f", 0, b"data").unwrap();
    d.fdatasync("seg/f").unwrap();
    let kept = d.fork();
    d.crash();
    d.restart();
    assert!(!d.exists("seg/f").unwrap());
    assert_eq!(
        d.read_all("seg/f"),
        Err(DiskError::NotFound("seg/f".into()))
    );

    kept.fsync_dir("seg").unwrap();
    kept.crash();
    kept.restart();
    assert_eq!(kept.read_all("seg/f").unwrap(), b"data");
}

/// A durable name whose contents were never synced holds the (empty) durable contents.
#[test]
fn a_durable_name_keeps_only_durable_contents() {
    let d = FakeDisk::new();
    d.create("f").unwrap();
    d.fsync_dir("").unwrap();
    d.write_at("f", 0, b"abc").unwrap();
    d.crash();
    d.restart();
    assert_eq!(d.read_all("f").unwrap(), b"");
}

/// A size change and a punched hole are volatile until the file is synced.
///
/// Catches: a punch that is durable without its fdatasync (section 14: "Crash after a batched
/// PUNCH fsync" needs the punch to be undone when the fsync did not happen).
#[test]
fn set_len_and_punch_are_durable_only_after_fdatasync() {
    let d = disk_with("f", b"abcdef");
    d.punch_hole("f", 0, 3).unwrap();
    d.set_len("f", 9).unwrap();
    let synced = d.fork();
    d.crash();
    d.restart();
    assert_eq!(d.read_all("f").unwrap(), b"abcdef");
    synced.fdatasync("f").unwrap();
    synced.crash();
    synced.restart();
    assert_eq!(synced.read_all("f").unwrap(), b"\0\0\0def\0\0\0");
}

/// A rename is undone by a crash unless its directory was synced; synced, it stays.
#[test]
fn a_rename_needs_fsync_dir_to_survive() {
    let d = disk_with("d/t.up", b"pack");
    d.rename("d/t.up", "d/t", false).unwrap();
    assert_eq!(d.read_all("d/t").unwrap(), b"pack");
    let synced = d.fork();
    d.crash();
    d.restart();
    assert!(d.exists("d/t.up").unwrap());
    assert!(!d.exists("d/t").unwrap());

    synced.fsync_dir("d").unwrap();
    synced.crash();
    synced.restart();
    assert!(!synced.exists("d/t.up").unwrap());
    assert_eq!(synced.read_all("d/t").unwrap(), b"pack");
}

/// A rename moves a file between directories, replaces an existing target unless `noreplace`,
/// and refuses an existing target with `Exists` (renameat2 `RENAME_NOREPLACE`) without effect.
#[test]
fn rename_noreplace_refuses_an_existing_target() {
    let d = FakeDisk::new();
    d.create("a/x").unwrap();
    d.write_at("a/x", 0, b"X").unwrap();
    d.create("b/y").unwrap();
    d.write_at("b/y", 0, b"Y").unwrap();
    assert_eq!(
        d.rename("a/x", "b/y", true),
        Err(DiskError::Exists("b/y".into()))
    );
    assert_eq!(d.read_all("a/x").unwrap(), b"X");
    assert_eq!(d.read_all("b/y").unwrap(), b"Y");
    d.rename("a/x", "b/y", false).unwrap();
    assert!(!d.exists("a/x").unwrap());
    assert_eq!(d.read_all("b/y").unwrap(), b"X");
    d.rename("b/y", "b/z", true).unwrap();
    assert_eq!(d.list("b/").unwrap(), ["b/z"]);
}

/// An unlink is undone by a crash unless its directory was synced.
#[test]
fn an_unlink_needs_fsync_dir_to_survive() {
    let d = disk_with("f", b"keep");
    d.unlink("f").unwrap();
    assert!(!d.exists("f").unwrap());
    let synced = d.fork();
    d.crash();
    d.restart();
    assert_eq!(d.read_all("f").unwrap(), b"keep");

    synced.fsync_dir("").unwrap();
    synced.crash();
    synced.restart();
    assert!(!synced.exists("f").unwrap());
}

/// `CrashBefore` leaves the op without effect and crashes the disk; `CrashAfter` applies the op,
/// crashes, and still reports an error. Both stay crashed until restart.
#[test]
fn crash_faults_before_and_after_an_op() {
    let d = FakeDisk::new();
    d.create("f").unwrap();
    d.inject(OpKind::FsyncDir, 0, Fault::CrashBefore);
    assert_eq!(d.fsync_dir(""), Err(DiskError::Crashed));
    assert!(d.is_crashed());
    assert_eq!(d.exists("f"), Err(DiskError::Crashed));
    d.restart();
    assert!(!d.exists("f").unwrap(), "the directory sync did not happen");

    d.create("f").unwrap();
    d.inject(OpKind::FsyncDir, 0, Fault::CrashAfter);
    assert_eq!(d.fsync_dir(""), Err(DiskError::Crashed));
    assert!(d.is_crashed());
    d.restart();
    assert!(
        d.exists("f").unwrap(),
        "the directory sync happened before the crash"
    );
}

/// A `CrashAfter` fault applies its op: a synced write is durable although the call failed.
#[test]
fn a_crash_after_an_fdatasync_keeps_the_data() {
    let d = FakeDisk::new();
    d.create("f").unwrap();
    d.fsync_dir("").unwrap();
    d.write_at("f", 0, b"abc").unwrap();
    d.inject(OpKind::Fdatasync, 0, Fault::CrashAfter);
    assert_eq!(d.fdatasync("f"), Err(DiskError::Crashed));
    d.restart();
    assert_eq!(d.read_all("f").unwrap(), b"abc");

    // The same fault before the sync loses the data.
    d.write_at("f", 0, b"xyz").unwrap();
    d.inject(OpKind::Fdatasync, 0, Fault::CrashBefore);
    assert_eq!(d.fdatasync("f"), Err(DiskError::Crashed));
    d.restart();
    assert_eq!(d.read_all("f").unwrap(), b"abc");
}

/// An `Eio` fault fails exactly the nth op of its kind with an EIO that names the op and path,
/// without effect, and leaves the disk up; other kinds and other ops are untouched.
#[test]
fn eio_fault_fails_the_nth_op_of_a_kind() {
    let d = FakeDisk::new();
    d.create("f").unwrap();
    d.inject(OpKind::WriteAt, 1, Fault::Eio);
    d.write_at("f", 0, b"a").unwrap();
    assert_eq!(d.len("f").unwrap(), 1);
    assert_eq!(
        d.write_at("f", 0, b"b"),
        Err(DiskError::Eio {
            op: OpKind::WriteAt,
            path: "f".into()
        })
    );
    assert!(!d.is_crashed());
    assert_eq!(d.read_all("f").unwrap(), b"a");
    d.write_at("f", 0, b"c").unwrap();
    assert_eq!(d.read_all("f").unwrap(), b"c");
}

/// `inject_any` counts ops of every kind, and two faults due at the same op fire one per op in the
/// order injected.
#[test]
fn inject_any_counts_every_kind_and_faults_queue_up() {
    let d = FakeDisk::new();
    d.inject_any(2, Fault::Eio);
    d.inject_any(2, Fault::CrashBefore);
    d.create("f").unwrap();
    d.len("f").unwrap();
    assert_eq!(
        d.exists("f"),
        Err(DiskError::Eio {
            op: OpKind::Exists,
            path: "f".into()
        })
    );
    assert!(!d.is_crashed());
    assert_eq!(d.list(""), Err(DiskError::Crashed));
    assert!(d.is_crashed());
    d.restart();
    assert!(
        d.list("").unwrap().is_empty(),
        "restart dropped any pending fault"
    );
}

/// The op log lists every op attempted while up, in order, with its path (the source of a
/// rename, the directory of an fsync_dir); fdatasyncs are counted per path and in total, and
/// fsync_dirs in total; failed and crashed ops are not counted as synced.
#[test]
fn the_op_log_and_sync_counts() {
    let d = FakeDisk::new();
    d.create("d/a").unwrap();
    d.write_at("d/a", 0, b"1").unwrap();
    d.fdatasync("d/a").unwrap();
    d.fdatasync("d/a").unwrap();
    d.create("d/b").unwrap();
    d.fdatasync("d/b").unwrap();
    assert_eq!(
        d.fdatasync("d/none"),
        Err(DiskError::NotFound("d/none".into()))
    );
    d.rename("d/b", "d/c", false).unwrap();
    d.fsync_dir("d").unwrap();
    d.unlink("d/c").unwrap();
    d.set_len("d/a", 0).unwrap();
    d.punch_hole("d/a", 0, 0).unwrap();
    d.read_at("d/a", 0, 1).unwrap();
    d.list("d").unwrap();
    assert_eq!(d.fdatasyncs("d/a"), 2);
    assert_eq!(d.fdatasyncs("d/b"), 1);
    assert_eq!(d.fdatasyncs("d/none"), 0);
    assert_eq!(d.total_fdatasyncs(), 3);
    assert_eq!(d.fsync_dir_count(), 1);
    let kinds: Vec<OpKind> = d.ops().iter().map(|(k, _)| *k).collect();
    assert_eq!(
        kinds,
        [
            OpKind::Create,
            OpKind::WriteAt,
            OpKind::Fdatasync,
            OpKind::Fdatasync,
            OpKind::Create,
            OpKind::Fdatasync,
            OpKind::Fdatasync,
            OpKind::Rename,
            OpKind::FsyncDir,
            OpKind::Unlink,
            OpKind::SetLen,
            OpKind::PunchHole,
            OpKind::ReadAt,
            OpKind::List,
        ]
    );
    assert_eq!(d.ops()[7].1, "d/b");
    assert_eq!(d.ops()[8].1, "d");
    d.crash();
    let _ = d.len("d/a");
    assert_eq!(
        d.ops().len(),
        14,
        "an op refused by a crashed disk is not logged"
    );
}

/// A fork is a copy: changes to it do not reach the original, a clone is the same disk.
#[test]
fn a_fork_is_a_copy_and_a_clone_is_shared() {
    let d = disk_with("f", b"a");
    let alias = d.clone();
    let copy = d.fork();
    copy.write_at("f", 0, b"z").unwrap();
    alias.write_at("f", 0, b"q").unwrap();
    assert_eq!(d.read_all("f").unwrap(), b"q");
    assert_eq!(copy.read_all("f").unwrap(), b"z");
    assert_eq!(
        copy.fdatasyncs("f"),
        1,
        "the copy starts with the counts of the original"
    );
}

/// The commit protocol under test: write a temporary file, make it durable, rename it over the
/// target, make the directory durable.
fn commit_safely(d: &FakeDisk, new: &[u8]) {
    let _ = (|| -> Result<(), DiskError> {
        d.create("db/tmp")?;
        d.write_at("db/tmp", 0, new)?;
        d.fdatasync("db/tmp")?;
        d.rename("db/tmp", "db/target", false)?;
        d.fsync_dir("db")
    })();
}

/// The same protocol with the data sync left out: the rename can reach the disk before the data.
fn commit_unsafely(d: &FakeDisk, new: &[u8]) {
    let _ = (|| -> Result<(), DiskError> {
        d.create("db/tmp")?;
        d.write_at("db/tmp", 0, new)?;
        d.rename("db/tmp", "db/target", false)?;
        d.fsync_dir("db")
    })();
}

/// Section 14: "Destage commit is atomic under a crash at every step": after a crash before or
/// after each op of write-temp, fdatasync, rename, fsync_dir, the target holds the old bytes or
/// the new bytes whole, and the driver tried every op (4 ops, 2 points each).
#[test]
fn a_synced_rename_commit_is_atomic_at_every_crash_point() {
    let start = disk_with("db/target", b"OLD-OLD");
    let mut seen = Vec::new();
    let tried = crash_points(
        &start,
        true,
        |d| commit_safely(d, b"NEW-NEW"),
        |p, d| {
            let bytes = d.read_all("db/target").unwrap();
            assert!(
                bytes == b"OLD-OLD" || bytes == b"NEW-NEW",
                "{p:?}: {bytes:?}"
            );
            seen.push((p.index, p.after, bytes));
        },
    );
    assert_eq!(
        tried, 10,
        "five ops (create, write, fdatasync, rename, fsync_dir), two points each"
    );
    assert_eq!(seen.len(), 10);
    // Before the directory sync is durable the old bytes stay; after it, the new ones.
    assert_eq!(seen[0], (0, false, b"OLD-OLD".to_vec()));
    assert_eq!(seen[9], (4, true, b"NEW-NEW".to_vec()));
}

/// A protocol that renames before syncing the data is caught: some crash point leaves a target
/// that is neither the old nor the new bytes (here, the durable directory entry of the new inode
/// with its data never synced: empty).
///
/// Catches: a crash-point driver that never lands between the directory sync and the data.
#[test]
fn a_commit_without_the_data_sync_is_caught() {
    let start = disk_with("db/target", b"OLD-OLD");
    let mut torn = Vec::new();
    let tried = crash_points(
        &start,
        true,
        |d| commit_unsafely(d, b"NEW-NEW"),
        |p, d| {
            let bytes = d.read_all("db/target").unwrap();
            if bytes != b"OLD-OLD" && bytes != b"NEW-NEW" {
                torn.push((p, bytes));
            }
        },
    );
    assert_eq!(tried, 8);
    assert_eq!(
        torn,
        [(
            CrashPoint {
                index: 3,
                after: true
            },
            Vec::new()
        )]
    );
}

/// With `also_after` off only before-points are tried; a run that makes no ops completes at once
/// with zero crash points and never calls the checker.
#[test]
fn crash_points_stops_at_the_first_run_without_a_crash() {
    let start = FakeDisk::new();
    let tried = crash_points(
        &start,
        false,
        |_| {},
        |_, _| panic!("no crash point expected"),
    );
    assert_eq!(tried, 0);
    let mut points = Vec::new();
    let tried = crash_points(
        &start,
        false,
        |d| {
            let _ = d.create("a");
            let _ = d.create("b");
        },
        |p, _| points.push(p),
    );
    assert_eq!(tried, 2);
    assert_eq!(
        points,
        [
            CrashPoint {
                index: 0,
                after: false
            },
            CrashPoint {
                index: 1,
                after: false
            }
        ]
    );
    assert!(
        start.list("").unwrap().is_empty(),
        "each run used its own copy"
    );
}
