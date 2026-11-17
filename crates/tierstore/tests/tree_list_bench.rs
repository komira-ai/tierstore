//! The tree list benchmark (`examples/tree_list_bench.rs`), run on a scratch tree.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[path = "../examples/tree_list_bench.rs"]
#[expect(
    dead_code,
    reason = "the example's main, which runs it from the command line"
)]
mod bench;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, parse_tree_list};
use tierstore_testkit::dir::TestDir;

const SCRATCH: &str = env!("CARGO_TARGET_TMPDIR");
const USAGE: &str =
    "usage: tree_list_bench [--runs N] [--pack-mib M] [--save LIST] (--from LIST | DIR...)";
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

fn sha(bytes: &[u8]) -> Option<TreeDigest> {
    Some(TreeDigest::from_bytes(Sha256::digest(bytes).into()))
}

fn source(pack: u64, offset: u64) -> Option<BlobSource> {
    Some(BlobSource {
        key: Arc::from(format!("out/{pack:016x}/pack")),
        offset,
    })
}

/// A scratch tree: its directory (the root's name is the directory's), and the directory that
/// holds it, which the test removes on drop.
fn scratch_tree() -> (TestDir, std::path::PathBuf) {
    let (holder, sock) = TestDir::for_socket(
        &[&std::env::temp_dir(), Path::new(SCRATCH)],
        "tree-list-bench",
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

fn name_of(root: &Path) -> String {
    root.file_name().unwrap().to_string_lossy().into_owned()
}

/// The first `label` line of the output, without the label.
fn line<'a>(out: &'a str, label: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(label))
        .unwrap_or_else(|| panic!("no line {label:?} in {out}"))
}

/// Asserts that a run over a tree of every kind of entry lists exactly the files, the empty
/// directory and the link in the walk's order, with the packs laid out as the walk lays them out,
/// leaves out `.git` and a socket, prints each line of the report, and saves the tree as a list.
#[test]
fn walking_a_tree_saves_its_entries_and_reports_each_step() {
    let (_holder, root) = scratch_tree();
    let saves = TestDir::under(SCRATCH, "tree-list-bench-save");
    let saved = saves.path().join("tree.jsonl");
    let (out, result) = run(&[
        "--runs",
        "1",
        "--pack-mib",
        "1",
        "--save",
        saved.to_str().unwrap(),
        root.to_str().unwrap(),
    ]);
    assert_eq!(result, Ok(()));

    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert!(
        lines[0].starts_with("entries listed: 7 (5 files); version 2 entries: "),
        "{out}"
    );
    assert!(lines[1].starts_with("bytes: v1 "), "{out}");
    assert!(lines[2].starts_with("sha256 of the list: v1 "), "{out}");
    assert!(
        lines[3].starts_with("v1: parse (with its sha256) "),
        "{out}"
    );
    assert!(lines[4].starts_with("v2: open "), "{out}");
    assert!(lines[4].contains(" (2 ranges); first span "), "{out}");
    assert!(lines[4].ends_with(" (4 spans)"), "{out}");
    assert!(
        line(&out, "the same tree from both: ").starts_with("true ("),
        "{out}"
    );

    let bytes = fs::read(&saved).unwrap();
    let list = parse_tree_list(&bytes, &TreeDigest::of(&bytes)).unwrap();
    let name = name_of(&root);
    let big1 = vec![b'x'; KIB600];
    let big2 = vec![b'y'; KIB600];
    let file = |path: &str, mode: u16, bytes: &[u8], source| ListEntry {
        path: format!("{name}/{path}"),
        kind: ListKind::File {
            mode: Mode::new(mode).unwrap(),
            size: bytes.len() as u64,
            source,
            sha256: sha(bytes),
        },
    };
    let expected = vec![
        file("a_small", 0o644, b"small", source(0, 0)),
        file("b_big1", 0o755, &big1, source(0, 5)),
        file("c_big2", 0o600, &big2, source(1, 0)),
        file("d_empty", 0o640, b"", None),
        ListEntry {
            path: format!("{name}/e_link"),
            kind: ListKind::Symlink {
                target: "a_small".to_owned(),
            },
        },
        ListEntry {
            path: format!("{name}/f_emptydir"),
            kind: ListKind::Dir { mode: Mode::DIR },
        },
        file(
            "g_nested/h_inner/leaf",
            0o600,
            b"leaf!!!",
            source(1, KIB600 as u64),
        ),
    ];
    assert_eq!(list.entries, expected);
}

/// Asserts that a run over a saved list, in place of a walk, lists the same entries and reports
/// the same tree from both versions.
#[test]
fn a_saved_list_is_read_in_place_of_a_walk() {
    let (_holder, root) = scratch_tree();
    let saves = TestDir::under(SCRATCH, "tree-list-bench-from");
    let saved = saves.path().join("tree.jsonl");
    let (walked, result) = run(&[
        "--runs",
        "1",
        "--pack-mib",
        "1",
        "--save",
        saved.to_str().unwrap(),
        root.to_str().unwrap(),
    ]);
    assert_eq!(result, Ok(()));
    let (out, result) = run(&["--runs", "1", "--from", saved.to_str().unwrap()]);
    assert_eq!(result, Ok(()));
    assert_eq!(
        line(&out, "entries listed: ").split(';').next(),
        line(&walked, "entries listed: ").split(';').next()
    );
    assert_eq!(
        line(&out, "entries listed: "),
        "7 (5 files); version 2 entries: 11"
    );
    assert!(
        line(&out, "the same tree from both: ").starts_with("true ("),
        "{out}"
    );
}

/// Asserts that each bad invocation stops with its own message and prints nothing.
#[test]
fn bad_invocations_are_refused_with_their_messages() {
    let missing = TestDir::under(SCRATCH, "tree-list-bench-missing");
    let gone = missing.path().join("gone");
    let gone = gone.to_str().unwrap();
    let io = "No such file or directory (os error 2)";
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], USAGE),
        (vec!["--runs", "0", "x"], USAGE),
        (vec!["--runs"], "--runs N"),
        (vec!["--runs", "x"], "invalid digit found in string"),
        (vec!["--pack-mib"], "--pack-mib M"),
        (vec!["--save"], "--save LIST"),
        (vec!["--from"], "--from LIST"),
        (vec!["--from", gone], io),
        (vec!["/"], USAGE),
        (vec![gone], io),
    ];
    for (args, message) in cases {
        let (out, result) = run(&args);
        assert_eq!(result, Err(message.to_owned()), "{args:?}");
        assert_eq!(out, "", "{args:?}");
    }
}

/// Asserts that a tree with no file of size above 0 is refused for having no range, since the
/// ranges table is read before any span.
#[test]
fn a_tree_of_no_bytes_has_no_range() {
    let holder = TestDir::under(SCRATCH, "tree-list-bench-nobytes");
    let root = holder.path().join("tree");
    fs::create_dir(&root).unwrap();
    put(&root.join("empty"), b"", 0o644);
    let (_, result) = run(&["--runs", "1", root.to_str().unwrap()]);
    assert_eq!(result, Err("the list has no range".to_owned()));
}
