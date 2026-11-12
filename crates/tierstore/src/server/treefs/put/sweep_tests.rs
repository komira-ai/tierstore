//! The sweep: staged files past the stage TTL, committed names past the committed TTL, and the
//! rewrite of a journal of mostly dead lines.

use super::commit::PutOutcome;
use super::journal::{NEW_FILE, decode};
use super::store::{Kind, Rename, stage_name};
use super::sweep::{COMPACT_SLACK, SweepReport};
use super::testing::{Rig, commit, dir, staged, warnings, written};
use crate::server::treefs::locked;

const STAGE_TTL_MS: u64 = 43_200_000;
const COMMITTED_TTL_MS: u64 = 600_000;

/// Asserts that a staged file is kept until its age from its create reaches the stage TTL, a write
/// does not make it younger, and the sweep then removes its name, its file and its bytes, and
/// journals the removal.
#[test]
fn a_staged_file_is_swept_at_the_stage_ttl() {
    let rig = Rig::new("put-sweep-staged");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = written(&store, &out, "a.part", b"abc");
    rig.clock.advance(STAGE_TTL_MS - 1);
    let _ = store.write(handle, 3, b"d").unwrap();
    assert_eq!(store.sweep(), SweepReport::default());
    assert!(store.lookup(&out, "a.part").is_some());
    rig.clock.advance(1);
    assert_eq!(
        store.sweep(),
        SweepReport {
            parts: 1,
            ..SweepReport::default()
        }
    );
    assert_eq!(store.lookup(&out, "a.part"), None);
    assert_eq!(rig.stage_files(), vec!["journal".to_owned()]);
    assert!(locked(&store.table).dirs.is_empty());
    assert!(store.write(handle, 4, b"e").is_err());
    assert_eq!(decode(rig.journal().as_bytes()).0.len(), 2);
}

/// Asserts that a staged file whose commit runs is not swept, however old.
#[test]
fn a_file_whose_commit_runs_is_not_swept() {
    let rig = Rig::new("put-sweep-uploading");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"abc");
    let running = store.rename(&out, "a.part", "a").unwrap();
    rig.clock.advance(STAGE_TTL_MS * 2);
    assert_eq!(store.sweep(), SweepReport::default());
    assert!(rig.stage().join(stage_name(0)).exists());
    let Rename::Commit(running) = running else {
        panic!("a commit");
    };
    running.finish(PutOutcome::Created).unwrap();
}

/// Asserts that a committed name is shown until its age from the commit reaches the committed TTL,
/// and is then swept from the names and from the directory's count.
#[test]
fn a_committed_name_is_shown_until_the_committed_ttl() {
    let mut rig = Rig::new("put-sweep-committed");
    rig.settings.max_dir_files = Some(1);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"abc");
    rig.clock.advance(5);
    commit(&store, &out, "a.part", "a", PutOutcome::Created).unwrap();
    rig.clock.advance(COMMITTED_TTL_MS - 1);
    assert_eq!(store.sweep(), SweepReport::default());
    assert!(matches!(
        store.lookup(&out, "a").unwrap().kind,
        Kind::Committed(_)
    ));
    assert!(store.create(&out, "b.part").is_err());
    rig.clock.advance(1);
    assert_eq!(
        store.sweep(),
        SweepReport {
            committed: 1,
            ..SweepReport::default()
        }
    );
    assert_eq!(store.lookup(&out, "a"), None);
    let _ = store.create(&out, "b.part").unwrap();
}

/// Asserts that a sweep rewrites a journal with more than twice the live entries' lines plus the
/// slack, to the live entries alone, and leaves one at that many.
#[test]
fn a_sweep_rewrites_a_journal_of_mostly_dead_lines() {
    let rig = Rig::new("put-sweep-compact");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "keep.part", b"k");
    // One live entry: the limit is 2 + COMPACT_SLACK lines, and there is one.
    for _ in 0..COMPACT_SLACK.div_ceil(2) {
        staged(&store, &out, "a.part", b"a");
        store.unlink(&out, "a.part").unwrap();
    }
    assert_eq!(
        locked(&store.table).journal.lines(),
        2 + COMPACT_SLACK + 1 - 2
    );
    let _ = store.sweep();
    assert_eq!(locked(&store.table).journal.lines(), COMPACT_SLACK + 1);
    staged(&store, &out, "a.part", b"a");
    store.unlink(&out, "a.part").unwrap();
    let _ = store.sweep();
    assert_eq!(locked(&store.table).journal.lines(), 1);
    let (live, _) = decode(rig.journal().as_bytes());
    assert_eq!(live.len(), 1);
    staged(&store, &out, "b.part", b"b");
    assert_eq!(decode(rig.journal().as_bytes()).0.len(), 2);
}

/// Asserts that a rewrite that fails (a directory in the place of `journal.new`) is said, and the
/// journal is kept and still appended to; and that a sweep whose journal lines fail still removes;
/// each failure is said in full.
#[test]
fn a_failed_rewrite_keeps_the_journal() {
    let rig = Rig::new("put-sweep-rewrite-fails");
    let store = rig.open();
    let (_log, warnings) = warnings();
    let out = dir("cas/b/1/n");
    for _ in 0..COMPACT_SLACK {
        staged(&store, &out, "a.part", b"a");
        store.unlink(&out, "a.part").unwrap();
    }
    std::fs::create_dir(rig.stage().join(NEW_FILE)).unwrap();
    let _ = store.sweep();
    assert_eq!(locked(&store.table).journal.lines(), 2 * COMPACT_SLACK);
    staged(&store, &out, "b.part", b"b");
    assert_eq!(
        decode(rig.journal().as_bytes()).0.len() as u64,
        2 * COMPACT_SLACK + 1
    );
    locked(&store.table).journal.break_appends();
    rig.clock.advance(STAGE_TTL_MS);
    assert_eq!(store.sweep().parts, 1);
    assert_eq!(store.lookup(&out, "b.part"), None);
    let not_rewritten = format!(
        "tierstore fs: the put journal in {} is not rewritten: Is a directory (os error 21)\n",
        rig.stage().display()
    );
    assert_eq!(
        warnings.text(),
        format!(
            "{not_rewritten}tierstore fs: the put journal is not written (Remove {{ id: {COMPACT_SLACK} }}): \
             Bad file descriptor (os error 9)\n{not_rewritten}"
        )
    );
}
