//! Internal names stay out of the public repository: no tracked file, and no tracked path,
//! holds a banned word. The words are kept as SHA-256 digests, so this file does not hold
//! them either.
//!
//! A word is a run of ASCII letters and digits, so `a-b`, `a_b` and `a/b` are each two words.
//! Each word is lowercased and its digest looked up; a finding names the file and line and
//! the first 12 hex digits of the digest, never the word; a banned word in the path it names
//! is shown as `<banned>`.
//!
//! A tracked file that cannot be read fails the test with the path and the I/O error. The one
//! exception is a path `git ls-files --deleted` lists, which is deleted in the work tree and has
//! no content to scan (its path is still scanned).
//!
//! Catches:
//! - a banned word in any tracked file, in any case, whether alone, in a hyphenated or
//!   underscored name, in a URL or in a comment;
//! - a banned word in the path of a tracked file;
//! - a scan that cannot see a defect: the same scan, run on texts that hold a word of a
//!   test-only deny list, finds it, and finds nothing in clean text.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use tierstore_testkit::dir::TestDir;

/// Lowercase hex SHA-256 of each banned word, lowercased. The longest banned word has 10
/// letters; `MAX_WORD` is above it.
const BANNED: &[&str] = &[
    "26d3db7618fdeda8526fe5d7a332a43a3eddf62f42f8cced23e9ac6dfa9fe96d",
    "3900342427d1a2ba2a845b221daab609b2dcd0fe0fd8929da5777fc6f2d1fe02",
];

/// No banned word is longer than this, so a longer word is not hashed.
const MAX_WORD: usize = 16;

fn digest(word: &str) -> String {
    hex::encode(Sha256::digest(word.to_ascii_lowercase().as_bytes()))
}

/// `text` with each word whose digest is in `deny` replaced by `<banned>`.
fn redact(text: &str, deny: &HashSet<String>) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.len() <= MAX_WORD && deny.contains(&digest(word)) {
            out.push_str("<banned>");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The findings for `text` read from `label`: one per word whose digest is in `deny`.
fn findings(label: &str, text: &str, deny: &HashSet<String>) -> Vec<String> {
    let label = redact(label, deny);
    let mut found = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for word in line.split(|c: char| !c.is_ascii_alphanumeric()) {
            if word.is_empty() || word.len() > MAX_WORD {
                continue;
            }
            let d = digest(word);
            if deny.contains(&d) {
                found.push(format!(
                    "{label}:{}: a banned name (sha256 {})",
                    i + 1,
                    &d[..12]
                ));
            }
        }
    }
    found
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The tracked paths of the repository, by `git ls-files`.
fn tracked() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
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

/// The tracked paths that are deleted in the work tree, by `git ls-files --deleted`.
fn deleted() -> HashSet<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z", "--deleted"])
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| panic!("run git ls-files --deleted: {e}"));
    assert!(
        out.status.success(),
        "git ls-files --deleted failed: {}",
        out.status
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The bytes of the tracked file `rel` under `root`, or `None` for a path in `gone` (deleted
/// in the work tree). Any other failure to read, a missing file that is not in `gone` too, is
/// an error that names the path (redacted by `deny`) and the cause.
fn read_tracked(
    root: &Path,
    rel: &str,
    gone: &HashSet<String>,
    deny: &HashSet<String>,
) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(root.join(rel)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound && gone.contains(rel) => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", redact(rel, deny))),
    }
}

fn real_deny() -> HashSet<String> {
    BANNED.iter().map(|d| (*d).to_owned()).collect()
}

/// Asserts that no tracked path and no tracked text file holds a banned word. A file that
/// is not UTF-8 is read lossily, so its text parts are scanned too.
#[test]
fn no_tracked_file_or_path_holds_a_banned_name() {
    let deny = real_deny();
    let gone = deleted();
    let mut found = Vec::new();
    for rel in tracked() {
        found.extend(findings(&format!("path {rel}"), &rel, &deny));
        match read_tracked(&root(), &rel, &gone, &deny) {
            Ok(Some(bytes)) => {
                found.extend(findings(&rel, &String::from_utf8_lossy(&bytes), &deny));
            }
            Ok(None) => {}
            Err(e) => found.push(e),
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// The deny list of the negative cases: three words of no meaning, by their digests.
fn test_deny() -> HashSet<String> {
    ["zebra", "quokka", "narwhal"]
        .map(digest)
        .into_iter()
        .collect()
}

/// Asserts that a word of the deny list is found in any case, between punctuation, in a
/// hyphenated and an underscored name and in a URL, with its line number, and that the
/// finding does not name the word.
#[test]
fn a_banned_word_is_found_in_every_shape_and_not_named() {
    let text = "clean line\n\
        the Zebra here\n\
        name-quokka-01 and narwhal_x\n\
        see https://example.org/NARWHAL/page\n";
    let got = findings("f.md", text, &test_deny());
    let lines: Vec<&str> = got
        .iter()
        .map(|g| g.split(": a banned name").next().unwrap_or(""))
        .collect();
    assert_eq!(lines, ["f.md:2", "f.md:3", "f.md:3", "f.md:4"]);
    for g in &got {
        for word in ["zebra", "quokka", "narwhal"] {
            assert!(!g.to_lowercase().contains(word), "{g}");
        }
    }
}

/// Asserts that text without a deny-list word, including words that only contain one or
/// are longer than any banned word, has no finding.
#[test]
fn text_without_a_banned_word_has_no_finding() {
    let text = "zebras graze\nquokkas\nnarwhalnarwhalnarwhalnarwhal\n\n";
    assert_eq!(findings("f.md", text, &test_deny()), Vec::<String>::new());
}

/// Asserts that the real deny list holds the two names it is for: each is found in a
/// hyphenated name and in capitals, built here from parts so this file does not hold them.
#[test]
fn the_real_deny_list_finds_the_names_it_is_for() {
    let names = [["k", "b", "f"].concat(), ["host", "keeper"].concat()];
    for name in names {
        let text = format!("x {}-fake and {}\n", name, name.to_uppercase());
        assert_eq!(findings("f", &text, &real_deny()).len(), 2, "{name}");
    }
}

/// Asserts that a banned word in a file name or a directory name is not printed: the finding
/// for the path and the finding for the text read from it show `<banned>` in its place, in
/// full.
#[test]
fn a_banned_word_in_a_path_is_redacted_in_the_finding() {
    let deny = test_deny();
    assert_eq!(
        findings("path docs/Zebra-notes.md", "docs/Zebra-notes.md", &deny),
        [format!(
            "path docs/<banned>-notes.md:1: a banned name (sha256 {})",
            &digest("zebra")[..12]
        )]
    );
    assert_eq!(
        findings("quokka_dir/readme.md", "narwhal\n", &deny),
        [format!(
            "<banned>_dir/readme.md:1: a banned name (sha256 {})",
            &digest("narwhal")[..12]
        )]
    );
    assert_eq!(redact("a/b-c.md", &deny), "a/b-c.md");
}

/// Asserts that a tracked file that cannot be read is an error naming the path and the cause,
/// with a banned word of the path redacted, whether it is a directory where a file was
/// expected or a missing file that git does not list as deleted.
#[test]
fn an_unreadable_tracked_file_is_an_error() {
    let deny = test_deny();
    let none = HashSet::new();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "banned-names-unreadable");
    std::fs::create_dir(dir.path().join("sub")).expect("make sub");
    let cause = std::fs::read(dir.path().join("sub")).unwrap_err();
    assert_eq!(
        read_tracked(dir.path(), "sub", &none, &deny),
        Err(format!("cannot read sub: {cause}"))
    );
    let cause = std::fs::read(dir.path().join("zebra/missing.md")).unwrap_err();
    assert_eq!(
        read_tracked(dir.path(), "zebra/missing.md", &none, &deny),
        Err(format!("cannot read <banned>/missing.md: {cause}"))
    );
}

/// Asserts that a missing file is skipped only when it is in the set of paths deleted in the
/// work tree, and that a file that is there is read.
#[test]
fn a_file_deleted_in_the_work_tree_is_skipped_explicitly() {
    let deny = test_deny();
    let gone: HashSet<String> = ["gone/file.md".to_owned()].into();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "banned-names-deleted");
    std::fs::write(dir.path().join("here.md"), b"text").expect("write here.md");
    assert_eq!(
        read_tracked(dir.path(), "gone/file.md", &gone, &deny),
        Ok(None)
    );
    assert_eq!(
        read_tracked(dir.path(), "here.md", &gone, &deny),
        Ok(Some(b"text".to_vec()))
    );
}
