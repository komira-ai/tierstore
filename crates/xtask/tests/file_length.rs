//! No tracked Rust file is longer than 1000 lines (AGENTS.md R12): a long file is split by
//! subject, with the moved code unchanged.
//!
//! Two files were longer than the limit when this test was added and are split by a change
//! of their own that moves code only. Each is listed below with the length it must not
//! grow beyond. A listed file that is within the limit fails the test too, so the entry is
//! removed in the change that splits the file and the list cannot go stale.
//!
//! Catches:
//! - a tracked `.rs` file of more than 1000 lines, named with its length;
//! - a listed file that has grown past its recorded length;
//! - a listed file that no longer exceeds the limit, or no longer exists;
//! - a count that cannot see a defect: the same check, run on a text of 1001 lines, names it,
//!   and finds nothing in a text of 1000 lines, in one of no lines, or in an unterminated last
//!   line that stays within the limit.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// The most lines a Rust file has (AGENTS.md R12).
const LIMIT: usize = 1000;

/// The files over the limit that a split is still to remove, with the length each had when it was
/// listed. The splits of `server/cli.rs` and `server/config.rs` are done, so none is listed.
const SPLIT_PENDING: &[(&str, usize)] = &[];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The tracked paths of the repository that end in `.rs`, by `git ls-files`.
fn tracked_rust() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z", "--", "*.rs"])
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| panic!("run git ls-files: {e}"));
    assert!(out.status.success(), "git ls-files failed: {}", out.status);
    String::from_utf8(out.stdout)
        .unwrap_or_else(|e| panic!("git ls-files wrote a path that is not UTF-8: {e}"))
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The number of lines of `text`: a last line with no newline counts.
fn lines_of(text: &str) -> usize {
    text.lines().count()
}

/// What is wrong with the files `lengths` (path to line count), one finding each: a file over
/// `limit` that is not listed in `pending`, a listed file longer than its recorded length, a
/// listed file within `limit` (or missing from `lengths`).
fn findings(
    lengths: &BTreeMap<String, usize>,
    limit: usize,
    pending: &[(&str, usize)],
) -> Vec<String> {
    let mut found = Vec::new();
    for (path, &lines) in lengths {
        match pending.iter().find(|(listed, _)| listed == path) {
            None if lines > limit => {
                found.push(format!(
                    "{path} has {lines} lines, over the limit of {limit}"
                ));
            }
            Some(&(_, recorded)) if lines > recorded => {
                found.push(format!(
                    "{path} has grown to {lines} lines from the {recorded} it had when it was listed"
                ));
            }
            Some(_) if lines <= limit => {
                found.push(format!(
                    "{path} has {lines} lines, within the limit of {limit}: remove it from SPLIT_PENDING"
                ));
            }
            _ => {}
        }
    }
    for (listed, _) in pending {
        if !lengths.contains_key(*listed) {
            found.push(format!(
                "{listed} is listed in SPLIT_PENDING but is not tracked"
            ));
        }
    }
    found
}

#[test]
fn no_tracked_rust_file_is_over_the_limit() {
    let mut lengths = BTreeMap::new();
    for rel in tracked_rust() {
        let path = root().join(&rel);
        if !path.exists() {
            // Deleted in the work tree, not yet committed: nothing to count.
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        lengths.insert(rel, lines_of(&text));
    }
    let found = findings(&lengths, LIMIT, SPLIT_PENDING);
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// Asserts that the check names a file of 1001 lines, and accepts files of 1000 lines, of none,
/// and of 1000 lines whose last has no newline.
#[test]
fn the_check_names_a_file_one_line_over_and_accepts_one_at_the_limit() {
    let lines = |n: usize| "x\n".repeat(n);
    let mut lengths = BTreeMap::new();
    lengths.insert("over.rs".to_owned(), lines_of(&lines(LIMIT + 1)));
    lengths.insert("at.rs".to_owned(), lines_of(&lines(LIMIT)));
    lengths.insert("empty.rs".to_owned(), lines_of(""));
    lengths.insert(
        "bare.rs".to_owned(),
        lines_of(&format!("{}x", lines(LIMIT - 1))),
    );
    assert_eq!(
        findings(&lengths, LIMIT, &[]),
        vec!["over.rs has 1001 lines, over the limit of 1000".to_owned()]
    );
}

/// Asserts the three ways a listed file is wrong: it grew, it fits the limit now, it is gone.
#[test]
fn a_listed_file_that_grew_fits_or_is_gone_is_named() {
    let pending = [("long.rs", 1200), ("short.rs", 1200), ("gone.rs", 1200)];
    let mut lengths = BTreeMap::new();
    lengths.insert("long.rs".to_owned(), 1201);
    lengths.insert("short.rs".to_owned(), 900);
    assert_eq!(
        findings(&lengths, LIMIT, &pending),
        vec![
            "long.rs has grown to 1201 lines from the 1200 it had when it was listed".to_owned(),
            "short.rs has 900 lines, within the limit of 1000: remove it from SPLIT_PENDING"
                .to_owned(),
            "gone.rs is listed in SPLIT_PENDING but is not tracked".to_owned(),
        ]
    );
    lengths.insert("long.rs".to_owned(), 1200);
    lengths.insert("short.rs".to_owned(), 1100);
    lengths.insert("gone.rs".to_owned(), 1000 + 1);
    assert_eq!(findings(&lengths, LIMIT, &pending), Vec::<String>::new());
}
