//! Internal names stay out of the public repository: no tracked file, and no tracked path,
//! holds a banned word. The words are kept as SHA-256 digests, so this file does not hold
//! them either.
//!
//! A word is a run of ASCII letters and digits, so `a-b`, `a_b` and `a/b` are each two words.
//! Each word is lowercased and its digest looked up; a finding names the file and line and
//! the first 12 hex digits of the digest, never the word.
//!
//! Catches:
//! - a banned word in any tracked file, in any case, whether alone, in a hyphenated or
//!   underscored name, in a URL or in a comment;
//! - a banned word in the path of a tracked file;
//! - a scan that cannot see a defect: the same scan, run on texts that hold a word of a
//!   test-only deny list, finds it, and finds nothing in clean text.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

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

/// The findings for `text` read from `label`: one per word whose digest is in `deny`.
fn findings(label: &str, text: &str, deny: &HashSet<String>) -> Vec<String> {
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

fn real_deny() -> HashSet<String> {
    BANNED.iter().map(|d| (*d).to_owned()).collect()
}

/// Asserts that no tracked path and no tracked text file holds a banned word. A file that
/// is not UTF-8 is read lossily, so its text parts are scanned too.
#[test]
fn no_tracked_file_or_path_holds_a_banned_name() {
    let deny = real_deny();
    let mut found = Vec::new();
    for rel in tracked() {
        found.extend(findings(&format!("path {rel}"), &rel, &deny));
        let Ok(bytes) = std::fs::read(root().join(&rel)) else {
            continue; // a tracked path deleted in the work tree
        };
        found.extend(findings(&rel, &String::from_utf8_lossy(&bytes), &deny));
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
