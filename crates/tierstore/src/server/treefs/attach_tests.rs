//! The attachments: what a run holds, what its opens failed on, and the file.

use tierstore_testkit::dir::TestDir;

use super::{Attachments, FILE, Failure, MAX_RUN, is_run};
use crate::tree_manifest::TreeDigest;

fn tree(seed: &[u8]) -> TreeDigest {
    TreeDigest::of(seed)
}

fn failure(path: &str) -> Failure {
    Failure {
        path: path.to_owned(),
        digest: "d".repeat(64),
        reason: "upstream 404".to_owned(),
    }
}

/// Asserts that a run name is one word of visible ASCII of at most 128 bytes.
#[test]
fn a_run_is_one_visible_word_of_at_most_128_bytes() {
    assert!(is_run("run-1"));
    assert!(is_run(&"r".repeat(MAX_RUN)));
    assert!(!is_run(""));
    assert!(!is_run(&"r".repeat(MAX_RUN + 1)));
    assert!(!is_run("two words"));
    assert!(!is_run("tab\tbed"));
    assert!(!is_run("caf\u{e9}"));
}

/// Asserts that two attaches of one tree for one run are one attachment, that a tree is attached
/// while any run has it, and that removing a run gives its trees and says so for a run that has
/// none.
#[test]
fn a_tree_is_attached_while_any_run_has_it_and_a_run_attaches_it_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-attach");
    let mut attachments = Attachments::new(dir.path());
    attachments.add("one", 1, tree(b"a"));
    attachments.add("one", 1, tree(b"a"));
    attachments.add("two", 1, tree(b"a"));
    attachments.add("two", 2, tree(b"b"));
    assert_eq!(attachments.runs(), ["one", "two"]);
    assert_eq!(attachments.listing().len(), 3);
    assert!(attachments.is_attached(1));
    assert!(!attachments.is_attached(3));
    let gone = attachments.remove("one").unwrap();
    assert_eq!(gone.len(), 1);
    assert_eq!((gone[0].0, gone[0].1), (1, tree(b"a")));
    assert!(attachments.is_attached(1));
    assert!(attachments.remove("one").is_none());
    let _ = attachments.remove("two").unwrap();
    assert!(!attachments.is_attached(1));
    assert!(attachments.runs().is_empty());
}

/// Asserts that the trees attached are listed by digest once each, whichever runs have them.
#[test]
fn the_trees_attached_are_listed_once_each() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-attach-trees");
    let mut attachments = Attachments::new(dir.path());
    assert!(attachments.trees().is_empty());
    attachments.add("one", 1, tree(b"a"));
    attachments.add("two", 1, tree(b"a"));
    attachments.add("two", 2, tree(b"b"));
    let mut want = vec![tree(b"a"), tree(b"b")];
    want.sort_by_key(ToString::to_string);
    assert_eq!(attachments.trees(), want);
}

/// Asserts that a failure is recorded on every run that has the tree, once for a path however many
/// times it fails, and is given with the attachment when the run is removed, and not on a run that
/// does not have the tree.
#[test]
fn a_failure_is_recorded_for_every_run_of_the_tree_once_per_path() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-attach-fail");
    let mut attachments = Attachments::new(dir.path());
    attachments.add("one", 1, tree(b"a"));
    attachments.add("two", 1, tree(b"a"));
    attachments.add("three", 2, tree(b"b"));
    attachments.fail(1, &failure("/bin/cc"));
    attachments.fail(1, &failure("/bin/cc"));
    attachments.fail(1, &failure("/bin/ar"));
    let one = attachments.remove("one").unwrap();
    let paths: Vec<&str> = one[0].2.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["/bin/ar", "/bin/cc"]);
    assert_eq!(attachments.remove("two").unwrap()[0].2.len(), 2);
    assert!(attachments.remove("three").unwrap()[0].2.is_empty());
}

/// Asserts that the file holds one line for each attachment, and reads back as the same pairs; a
/// line that is not a digest and a run is passed over; and a directory with no file has none.
#[test]
fn the_file_keeps_the_attachments_and_passes_over_bad_lines() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-attach-file");
    assert!(Attachments::saved(dir.path()).unwrap().is_empty());
    let mut attachments = Attachments::new(dir.path());
    attachments.add("run-b", 2, tree(b"b"));
    attachments.add("run-a", 1, tree(b"a"));
    attachments.save().unwrap();
    let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(!dir.path().join("attachments.new").exists());
    let saved = Attachments::saved(dir.path()).unwrap();
    assert_eq!(saved, attachments.listing());
    let junk = format!(
        "{text}nonsense\n{} \n{} two words\nnot-a-digest run\n",
        tree(b"x"),
        tree(b"y")
    );
    std::fs::write(dir.path().join(FILE), junk).unwrap();
    let saved = Attachments::saved(dir.path()).unwrap();
    assert_eq!(saved, attachments.listing());
}

/// Asserts that a file that cannot be read is an error and a file that cannot be written is too.
#[test]
fn a_file_that_cannot_be_read_or_written_is_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-attach-err");
    std::fs::create_dir(dir.path().join(FILE)).unwrap();
    assert!(Attachments::saved(dir.path()).is_err());
    let missing = dir.path().join("none");
    assert!(Attachments::new(&missing).save().is_err());
    std::fs::write(dir.path().join("attachments.new"), "").unwrap();
    std::fs::create_dir(missing.clone()).unwrap();
    std::fs::create_dir(missing.join(FILE)).unwrap();
    // A rename of a file over a directory fails.
    assert!(Attachments::new(&missing).save().is_err());
}
