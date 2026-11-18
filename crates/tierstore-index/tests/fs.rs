//! The `Fs` trait on a real directory and on the fake, and the fake's crash and failure model.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use std::io::ErrorKind;

use tierstore_index::fs::fake::{FakeFs, FsOp, Torn};
use tierstore_index::fs::{DirFs, Fs};
use tierstore_testkit::dir::TestDir;

/// Runs `f` on a fresh `DirFs` and on a fresh `FakeFs`.
fn on_both(name: &str, f: impl Fn(&dyn Fs)) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    f(&DirFs::new(dir.path()));
    f(&FakeFs::new());
}

fn kind<T: std::fmt::Debug>(r: std::io::Result<T>) -> ErrorKind {
    r.expect_err("the call must fail").kind()
}

#[test]
fn append_creates_the_file_and_its_parents_and_then_adds_to_it() {
    on_both("fs-append", |fs| {
        fs.append("a/b/c.log", b"hello").unwrap();
        fs.append("a/b/c.log", b" world").unwrap();
        assert_eq!(fs.len("a/b/c.log").unwrap(), 11);
        assert_eq!(fs.read_at("a/b/c.log", 0, 100).unwrap(), b"hello world");
    });
}

#[test]
fn read_at_returns_the_range_and_fewer_bytes_at_the_end() {
    on_both("fs-read", |fs| {
        fs.append("f", b"0123456789").unwrap();
        assert_eq!(fs.read_at("f", 2, 3).unwrap(), b"234");
        assert_eq!(fs.read_at("f", 8, 10).unwrap(), b"89");
        assert_eq!(fs.read_at("f", 10, 4).unwrap(), b"");
        assert_eq!(fs.read_at("f", 99, 4).unwrap(), b"");
        assert_eq!(fs.read_at("f", 0, 0).unwrap(), b"");
        assert_eq!(fs.read_at("f", 0, u64::MAX).unwrap(), b"0123456789");
    });
}

#[test]
fn list_gives_sorted_names_of_files_and_directories_and_nothing_for_a_missing_directory() {
    on_both("fs-list", |fs| {
        fs.append("tomb/2/1.log", b"x").unwrap();
        fs.append("tomb/10/1.log", b"x").unwrap();
        fs.append("tomb/10/5.log", b"x").unwrap();
        fs.append("top", b"x").unwrap();
        assert_eq!(fs.list("").unwrap(), ["tomb", "top"]);
        assert_eq!(fs.list("tomb").unwrap(), ["10", "2"]);
        assert_eq!(fs.list("tomb/10").unwrap(), ["1.log", "5.log"]);
        assert_eq!(fs.list("nothing").unwrap(), Vec::<String>::new());
        assert_eq!(fs.list("nothing/below").unwrap(), Vec::<String>::new());
    });
}

#[test]
fn truncate_cuts_and_zero_extends() {
    on_both("fs-truncate", |fs| {
        fs.append("f", b"abcdef").unwrap();
        fs.truncate("f", 2).unwrap();
        assert_eq!(fs.read_at("f", 0, 10).unwrap(), b"ab");
        fs.truncate("f", 4).unwrap();
        assert_eq!(fs.read_at("f", 0, 10).unwrap(), b"ab\0\0");
        assert_eq!(fs.len("f").unwrap(), 4);
    });
}

#[test]
fn sync_succeeds_on_a_file_and_remove_deletes_it() {
    on_both("fs-remove", |fs| {
        fs.append("d/f", b"abc").unwrap();
        fs.sync("d/f").unwrap();
        fs.append("g", b"x").unwrap();
        fs.sync("g").unwrap();
        fs.remove("d/f").unwrap();
        assert_eq!(kind(fs.len("d/f")), ErrorKind::NotFound);
        assert_eq!(fs.list("d").unwrap(), Vec::<String>::new());
        assert!(fs.list("").unwrap().contains(&"g".to_owned()));
    });
}

#[test]
fn a_missing_file_is_not_found_for_len_read_truncate_sync_and_remove() {
    on_both("fs-missing", |fs| {
        assert_eq!(kind(fs.len("m")), ErrorKind::NotFound);
        assert_eq!(kind(fs.read_at("m", 0, 1)), ErrorKind::NotFound);
        assert_eq!(kind(fs.truncate("m", 0)), ErrorKind::NotFound);
        assert_eq!(kind(fs.sync("m")), ErrorKind::NotFound);
        assert_eq!(kind(fs.remove("m")), ErrorKind::NotFound);
    });
}

#[test]
fn a_path_that_is_not_relative_and_plain_is_invalid_input_for_every_method() {
    on_both("fs-paths", |fs| {
        for bad in ["", "/abs", "a/../b", "..", "a//b", "./a", "a/", "a/."] {
            assert_eq!(kind(fs.len(bad)), ErrorKind::InvalidInput, "len {bad:?}");
            assert_eq!(kind(fs.read_at(bad, 0, 1)), ErrorKind::InvalidInput);
            assert_eq!(kind(fs.append(bad, b"x")), ErrorKind::InvalidInput);
            assert_eq!(kind(fs.truncate(bad, 0)), ErrorKind::InvalidInput);
            assert_eq!(kind(fs.sync(bad)), ErrorKind::InvalidInput);
            assert_eq!(kind(fs.remove(bad)), ErrorKind::InvalidInput);
        }
        for bad in ["/abs", "a/../b", "..", "a//b", "a/"] {
            assert_eq!(kind(fs.list(bad)), ErrorKind::InvalidInput, "list {bad:?}");
        }
        assert_eq!(fs.list("").unwrap(), Vec::<String>::new());
    });
}

#[test]
fn dir_fs_lists_a_file_as_an_error_not_as_a_missing_directory() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fs-list-file");
    let fs = DirFs::new(dir.path());
    fs.append("f", b"x").unwrap();
    assert_ne!(kind(fs.list("f")), ErrorKind::NotFound);
}

#[test]
fn dir_fs_files_are_real_files_under_the_root() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "fs-real");
    let fs = DirFs::new(dir.path());
    fs.append("x/y.log", b"abc").unwrap();
    assert_eq!(std::fs::read(dir.path().join("x/y.log")).unwrap(), b"abc");
}

#[test]
fn crash_lost_keeps_synced_bytes_and_drops_the_unsynced_tail() {
    let fs = FakeFs::new();
    fs.append("f", b"synced").unwrap();
    fs.sync("f").unwrap();
    fs.append("f", b"-tail").unwrap();
    assert_eq!(fs.bytes("f").unwrap(), b"synced-tail");
    let after = fs.crash(Torn::Lost);
    assert_eq!(after.bytes("f").unwrap(), b"synced");
}

#[test]
fn crash_kept_keeps_the_whole_unsynced_tail() {
    let fs = FakeFs::new();
    fs.append("f", b"synced").unwrap();
    fs.sync("f").unwrap();
    fs.append("f", b"-tail").unwrap();
    assert_eq!(fs.crash(Torn::Kept).bytes("f").unwrap(), b"synced-tail");
}

#[test]
fn crash_prefix_keeps_the_first_n_unsynced_bytes_and_no_more_than_there_are() {
    let fs = FakeFs::new();
    fs.append("f", b"synced").unwrap();
    fs.sync("f").unwrap();
    fs.append("f", b"-tail").unwrap();
    assert_eq!(fs.crash(Torn::Prefix(0)).bytes("f").unwrap(), b"synced");
    assert_eq!(fs.crash(Torn::Prefix(2)).bytes("f").unwrap(), b"synced-t");
    assert_eq!(
        fs.crash(Torn::Prefix(5)).bytes("f").unwrap(),
        b"synced-tail"
    );
    assert_eq!(
        fs.crash(Torn::Prefix(99)).bytes("f").unwrap(),
        b"synced-tail"
    );
    assert_eq!(
        fs.crash(Torn::Prefix(u64::MAX)).bytes("f").unwrap(),
        b"synced-tail"
    );
}

#[test]
fn a_file_created_and_never_synced_vanishes_in_every_crash() {
    let fs = FakeFs::new();
    fs.append("new", b"bytes").unwrap();
    fs.append("synced", b"bytes").unwrap();
    fs.sync("synced").unwrap();
    for torn in [Torn::Lost, Torn::Kept, Torn::Prefix(3)] {
        let after = fs.crash(torn);
        assert_eq!(after.bytes("new"), None, "{torn:?}");
        assert!(after.bytes("synced").is_some(), "{torn:?}");
        assert_eq!(after.list("").unwrap(), ["synced"]);
    }
}

#[test]
fn put_makes_a_durable_file_and_replaces_an_earlier_one() {
    let fs = FakeFs::new();
    fs.put("f", b"first");
    fs.put("f", b"second");
    assert_eq!(fs.crash(Torn::Lost).bytes("f").unwrap(), b"second");
    assert_eq!(fs.bytes("none"), None);
    assert_eq!(fs.calls(FsOp::Append), 0, "put is not a counted call");
}

#[test]
fn a_truncate_below_the_durable_length_survives_a_crash_as_the_shorter_file() {
    let fs = FakeFs::new();
    fs.put("f", b"0123456789");
    fs.truncate("f", 4).unwrap();
    assert_eq!(fs.crash(Torn::Lost).bytes("f").unwrap(), b"0123");
    assert_eq!(fs.crash(Torn::Kept).bytes("f").unwrap(), b"0123");
    fs.truncate("f", 6).unwrap();
    assert_eq!(fs.crash(Torn::Lost).bytes("f").unwrap(), b"0123");
    assert_eq!(fs.crash(Torn::Kept).bytes("f").unwrap(), b"0123\0\0");
}

#[test]
fn a_crashed_fs_is_durable_and_has_no_failures_or_call_counts() {
    let fs = FakeFs::new();
    fs.append("f", b"abc").unwrap();
    fs.sync("f").unwrap();
    fs.fail(FsOp::Len, "", 5);
    let after = fs.crash(Torn::Lost);
    assert_eq!(after.calls(FsOp::Append), 0);
    assert_eq!(after.len("f").unwrap(), 3);
    assert_eq!(fs.calls(FsOp::Sync), 1);
}

#[test]
fn a_failure_hits_only_the_next_count_calls_of_its_op_on_matching_paths() {
    for (op, call) in [
        (FsOp::List, 0),
        (FsOp::Len, 1),
        (FsOp::Read, 2),
        (FsOp::Append, 3),
        (FsOp::Truncate, 4),
        (FsOp::Sync, 5),
        (FsOp::Remove, 6),
    ] {
        let fs = FakeFs::new();
        fs.put("keep/f", b"abc");
        fs.put("other/f", b"abc");
        fs.fail(op, "keep", 2);
        let run = |path: &str| -> std::io::Result<()> {
            let dir = path.split('/').next().unwrap();
            match call {
                0 => fs.list(dir).map(|_| ()),
                1 => fs.len(path).map(|_| ()),
                2 => fs.read_at(path, 0, 1).map(|_| ()),
                3 => fs.append(path, b"x"),
                4 => fs.truncate(path, 1),
                5 => fs.sync(path),
                _ => fs.remove(path),
            }
        };
        assert_eq!(kind(run("keep/f")), ErrorKind::Other, "{op:?} first");
        assert_eq!(kind(run("keep/f")), ErrorKind::Other, "{op:?} second");
        run("other/f").unwrap_or_else(|e| panic!("{op:?} on a path outside the prefix: {e}"));
        if op == FsOp::Remove {
            fs.put("keep/f", b"abc");
        }
        run("keep/f").unwrap_or_else(|e| panic!("{op:?} after the count: {e}"));
        assert_eq!(fs.calls(op), 4, "{op:?}: failed calls are counted too");
    }
}

#[test]
fn a_failed_call_changes_nothing() {
    let fs = FakeFs::new();
    fs.put("f", b"abc");
    fs.fail(FsOp::Append, "", 1);
    fs.fail(FsOp::Truncate, "", 1);
    fs.fail(FsOp::Remove, "", 1);
    fs.append("f", b"x").unwrap_err();
    fs.truncate("f", 0).unwrap_err();
    fs.remove("f").unwrap_err();
    assert_eq!(fs.bytes("f").unwrap(), b"abc");
}

#[test]
fn a_failure_of_another_op_does_not_fire_on_this_one() {
    let fs = FakeFs::new();
    fs.put("f", b"abc");
    fs.fail(FsOp::Sync, "", 1);
    assert_eq!(fs.len("f").unwrap(), 3);
    fs.sync("f").unwrap_err();
    fs.sync("f").unwrap();
}

#[test]
fn calls_count_each_op_separately_including_refused_paths() {
    let fs = FakeFs::new();
    fs.append("f", b"abc").unwrap();
    fs.append("f", b"abc").unwrap();
    fs.len("f").unwrap();
    fs.read_at("f", 0, 1).unwrap();
    fs.list("").unwrap();
    fs.sync("f").unwrap();
    fs.truncate("f", 1).unwrap();
    fs.remove("f").unwrap();
    fs.len("../x").unwrap_err();
    let counts: Vec<u64> = [
        FsOp::List,
        FsOp::Len,
        FsOp::Read,
        FsOp::Append,
        FsOp::Truncate,
        FsOp::Sync,
        FsOp::Remove,
    ]
    .iter()
    .map(|op| fs.calls(*op))
    .collect();
    assert_eq!(counts, [1, 2, 1, 2, 1, 1, 1]);
}

#[test]
fn an_unsynced_file_is_listed_and_readable_before_the_crash() {
    let fs = FakeFs::new();
    fs.append("d/new", b"abc").unwrap();
    assert_eq!(fs.list("d").unwrap(), ["new"]);
    assert_eq!(fs.read_at("d/new", 0, 9).unwrap(), b"abc");
}
