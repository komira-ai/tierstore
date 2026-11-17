//! The tree-ready benchmark (`examples/tree_ready_bench.rs`), run on a small list in a scratch
//! directory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[path = "../examples/tree_ready_bench.rs"]
#[expect(
    dead_code,
    reason = "the example's main, which runs it from the command line"
)]
mod bench;

use std::fs;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};
use tierstore_testkit::dir::TestDir;

const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const USAGE: &str = "usage: tree_ready_bench [--runs N] --cache DIR --from LIST";
const FIRST: &str = "tree: 8 entries, 3 files, 2 packs; list 720 B";

/// Runs the benchmark with `args`: its output, and the message of the error it stopped on.
fn run(args: &[&str]) -> (String, Result<(), String>) {
    let mut out = Vec::new();
    let result = bench::run(args.iter().map(ToString::to_string), &mut out);
    (
        String::from_utf8(out).unwrap(),
        result.map_err(|e| e.to_string()),
    )
}

fn file(path: &str, contents: &[u8], pack: u64, offset: u64) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::new(0o644).unwrap(),
            size: contents.len() as u64,
            source: Some(BlobSource {
                key: Arc::from(format!("out/{pack:016x}/pack")),
                offset,
            }),
            sha256: Some(TreeDigest::from_bytes(Sha256::digest(contents).into())),
        },
    }
}

/// A small list: three files in two packs, a link and an empty directory, under a root.
fn list() -> TreeList {
    TreeList {
        entries: vec![
            file("root/a", b"alpha", 0, 0),
            file("root/b", b"bravo!", 0, 5),
            file("root/d/c", b"charlie", 1, 0),
            ListEntry {
                path: "root/link".to_owned(),
                kind: ListKind::Symlink {
                    target: "a".to_owned(),
                },
            },
            ListEntry {
                path: "root/empty".to_owned(),
                kind: ListKind::Dir { mode: Mode::DIR },
            },
        ],
    }
}

/// Runs the benchmark on `bytes` with `runs` runs, and checks its six lines and that the cache
/// directory is left empty.
fn check(name: &str, bytes: &[u8], runs: &str) {
    let scratch = TestDir::under(SCRATCH, name);
    let from = scratch.path().join("list");
    fs::write(&from, bytes).unwrap();
    let cache = scratch.path().join("cache");
    fs::create_dir(&cache).unwrap();
    let (out, result) = run(&[
        "--runs",
        runs,
        "--cache",
        cache.to_str().unwrap(),
        "--from",
        from.to_str().unwrap(),
    ]);
    assert_eq!(result, Ok(()));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert_eq!(lines[0], FIRST);
    assert!(lines[1].starts_with("sha256 of the list: "), "{out}");
    assert!(
        lines[2].starts_with("before (records): walk and records "),
        "{out}"
    );
    assert!(lines[2].contains("; create "), "{out}");
    assert!(
        lines[2].contains("; ready as the provider makes it "),
        "{out}"
    );
    assert!(lines[2].ends_with(" with no sha256"), "{out}");
    assert!(lines[3].starts_with("after (image): walk "), "{out}");
    assert!(lines[3].contains("; create_image "), "{out}");
    assert!(lines[3].ends_with(" with no sha256"), "{out}");
    assert!(
        lines[4].starts_with("image walked again (as the mount and a start do, with no span): "),
        "{out}"
    );
    assert_eq!(lines[5], "the same files in both: true");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
}

/// Asserts that a version 1 list is encoded as version 2 and benchmarked with one run, printing
/// the six lines and leaving the cache directory empty.
#[test]
fn a_version_1_list_is_benchmarked_with_one_run() {
    check("tree-ready-bench-v1-one", &list().encode(), "1");
}

/// Asserts that a version 1 list benchmarked with two runs prints the same six lines.
#[test]
fn a_version_1_list_is_benchmarked_with_two_runs() {
    check("tree-ready-bench-v1-two", &list().encode(), "2");
}

/// Asserts that a version 2 list is benchmarked as it is, with one run and with two, printing
/// the six lines and leaving the cache directory empty.
#[test]
fn a_version_2_list_is_benchmarked_as_it_is() {
    let v2 = list().encode_v2().unwrap();
    check("tree-ready-bench-v2-one", &v2, "1");
    check("tree-ready-bench-v2-two", &v2, "2");
}

/// Asserts that the first line of the report is the same for both versions of one list.
#[test]
fn both_versions_of_a_list_report_the_same_first_line() {
    let list = list();
    let first = |name: &str, bytes: &[u8]| {
        let scratch = TestDir::under(SCRATCH, name);
        let from = scratch.path().join("list");
        fs::write(&from, bytes).unwrap();
        let (out, result) = run(&[
            "--runs",
            "1",
            "--cache",
            scratch.path().join("cache").to_str().unwrap(),
            "--from",
            from.to_str().unwrap(),
        ]);
        assert_eq!(result, Ok(()));
        out.lines().next().unwrap().to_owned()
    };
    let v1 = first("tree-ready-bench-first-v1", &list.encode());
    let v2 = first("tree-ready-bench-first-v2", &list.encode_v2().unwrap());
    assert_eq!(v1, v2);
    assert_eq!(v1, FIRST);
}

/// Asserts that each bad invocation stops with its own message and prints nothing.
#[test]
fn bad_invocations_are_refused_with_their_messages() {
    let scratch = TestDir::under(SCRATCH, "tree-ready-bench-bad");
    let dir = scratch.path();
    let cache = dir.join("cache");
    let cache = cache.to_str().unwrap();
    let gone = dir.join("gone");
    let gone = gone.to_str().unwrap();
    let neither = dir.join("neither");
    fs::write(&neither, b"neither version\n").unwrap();
    let neither = neither.to_str().unwrap();
    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec![], USAGE.to_owned()),
        (
            vec!["--runs", "0", "--cache", cache, "--from", gone],
            USAGE.to_owned(),
        ),
        (vec!["--runs"], "--runs N".to_owned()),
        (
            vec!["--runs", "x"],
            "invalid digit found in string".to_owned(),
        ),
        (vec!["--cache"], "--cache DIR".to_owned()),
        (vec!["--from"], "--from LIST".to_owned()),
        (vec!["--bogus"], USAGE.to_owned()),
        (vec!["--from", gone], USAGE.to_owned()),
        (vec!["--cache", cache], USAGE.to_owned()),
        (
            vec!["--cache", cache, "--from", gone],
            "No such file or directory (os error 2)".to_owned(),
        ),
    ];
    for (args, message) in cases {
        let (out, result) = run(&args);
        assert_eq!(result, Err(message), "{args:?}");
        assert_eq!(out, "", "{args:?}");
    }
    let (out, result) = run(&["--cache", cache, "--from", neither]);
    assert_eq!(
        result,
        Err("tree list line 1: malformed: expected ident at line 1 column 2".to_owned())
    );
    assert_eq!(out, "");
}
