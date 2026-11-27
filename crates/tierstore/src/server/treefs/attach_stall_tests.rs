//! The stall of the attachments: what the opens of a run waited on fills, and its lines in the
//! file, which the release before this one reads as it did.

use std::collections::BTreeMap;

use tierstore_testkit::dir::TestDir;

use super::{Attachments, FILE, Failure, is_run};
use crate::tree_manifest::TreeDigest;

fn tree(seed: &[u8]) -> TreeDigest {
    TreeDigest::of(seed)
}

/// Asserts that a stall is added to the attachment of every run that has the tree and of no other,
/// that a run's stall is the sum over its trees, that a run with none, or no run, has 0, and that
/// the implicit attachment of a tree records none.
#[test]
fn a_stall_is_added_to_every_run_of_the_tree_and_summed_by_run() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-sum");
    let mut attachments = Attachments::new(dir.path());
    assert!(!attachments.has_stall());
    let _ = attachments.add("one", 1, tree(b"a"));
    let _ = attachments.add("two", 1, tree(b"a"));
    let _ = attachments.add("two", 2, tree(b"b"));
    let _ = attachments.add("three", 3, tree(b"c"));
    attachments.add_implicit(4, tree(b"d"));
    attachments.stalled(1, 1500);
    attachments.stalled(2, 200);
    attachments.stalled(4, 70);
    attachments.stalled(9, 50);
    let stalls: Vec<u64> = ["one", "two", "three", "nobody"]
        .iter()
        .map(|run| attachments.stall_of(run))
        .collect();
    assert_eq!(stalls, [1500, 1700, 0, 0]);
    assert!(attachments.has_stall());
    attachments.stalled(3, u64::MAX);
    attachments.stalled(3, 1);
    assert_eq!(attachments.stall_of("three"), u64::MAX);
}

/// Asserts that an attach again of a tree the run has keeps its stall and its failures, and that
/// the run's stall ends with its attachments.
#[test]
fn an_attach_again_keeps_the_stall_and_the_failures() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-again");
    let mut attachments = Attachments::new(dir.path());
    assert!(attachments.add("one", 1, tree(b"a")));
    attachments.stalled(1, 1500);
    let failure = Failure {
        path: "/bin/cc".to_owned(),
        digest: "d".repeat(64),
        reason: "upstream 404".to_owned(),
    };
    attachments.fail(1, &failure);
    assert!(!attachments.add("one", 1, tree(b"a")));
    assert_eq!(attachments.stall_of("one"), 1500);
    let gone = attachments.remove("one").unwrap();
    assert_eq!(gone[0].2, vec![failure]);
    assert_eq!(attachments.stall_of("one"), 0);
    assert!(!attachments.has_stall());
}

/// Asserts that the file has a stall line after the line of each attachment whose stall is not
/// zero, and none for one whose stall is zero; that the pairs read back as before; and that the
/// stall reads back by tree digest and run.
#[test]
fn the_file_has_a_stall_line_only_for_an_attachment_with_a_stall() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-file");
    let mut attachments = Attachments::new(dir.path());
    let (a, b) = (tree(b"a"), tree(b"b"));
    let _ = attachments.add("run-a", 1, a);
    let _ = attachments.add("run-b", 2, b);
    attachments.stalled(1, 1500);
    attachments.snapshot().write().unwrap();
    let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
    assert_eq!(
        text,
        format!("{a} run-a\nstall_ms {a} run-a 1500\n{b} run-b\n")
    );
    assert_eq!(
        Attachments::saved(dir.path()).unwrap(),
        attachments.listing()
    );
    let want = BTreeMap::from([((a.to_string(), "run-a".to_owned()), 1500)]);
    assert_eq!(attachments.saved_stalls(), want);
}

/// Asserts that a stall line that is not `stall_ms`, a digest, a run and a whole number is passed
/// over, and that a file that is missing or cannot be read holds no stall.
#[test]
fn a_stall_line_that_is_not_a_digest_a_run_and_a_number_is_passed_over() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-junk");
    let attachments = Attachments::new(dir.path());
    assert!(attachments.saved_stalls().is_empty());
    let a = tree(b"a");
    let lines = [
        format!("{a} run-a"),
        format!("stall_ms {a} run-a 1500"),
        format!("stall_ms {a} run-b"),
        format!("stall_ms {a} run-c 5 more"),
        "stall_ms not-a-digest run-d 5".to_owned(),
        format!("stall_ms {a} run\u{e9} 5"),
        format!("stall_ms {a} run-e -1"),
        format!("stall_ms {a} run-f lots"),
        format!("stalls {a} run-g 5"),
        "stall_ms".to_owned(),
        String::new(),
    ];
    std::fs::write(dir.path().join(FILE), lines.join("\n")).unwrap();
    let want = BTreeMap::from([((a.to_string(), "run-a".to_owned()), 1500)]);
    assert_eq!(attachments.saved_stalls(), want);
    assert!(!is_run("run\u{e9}"));
    std::fs::remove_file(dir.path().join(FILE)).unwrap();
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    assert!(attachments.saved_stalls().is_empty());
}

/// Asserts that a stall carried from the file goes to the record of the run and tree, kept or
/// attached, and to no other; that a kept record keeps its stall in the file and adds it to the
/// run's; and that an attach of a kept record gives the new attachment its stall.
#[test]
fn a_carried_stall_goes_to_the_record_kept_or_attached() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-carry");
    let mut attachments = Attachments::new(dir.path());
    let (a, b) = (tree(b"a"), tree(b"b"));
    attachments.keep("run-k", a);
    assert!(!attachments.has_stall());
    attachments.carry("run-k", a, 300);
    assert!(attachments.has_stall());
    assert_eq!(attachments.stall_of("run-k"), 300);
    assert_eq!(
        attachments.snapshot().text,
        format!("{a} run-k\nstall_ms {a} run-k 300\n")
    );
    let _ = attachments.add("run-c", 2, b);
    attachments.carry("run-c", b, 70);
    attachments.carry("run-c", a, 5);
    attachments.carry("nobody", b, 9);
    assert_eq!(attachments.stall_of("run-c"), 70);
    attachments.keep("run-c", a);
    attachments.carry("run-c", a, 30);
    attachments.stalled(2, 1);
    assert_eq!(attachments.stall_of("run-c"), 101);
    assert!(attachments.add("run-k", 1, a));
    assert_eq!(attachments.stall_of("run-k"), 300);
    attachments.stalled(1, 1);
    assert_eq!(attachments.stall_of("run-k"), 301);
}

/// The reader of the attachments file of the release before the stall: a line is a digest and a
/// run, after the first space, and any other line is passed over.
fn previous_reader(text: &str) -> Vec<(TreeDigest, String)> {
    text.lines()
        .filter_map(|line| {
            let (digest, run) = line.split_once(' ')?;
            Some((TreeDigest::from_hex(digest)?, run.to_owned())).filter(|(_, run)| is_run(run))
        })
        .collect()
}

/// Asserts (B8) that the release before this one reads the attachments file this release writes
/// as the same pairs: the stall lines are passed over, and the file of a run with no stall is the
/// file it wrote.
#[test]
fn previous_release_reads_the_attachments_file_written_by_this_release() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-compat-new");
    let mut attachments = Attachments::new(dir.path());
    let (a, b, c) = (tree(b"a"), tree(b"b"), tree(b"c"));
    let _ = attachments.add("run-1", 1, a);
    let _ = attachments.add("run-2", 2, b);
    attachments.keep("run-3", c);
    attachments.carry("run-3", c, 12);
    attachments.stalled(1, 1500);
    attachments.snapshot().write().unwrap();
    let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
    assert_eq!(text.lines().count(), 5);
    let want = [
        (a, "run-1".to_owned()),
        (b, "run-2".to_owned()),
        (c, "run-3".to_owned()),
    ];
    assert_eq!(previous_reader(&text), want);
    let none = TestDir::under(std::env::temp_dir(), "tfs-stall-compat-none");
    let mut unstalled = Attachments::new(none.path());
    let _ = unstalled.add("run-1", 1, a);
    assert_eq!(unstalled.snapshot().text, format!("{a} run-1\n"));
}

/// Asserts (B8) that this release reads the attachments file the release before it wrote (a
/// checked-in file, with no stall line): the same pairs, and no stall, so each run's detach answers
/// a stall of 0.
#[test]
fn this_release_reads_the_attachments_file_written_by_the_previous_release() {
    let golden = include_str!("attachments_previous.golden");
    let dir = TestDir::under(std::env::temp_dir(), "tfs-stall-compat-old");
    std::fs::write(dir.path().join(FILE), golden).unwrap();
    let saved = Attachments::saved(dir.path()).unwrap();
    let (ab, cd) = (
        TreeDigest::from_hex(&"ab".repeat(32)).unwrap(),
        TreeDigest::from_hex(&"cd".repeat(32)).unwrap(),
    );
    assert_eq!(saved, [(ab, "run-1".to_owned()), (cd, "run-2".to_owned())]);
    let mut attachments = Attachments::new(dir.path());
    assert!(attachments.saved_stalls().is_empty());
    let _ = attachments.add("run-1", 1, ab);
    assert_eq!(attachments.stall_of("run-1"), 0);
    assert_eq!(attachments.snapshot().text, format!("{ab} run-1\n"));
}
