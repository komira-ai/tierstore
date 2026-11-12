//! The commit: a rename of a `.part` name to a final one, the upload's answers, and what a commit
//! that runs keeps from happening.

use std::fs::OpenOptions;
use std::sync::Arc;

use rustix::io::Errno;

use super::commit::PutOutcome;
use super::error::PutError;
use super::journal::{Record, decode};
use super::store::{Attr, Kind, Rename, stage_name};
use super::testing::{Rig, START_MS, commit, dir, sha, staged, warnings, written};
use crate::server::treefs::locked;

/// Asserts that a commit hands the caller the key, the SHA-256, the size and the staged file of the
/// bytes; that a created object then shows as the committed name with its digest and size and the
/// time of the commit, with no `.part` name, no staged file and no staged bytes; and that the
/// commit is journaled.
#[test]
fn a_commit_hands_over_the_object_and_shows_the_committed_name() {
    let rig = Rig::new("put-commit");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "pack.part", b"hello");
    let Rename::Commit(commit) = store.rename(&out, "pack.part", "pack").unwrap() else {
        panic!("a commit");
    };
    assert_eq!(commit.key(), "cas/b/1/n/pack");
    assert_eq!(commit.sha256(), sha(b"hello"));
    assert_eq!(commit.size(), 5);
    assert_eq!(commit.path(), rig.stage().join(stage_name(0)));
    let shown = format!("{commit:?}");
    assert!(shown.contains("key: \"cas/b/1/n/pack\""), "{shown}");
    assert!(shown.contains(&format!("{:?}", rig.stage())), "{shown}");
    assert_eq!(std::fs::read(commit.path()).unwrap(), b"hello");
    rig.clock.advance(7);
    commit.finish(PutOutcome::Created).unwrap();
    let shown = Attr {
        size: 5,
        ms: START_MS + 7,
        kind: Kind::Committed(sha(b"hello")),
    };
    assert_eq!(store.lookup(&out, "pack"), Some(shown));
    assert_eq!(store.lookup(&out, "pack.part"), None);
    assert_eq!(rig.stage_files(), vec!["journal".to_owned()]);
    assert_eq!(locked(&store.table).dirs["cas/b/1/n"].bytes, 0);
    assert_eq!(
        decode(rig.journal().as_bytes()).0.last(),
        Some(&Record::Commit {
            id: 0,
            name: "pack".to_owned(),
            sha256: sha(b"hello").to_string(),
            size: 5,
            ms: START_MS + 7,
        })
    );
}

/// Asserts that a key the upstream already holds with the same SHA-256 (a put made again after its
/// answer was lost) is a commit too.
#[test]
fn an_object_already_stored_with_the_same_digest_is_committed() {
    let rig = Rig::new("put-commit-again");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "pack.part", b"hello");
    commit(&store, &out, "pack.part", "pack", PutOutcome::AlreadyStored).unwrap();
    assert_eq!(
        store.lookup(&out, "pack").unwrap().kind,
        Kind::Committed(sha(b"hello"))
    );
}

/// Asserts that a commit is refused with `EBUSY` while a handle of the file is open, and is made
/// once it is released.
#[test]
fn a_commit_waits_for_the_last_handle() {
    let rig = Rig::new("put-commit-open");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = written(&store, &out, "pack.part", b"hello");
    let refused = store.rename(&out, "pack.part", "pack").unwrap_err();
    assert!(matches!(
        refused,
        PutError::Busy("a handle of the file is open")
    ));
    assert_eq!(refused.errno(), Errno::BUSY);
    store.release(handle);
    commit(&store, &out, "pack.part", "pack", PutOutcome::Created).unwrap();
}

/// Asserts that bytes that do not hash to the digest the `.part` name carries are refused with
/// `EBADMSG` naming both digests, and stay staged; that bytes that do are committed; and that a
/// name without a digest commits whatever bytes it has.
#[test]
fn the_digest_of_the_name_must_be_the_digest_of_the_bytes() {
    let rig = Rig::new("put-commit-digest");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let wrong = format!("a.sha256-{}.part", sha(b"hello"));
    staged(&store, &out, &wrong, b"hellO");
    let refused = store.rename(&out, &wrong, "a").unwrap_err();
    assert!(matches!(
        refused,
        PutError::DigestMismatch { expected, actual } if expected == sha(b"hello") && actual == sha(b"hellO")
    ));
    assert_eq!(refused.errno(), Errno::BADMSG);
    assert_eq!(store.lookup(&out, &wrong).unwrap().kind, Kind::Staged);
    let right = format!("b.sha256-{}.part", sha(b"hello"));
    staged(&store, &out, &right, b"hello");
    commit(&store, &out, &right, "b", PutOutcome::Created).unwrap();
    assert!(matches!(
        store.rename(&out, &wrong, "plain.part").unwrap(),
        Rename::Renamed
    ));
    commit(&store, &out, "plain.part", "a", PutOutcome::Created).unwrap();
    assert_eq!(
        store.lookup(&out, "a").unwrap().kind,
        Kind::Committed(sha(b"hellO"))
    );
}

/// Asserts that the digest of a file written in order is the inline one: the commit does not read
/// the file again (bytes changed behind the store are not what it hashes).
#[test]
fn a_file_written_in_order_is_not_read_again_at_commit() {
    let rig = Rig::new("put-commit-inline");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = store.create(&out, "a.part").unwrap();
    let _ = store.write(handle, 0, b"hello ").unwrap();
    let _ = store.write(handle, 6, b"world").unwrap();
    store.release(handle);
    std::fs::write(rig.stage().join(stage_name(0)), b"HELLO WORLD").unwrap();
    let Rename::Commit(commit) = store.rename(&out, "a.part", "a").unwrap() else {
        panic!("a commit");
    };
    assert_eq!(commit.sha256(), sha(b"hello world"));
}

/// Asserts that a file written out of order is hashed at commit from what it holds.
#[test]
fn a_file_written_out_of_order_is_hashed_at_commit() {
    let rig = Rig::new("put-commit-hash");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = store.create(&out, "a.part").unwrap();
    let _ = store.write(handle, 6, b"world").unwrap();
    let _ = store.write(handle, 0, b"hello ").unwrap();
    store.release(handle);
    let Rename::Commit(commit) = store.rename(&out, "a.part", "a").unwrap() else {
        panic!("a commit");
    };
    assert_eq!((commit.sha256(), commit.size()), (sha(b"hello world"), 11));
}

/// Asserts that each failed answer of the upload is its own error and errno (`EEXIST`, `EBADMSG`,
/// `EFBIG`, `EIO`), leaves the file staged, and that the same rename made again commits it.
#[test]
fn a_failed_upload_leaves_the_file_staged_for_the_same_rename() {
    let rig = Rig::new("put-commit-fails");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "pack.part", b"hello");
    let answers = [
        (PutOutcome::ExistsDifferent, Errno::EXIST),
        (PutOutcome::Checksum, Errno::BADMSG),
        (PutOutcome::TooLarge, Errno::FBIG),
        (PutOutcome::Upstream, Errno::IO),
    ];
    for (outcome, errno) in answers {
        let refused = commit(&store, &out, "pack.part", "pack", outcome).unwrap_err();
        assert_eq!(refused.errno(), errno, "{outcome:?}");
        assert_eq!(store.lookup(&out, "pack.part").unwrap().kind, Kind::Staged);
        assert_eq!(store.lookup(&out, "pack"), None);
        assert!(locked(&store.table).uploading.is_empty());
    }
    commit(&store, &out, "pack.part", "pack", PutOutcome::Created).unwrap();
}

/// Asserts that a commit dropped unfinished (its caller gone) leaves the file staged, and that it
/// can be committed after.
#[test]
fn a_dropped_commit_leaves_the_file_staged() {
    let rig = Rig::new("put-commit-dropped");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "pack.part", b"hello");
    drop(store.rename(&out, "pack.part", "pack").unwrap());
    assert_eq!(store.lookup(&out, "pack.part").unwrap().kind, Kind::Staged);
    commit(&store, &out, "pack.part", "pack", PutOutcome::Created).unwrap();
}

/// Asserts that while a commit runs, the file cannot be committed again, renamed or removed, no
/// other file is committed to its key, and other keys are committed.
#[test]
fn a_running_commit_holds_its_file_and_its_key() {
    let rig = Rig::new("put-commit-busy");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"a");
    staged(&store, &out, "b.part", b"b");
    let running = store.rename(&out, "a.part", "a").unwrap();
    let file = "a commit of the file is running";
    assert!(matches!(store.rename(&out, "a.part", "x"), Err(PutError::Busy(why)) if why == file));
    assert!(
        matches!(store.rename(&out, "a.part", "x.part"), Err(PutError::Busy(why)) if why == file)
    );
    assert!(matches!(store.unlink(&out, "a.part"), Err(PutError::Busy(why)) if why == file));
    let key = "a commit of the key is running";
    assert!(matches!(store.rename(&out, "b.part", "a"), Err(PutError::Busy(why)) if why == key));
    commit(&store, &out, "b.part", "b", PutOutcome::Created).unwrap();
    let Rename::Commit(running) = running else {
        panic!("a commit");
    };
    running.finish(PutOutcome::Created).unwrap();
}

/// Asserts that a commit to a name already committed in the directory replaces it: one name, with
/// the later digest, size and time.
#[test]
fn a_commit_replaces_a_committed_name() {
    let rig = Rig::new("put-commit-replace");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"one");
    commit(&store, &out, "a.part", "x", PutOutcome::Created).unwrap();
    rig.clock.advance(1);
    staged(&store, &out, "b.part", b"three");
    commit(&store, &out, "b.part", "x", PutOutcome::AlreadyStored).unwrap();
    let shown = Attr {
        size: 5,
        ms: START_MS + 1,
        kind: Kind::Committed(sha(b"three")),
    };
    assert_eq!(store.list(&out), vec![("x".to_owned(), shown)]);
    assert_eq!(locked(&store.table).dirs["cas/b/1/n"].files, 1);
}

/// Asserts that a staged file over the `max_object_bytes` of the store that commits it (a restart
/// with a lower limit) is refused with `EFBIG` at the rename and stays staged.
#[test]
fn a_commit_over_max_object_bytes_is_too_large() {
    let mut rig = Rig::new("put-commit-large");
    {
        let store = rig.open();
        staged(&store, &dir("cas/b/1/n"), "a.part", b"0123456789");
    }
    rig.settings.max_object_bytes = Some(5);
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let refused = store.rename(&out, "a.part", "a").unwrap_err();
    assert!(matches!(refused, PutError::TooLarge));
    assert_eq!(store.lookup(&out, "a.part").unwrap().kind, Kind::Staged);
    assert!(locked(&store.table).uploading.is_empty());
}

/// Asserts that a commit whose file cannot be read to be hashed is said and leaves the file staged
/// with no commit running.
#[test]
fn a_commit_whose_file_cannot_be_read_is_refused() {
    let rig = Rig::new("put-commit-unreadable");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let handle = store.create(&out, "a.part").unwrap();
    let _ = store.write(handle, 1, b"x").unwrap();
    store.release(handle);
    {
        let body = Arc::clone(&locked(&store.table).bodies[&0]);
        let path = rig.stage().join(stage_name(0));
        locked(&body).file = OpenOptions::new().write(true).open(path).unwrap();
    }
    assert!(matches!(
        store.rename(&out, "a.part", "a"),
        Err(PutError::Io(_))
    ));
    assert_eq!(store.lookup(&out, "a.part").unwrap().kind, Kind::Staged);
    assert!(locked(&store.table).uploading.is_empty());
}

/// Asserts that a commit whose journal line cannot be written, or whose staged file is already gone,
/// still stands: the object was put, so the committed name is shown.
#[test]
fn a_commit_stands_when_its_journal_line_or_file_removal_fails() {
    let rig = Rig::new("put-commit-journal");
    let store = rig.open();
    let (_log, warnings) = warnings();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "a.part", b"a");
    staged(&store, &out, "b.part", b"b");
    std::fs::remove_file(rig.stage().join(stage_name(0))).unwrap();
    commit(&store, &out, "a.part", "a", PutOutcome::Created).unwrap();
    assert_eq!(
        store.lookup(&out, "a").unwrap().kind,
        Kind::Committed(sha(b"a"))
    );
    locked(&store.table).journal.break_appends();
    commit(&store, &out, "b.part", "a", PutOutcome::Created).unwrap();
    assert_eq!(
        store.lookup(&out, "a").unwrap().kind,
        Kind::Committed(sha(b"b"))
    );
    assert_eq!(
        warnings.text(),
        format!(
            "tierstore fs: cannot remove the staged file {}: No such file or directory (os error 2)\n\
             tierstore fs: the put journal is not written (Remove {{ id: 0 }}): Bad file descriptor \
             (os error 9)\n\
             tierstore fs: the put journal is not written (Commit {{ id: 1, name: \"a\", sha256: \
             \"{}\", size: 1, ms: {START_MS} }}): Bad file descriptor (os error 9)\n",
            stage_name(0),
            sha(b"b")
        )
    );
}

/// Asserts that a commit's key over 1024 bytes is refused before anything is read.
#[test]
fn a_commit_to_a_key_too_long_is_refused() {
    let rig = Rig::new("put-commit-long");
    let store = rig.open();
    let out = dir(&format!("cas/b/{0}/{0}/{0}/{0}", "d".repeat(250)));
    staged(&store, &out, "a.part", b"a");
    let long = "n".repeat(255);
    assert!(matches!(
        store.rename(&out, "a.part", &long),
        Err(PutError::NameTooLong)
    ));
}
