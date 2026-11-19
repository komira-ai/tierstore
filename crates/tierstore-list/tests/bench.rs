//! The L2 benchmark (`examples/l2_bench.rs`), run on a scratch tree and on a saved list: what it
//! reports of the tree (never how long a step took), and the flags it refuses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[path = "../examples/l2_bench.rs"]
#[expect(
    dead_code,
    reason = "the example's main, which runs it from the command line"
)]
mod bench;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use tierstore::TreeList;
use tierstore_testkit::dir::TestDir;

mod support;

const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const USAGE: &str = "usage: l2_bench [--runs N] [--pack-mib M] (--from LIST | DIR...)";
const KIB600: usize = 600 * 1024;

/// Runs the benchmark with `args`: its output, and the message of the error it stopped on.
fn run(args: &[&str]) -> (String, Result<(), String>) {
    let mut out = Vec::new();
    let result = bench::run(args.iter().map(ToString::to_string), &mut out);
    (
        String::from_utf8(out).unwrap(),
        result.map_err(|e| e.to_string()),
    )
}

fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A scratch tree of every kind of entry the walk meets: files (one empty, two of 600 KiB so that
/// packs of 1 MiB hold one each), a link, an empty directory, a nested directory, a `.git`
/// directory and a socket, which the walk leaves out. Its directory, and the directory that holds
/// it, which the test removes on drop.
fn scratch_tree() -> (TestDir, PathBuf) {
    let (holder, sock) = TestDir::for_socket(
        &[&std::env::temp_dir(), Path::new(SCRATCH)],
        "l2-bench",
        "sock",
    );
    let root = holder.path().to_path_buf();
    put(&root.join("a_small"), b"small", 0o644);
    put(&root.join("b_big1"), &vec![b'x'; KIB600], 0o755);
    put(&root.join("c_big2"), &vec![b'y'; KIB600], 0o600);
    put(&root.join("d_empty"), b"", 0o640);
    std::os::unix::fs::symlink("a_small", root.join("e_link")).unwrap();
    fs::create_dir(root.join("f_emptydir")).unwrap();
    fs::create_dir_all(root.join("g_nested/h_inner")).unwrap();
    put(&root.join("g_nested/h_inner/leaf"), b"leaf!!!", 0o600);
    fs::create_dir(root.join(".git")).unwrap();
    put(&root.join(".git/config"), b"history", 0o644);
    let _socket = UnixListener::bind(&sock).unwrap();
    // The listener is dropped here; the socket file stays, which the walk must leave out.
    (holder, root)
}

/// The line of `out` that begins with `label`.
fn line<'a>(out: &'a str, label: &str) -> &'a str {
    out.lines()
        .find(|l| l.starts_with(label))
        .unwrap_or_else(|| panic!("no line {label:?} in {out}"))
}

/// Asserts that a walk of a tree of every kind of entry, in packs of 1 MiB, hashes the five files'
/// bytes, makes a sidecar of 11 entries (the root, the tree's directory, its seven children but
/// `.git` and the socket, the nested directory's child and its leaf) in two packs of one cell each
/// (the second 600 KiB file does not fit in the first), and finds every path it looks up.
#[test]
fn walking_a_tree_reports_its_hashes_shape_and_lookups() {
    let (_holder, root) = scratch_tree();
    let (out, result) = run(&["--runs", "1", "--pack-mib", "1", root.to_str().unwrap()]);
    assert_eq!(result, Ok(()), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    let bytes = 5 + 2 * KIB600 + 7;
    assert!(
        lines[0].starts_with(&format!("hashed {bytes} B: whole files ")),
        "{out}"
    );
    assert!(lines[0].contains(" MB/s), cells "), "{out}");
    assert_eq!(
        lines[1], "entries: 11 (5 files); packs: 2; cells: 2",
        "{out}"
    );
    assert!(lines[2].starts_with("bytes: v2 "), "{out}");
    assert!(lines[3].starts_with("write: the sidecar "), "{out}");
    assert!(lines[4].starts_with("open "), "{out}");
    assert!(lines[4].contains("; tree id checked "), "{out}");
    let walk = line(&out, "walk ");
    assert!(
        walk.contains(&format!(" (11 entries, {bytes} B); ")),
        "{out}"
    );
    assert!(
        walk.contains(" (10 of 10); open and every pack_root "),
        "{out}"
    );
}

/// Asserts that a saved version 1 list is read in place of walking, with stand-in cells for packs
/// whose bytes are not there: its packs are as long as their last file's end.
#[test]
fn a_saved_list_is_measured_with_stand_in_cells() {
    let tree = support::small_tree();
    let saves = TestDir::under(SCRATCH, "l2-bench-from");
    let saved = saves.path().join("tree.jsonl");
    fs::write(
        &saved,
        TreeList {
            entries: tree.entries,
        }
        .encode(),
    )
    .unwrap();
    let (out, result) = run(&["--runs", "3", "--from", saved.to_str().unwrap()]);
    assert_eq!(result, Ok(()), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert_eq!(
        lines[0], "cells: stand-ins (the packs' bytes are not here)",
        "{out}"
    );
    // Pack a ends at its second file's end, 2 MiB + 4 KiB: three cells; pack b at 100 bytes: one.
    assert_eq!(
        lines[1], "entries: 10 (5 files); packs: 2; cells: 4",
        "{out}"
    );
    let walk = line(&out, "walk ");
    assert!(walk.contains(" (9 of 9); "), "{out}");
}

/// Asserts that each flag the benchmark cannot run with stops it with a message, before any
/// output: no directory, no runs, a flag without its value, a value that is not a number, a
/// directory with no name, and a list that is not there.
#[test]
fn bad_flags_stop_the_run_with_a_message() {
    let missing = Path::new(SCRATCH).join("l2-bench-no-such-list");
    let cases: [(&[&str], &str); 8] = [
        (&[], USAGE),
        (&["--runs", "0", "x"], USAGE),
        (&["--runs"], "--runs N"),
        (&["--pack-mib"], "--pack-mib M"),
        (&["--from"], "--from LIST"),
        (&["--runs", "many"], "invalid digit found in string"),
        (&["/"], USAGE),
        (
            &["--from", missing.to_str().unwrap()],
            "No such file or directory (os error 2)",
        ),
    ];
    for (args, message) in cases {
        let (out, result) = run(args);
        assert_eq!(result, Err(message.to_owned()), "{args:?}");
        assert_eq!(out, "", "{args:?}");
    }
}
