//! `du`: walks of a small tree, the depth, links, devices, errors and the report's file.

use std::cell::Cell;

use tierstore_testkit::dir::TestDir;

use super::*;

/// A writer that refuses everything, for the error paths of what `run` prints.
struct Refuses;

impl Write for Refuses {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("refused"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `root/a/f1` (100 bytes), `root/a/b/f2` (5000 bytes), `root/c` (10 bytes), `root/link -> a`.
fn tree(name: &str) -> TestDir {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/f1"), vec![1u8; 100]).unwrap();
    std::fs::write(root.join("a/b/f2"), vec![2u8; 5000]).unwrap();
    std::fs::write(root.join("c"), vec![3u8; 10]).unwrap();
    std::os::unix::fs::symlink("a", root.join("link")).unwrap();
    dir
}

/// The entry of `path` in a report.
fn entry<'a>(report: &'a DuReport, path: &str) -> &'a Entry {
    report
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("no entry {path}"))
}

/// Every entry down to the depth has its files, directories and apparent bytes counted from
/// everything below it, a link is one file of no apparent bytes and is not followed, and the
/// root's allocated bytes are its own blocks plus those of its entries.
#[test]
fn a_walk_counts_what_lies_below_each_entry() {
    let dir = tree("storebench-du-counts");
    let report = walk(dir.path(), 2).unwrap();
    let paths: Vec<&str> = report.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["a", "a/b", "a/f1", "c", "link"]);
    let a = entry(&report, "a");
    assert_eq!((a.files, a.dirs, a.apparent, a.errors), (2, 1, 5100, 0));
    let b = entry(&report, "a/b");
    assert_eq!((b.files, b.dirs, b.apparent), (1, 0, 5000));
    let c = entry(&report, "c");
    assert_eq!((c.files, c.dirs, c.apparent), (1, 0, 10));
    let link = entry(&report, "link");
    assert_eq!((link.files, link.dirs, link.apparent), (1, 0, 0));
    assert_eq!(report.total.path, ".");
    assert_eq!(
        (
            report.total.files,
            report.total.dirs,
            report.total.apparent,
            report.total.errors
        ),
        (4, 2, 5110, 0)
    );
    assert!(b.allocated >= 5000 && b.allocated.is_multiple_of(512));
    let own = std::fs::symlink_metadata(dir.path()).unwrap().blocks() * 512;
    let level_one: u64 = ["a", "c", "link"]
        .iter()
        .map(|p| entry(&report, p).allocated)
        .sum();
    assert_eq!(report.total.allocated, own + level_one);
    assert_eq!(report.depth, 2);
    assert_eq!(report.root, dir.path().display().to_string());
}

/// A depth of 1 lists only the entries of the root (their contents still counted), and a
/// depth of 0 lists none but still totals the tree.
#[test]
fn the_depth_bounds_which_entries_get_a_line() {
    let dir = tree("storebench-du-depth");
    let one = walk(dir.path(), 1).unwrap();
    let paths: Vec<&str> = one.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["a", "c", "link"]);
    assert_eq!(entry(&one, "a").files, 2);
    let zero = walk(dir.path(), 0).unwrap();
    assert!(zero.entries.is_empty());
    assert_eq!(zero.total.files, 4);
    assert_eq!(zero.total.dirs, 2);
}

/// A directory on another device is skipped whole: only the root's own files remain.
#[test]
fn a_directory_on_another_device_is_not_entered() {
    let dir = tree("storebench-du-device");
    let report = walk_on(dir.path(), 2, |device| device + 1).unwrap();
    let paths: Vec<&str> = report.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["c", "link"]);
    assert_eq!((report.total.files, report.total.dirs), (2, 0));
}

/// An error is counted and gives no value; a success gives its value and counts nothing.
#[test]
fn an_unreadable_entry_is_counted_and_skipped() {
    let errors = Cell::new(0);
    let dir = tree("storebench-du-note");
    let missing = std::fs::read_dir(dir.path().join("none"));
    assert!(note(missing, &errors).is_none());
    assert_eq!(errors.get(), 1);
    assert_eq!(note(Ok(7), &errors), Some(7));
    assert_eq!(errors.get(), 1);
    let present = note(std::fs::read_dir(std::env::temp_dir()), &errors);
    assert!(present.is_some());
    assert_eq!(errors.get(), 1);
}

/// The file system's size is its blocks times its fragment size as `statvfs` states them, at
/// least what is used; a missing path is an error naming it.
#[test]
fn the_file_system_is_measured() {
    let dir = tree("storebench-du-fs");
    let fs = filesystem(dir.path()).unwrap();
    let s = rustix::fs::statvfs(dir.path()).unwrap();
    assert_eq!(fs.size, s.f_blocks * s.f_frsize);
    assert!(fs.size >= fs.used);
    let missing = dir.path().join("none");
    let e = filesystem(&missing).unwrap_err();
    assert!(
        matches!(
            e,
            Error::Io {
                what: "statvfs",
                ..
            }
        ),
        "{e}"
    );
}

/// A root that is missing or is not a directory is refused, naming it.
#[test]
fn a_root_that_is_not_a_directory_is_refused() {
    let dir = tree("storebench-du-root");
    let missing = dir.path().join("none");
    let e = walk(&missing, 1).unwrap_err();
    assert!(matches!(e, Error::Io { what: "walk", .. }), "{e}");
    let file = dir.path().join("c");
    let e = walk(&file, 1).unwrap_err();
    assert_eq!(
        e.to_string(),
        format!("{} is not a directory", file.display())
    );
}

/// A report written as JSON reads back equal; a file that is not one, and one that is missing,
/// are refused with the path.
#[test]
fn a_report_round_trips_through_its_json_file() {
    let dir = tree("storebench-du-json");
    let report = walk(dir.path(), 2).unwrap();
    let path = dir.path().join("du.json");
    write_json(&path, &report).unwrap();
    assert_eq!(read_json::<DuReport>(&path).unwrap(), report);
    std::fs::write(&path, "{ not json").unwrap();
    let e = read_json::<DuReport>(&path).unwrap_err();
    assert!(matches!(e, Error::Parse { line: 1, .. }), "{e}");
    let e = read_json::<DuReport>(&dir.path().join("none.json")).unwrap_err();
    assert!(matches!(e, Error::Io { what: "read", .. }), "{e}");
    let e = write_json(&dir.path().join("no/such/dir.json"), &report).unwrap_err();
    assert!(matches!(e, Error::Io { what: "write", .. }), "{e}");
}

/// `run` writes the report and one line saying what it found.
#[test]
fn run_writes_the_report_and_a_summary_line() {
    let dir = tree("storebench-du-run");
    let out = dir.path().join("report.json");
    let args = Args {
        root: dir.path().to_path_buf(),
        depth: 1,
        out: out.clone(),
    };
    let mut stdout = Vec::new();
    run(&args, &mut stdout).unwrap();
    let report: DuReport = read_json(&out).unwrap();
    assert_eq!(report.depth, 1);
    assert_eq!(report.entries.len(), 3);
    let line = String::from_utf8(stdout).unwrap();
    assert!(
        line.contains(": 4 files, 2 dirs, 5.11 kB apparent, "),
        "{line}"
    );
    assert!(
        line.ends_with(&format!("0 errors, 3 entries in {}\n", out.display())),
        "{line}"
    );
}

/// `run` fails on a missing root, on a report that cannot be written and on a standard output
/// that refuses.
#[test]
fn run_fails_where_it_cannot_read_or_write() {
    let dir = tree("storebench-du-run-fails");
    let good = Args {
        root: dir.path().to_path_buf(),
        depth: 1,
        out: dir.path().join("report.json"),
    };
    let missing_root = Args {
        root: dir.path().join("none"),
        depth: 1,
        out: good.out.clone(),
    };
    assert!(matches!(
        run(&missing_root, &mut Vec::new()).unwrap_err(),
        Error::Io { what: "walk", .. }
    ));
    let no_out = Args {
        root: good.root.clone(),
        depth: 1,
        out: dir.path().join("no/such/report.json"),
    };
    assert!(matches!(
        run(&no_out, &mut Vec::new()).unwrap_err(),
        Error::Io { what: "write", .. }
    ));
    let e = run(&good, &mut Refuses).unwrap_err();
    assert!(matches!(e, Error::Io { what: "write", .. }), "{e}");
}
