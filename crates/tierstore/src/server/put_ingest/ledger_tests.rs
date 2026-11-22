//! The ledger of the put ingest: declarations, the verdict on a committed file (put it, it is
//! running, wait for its declaration, refuse it after the grace), the end of a put, which
//! directories have a put running, and what a scan lets the ledger forget.

use std::collections::BTreeSet;

use super::GRACE;
use super::ledger::{FileId, Ledger, Verdict};
use super::names::JobDir;
use crate::TreeDigest;

/// The directory of `build`.
fn dir(build: &str) -> JobDir {
    JobDir::new(build, "n1").unwrap()
}

/// The file `name` of the directory of `build`.
fn id(build: &str, name: &str) -> FileId {
    (dir(build), name.to_owned())
}

/// The digest of `text`.
fn sha(text: &str) -> TreeDigest {
    TreeDigest::of(text.as_bytes())
}

/// The milliseconds of the grace.
fn grace() -> u64 {
    u64::try_from(GRACE.as_millis()).unwrap()
}

/// Asserts that declaring a name nobody waited for says no, and that declaring one whose
/// committed file was waiting says yes and ends the wait.
#[test]
fn declare_says_whether_a_committed_file_was_waiting() {
    let mut ledger = Ledger::default();
    assert!(!ledger.declare(&dir("b1"), "pack", sha("a")));
    assert_eq!(ledger.sizes(), (1, 0));
    let mut ledger = Ledger::default();
    assert_eq!(ledger.verdict(&id("b1", "pack"), 5), Verdict::Wait);
    assert_eq!(ledger.sizes(), (0, 1));
    assert!(ledger.declare(&dir("b1"), "pack", sha("a")));
    assert_eq!(ledger.sizes(), (1, 0));
    assert!(!ledger.declare(&dir("b1"), "pack", sha("a")));
}

/// Asserts that the last declaration of a name counts.
#[test]
fn the_last_declaration_counts() {
    let mut ledger = Ledger::default();
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let _ = ledger.declare(&dir("b1"), "pack", sha("b"));
    assert_eq!(ledger.sizes(), (1, 0));
    assert_eq!(
        ledger.verdict(&id("b1", "pack"), 0),
        Verdict::Store(sha("b"))
    );
}

/// Asserts that a declared file is to be stored with its digest and marked running, so that a
/// second verdict on it is running.
#[test]
fn a_declared_file_is_stored_once_and_then_running() {
    let mut ledger = Ledger::default();
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let file = id("b1", "pack");
    assert_eq!(ledger.verdict(&file, 0), Verdict::Store(sha("a")));
    assert!(ledger.busy(&dir("b1")));
    assert_eq!(ledger.verdict(&file, 1), Verdict::Running);
    assert_eq!(ledger.verdict(&file, u64::MAX), Verdict::Running);
}

/// Asserts that the end of a put that forgets drops the declaration (the next verdict waits as for
/// an undeclared file) and frees the directory.
#[test]
fn done_with_forget_drops_the_declaration() {
    let mut ledger = Ledger::default();
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let file = id("b1", "pack");
    assert_eq!(ledger.verdict(&file, 0), Verdict::Store(sha("a")));
    ledger.done(&file, true);
    assert!(!ledger.busy(&dir("b1")));
    assert_eq!(ledger.sizes(), (0, 0));
    assert_eq!(ledger.verdict(&file, 10), Verdict::Wait);
}

/// Asserts that the end of a put that does not forget keeps the declaration, so that the next
/// verdict is to store again.
#[test]
fn done_without_forget_keeps_the_declaration() {
    let mut ledger = Ledger::default();
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let file = id("b1", "pack");
    assert_eq!(ledger.verdict(&file, 0), Verdict::Store(sha("a")));
    ledger.done(&file, false);
    assert!(!ledger.busy(&dir("b1")));
    assert_eq!(ledger.sizes(), (1, 0));
    assert_eq!(ledger.verdict(&file, 1), Verdict::Store(sha("a")));
}

/// Asserts that an undeclared file waits until the grace has passed since it was first found,
/// is still waiting one millisecond before, and is refused exactly at the grace.
#[test]
fn an_undeclared_file_waits_for_the_grace_and_is_then_refused() {
    let mut ledger = Ledger::default();
    let file = id("b1", "pack");
    let start = 1_000;
    assert_eq!(ledger.verdict(&file, start), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, start + 1), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, start + grace() - 1), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, start + grace()), Verdict::Refuse);
}

/// Asserts that a refusal forgets when the file was first found, so that a later verdict on the
/// same name starts the wait again.
#[test]
fn a_refusal_forgets_the_first_finding() {
    let mut ledger = Ledger::default();
    let file = id("b1", "pack");
    assert_eq!(ledger.verdict(&file, 0), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, grace()), Verdict::Refuse);
    assert_eq!(ledger.sizes(), (0, 0));
    assert_eq!(ledger.verdict(&file, grace() + 1), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, 2 * grace()), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, 2 * grace() + 1), Verdict::Refuse);
}

/// Asserts that a clock that reads earlier than the first finding does not refuse.
#[test]
fn a_clock_that_goes_back_does_not_refuse() {
    let mut ledger = Ledger::default();
    let file = id("b1", "pack");
    assert_eq!(ledger.verdict(&file, 10 * grace()), Verdict::Wait);
    assert_eq!(ledger.verdict(&file, 0), Verdict::Wait);
}

/// Asserts that a directory is busy only while a put of one of its files is running, and not
/// when a put runs in another directory.
#[test]
fn a_directory_is_busy_while_a_put_of_its_file_runs() {
    let mut ledger = Ledger::default();
    assert!(!ledger.busy(&dir("b1")));
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let _ = ledger.declare(&dir("b1"), "other", sha("b"));
    let _ = ledger.verdict(&id("b1", "pack"), 0);
    assert!(ledger.busy(&dir("b1")));
    assert!(!ledger.busy(&dir("b2")));
    ledger.done(&id("b1", "pack"), true);
    assert!(!ledger.busy(&dir("b1")));
}

/// Asserts that a scan drops the declarations and the waiting files of directories it did not
/// find, and keeps those of the directories it did.
#[test]
fn keep_only_prunes_what_is_not_live() {
    let mut ledger = Ledger::default();
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let _ = ledger.declare(&dir("b2"), "pack", sha("b"));
    assert_eq!(ledger.verdict(&id("b1", "wait"), 0), Verdict::Wait);
    assert_eq!(ledger.verdict(&id("b2", "wait"), 0), Verdict::Wait);
    assert_eq!(ledger.sizes(), (2, 2));
    ledger.keep_only(&BTreeSet::from([dir("b1")]));
    assert_eq!(ledger.sizes(), (1, 1));
    assert_eq!(
        ledger.verdict(&id("b1", "pack"), 0),
        Verdict::Store(sha("a"))
    );
    ledger.done(&id("b1", "pack"), false);
    ledger.keep_only(&BTreeSet::new());
    assert_eq!(ledger.sizes(), (0, 0));
}

/// Asserts that a scan keeps the declaration of a file whose put is running even when its
/// directory is not live, and that the declaration goes at the next scan once the put is done.
#[test]
fn keep_only_keeps_the_entries_of_running_files() {
    let mut ledger = Ledger::default();
    let _ = ledger.declare(&dir("b1"), "pack", sha("a"));
    let _ = ledger.declare(&dir("b1"), "idle", sha("b"));
    let file = id("b1", "pack");
    assert_eq!(ledger.verdict(&file, 0), Verdict::Store(sha("a")));
    ledger.keep_only(&BTreeSet::new());
    assert_eq!(ledger.sizes(), (1, 0));
    assert_eq!(ledger.verdict(&file, 1), Verdict::Running);
    ledger.done(&file, false);
    ledger.keep_only(&BTreeSet::new());
    assert_eq!(ledger.sizes(), (0, 0));
}
