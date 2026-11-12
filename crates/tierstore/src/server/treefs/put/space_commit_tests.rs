//! The commit of the put directory through its namespace: the upload and the errno of each answer,
//! the wait for the release of the file and for another commit of the key, the deadline, the
//! renames that are not commits, the abort, the sweep, and what a successor is handed.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rustix::io::Errno;

use super::super::carry::{Handed, HeldDir};
use super::super::fake::failed;
use super::super::testing::{DEADLINE, Rig, SpaceRig, sha};
use super::{PutSpace, dir_ino};
use crate::server::treefs::ino;
use crate::server::treefs::testing::until;

/// The key of the directory every test writes in.
const DIR: &str = "cas/b/build/nonce";

/// The directory every test writes in, looked up from `put/`.
fn nonce(space: &PutSpace) -> u64 {
    let mut ino = ino::PUT;
    for name in DIR.split('/') {
        ino = space.lookup(ino, name).unwrap().unwrap().ino;
    }
    ino
}

/// Creates `name` in `dir`, writes `bytes` and closes it; gives its inode number.
fn staged(space: &PutSpace, dir: u64, name: &str, bytes: &[u8]) -> u64 {
    let (attr, fh) = space.create(dir, name).unwrap();
    assert_eq!(space.write((attr.ino, fh), 0, bytes), Ok(bytes.len()));
    space.release(fh);
    attr.ino
}

/// Asserts that the commit uploads the bytes as the object `<dir>/<name>`, with their size and
/// SHA-256 in the request, and that the committed name then has the number of the `.part` name,
/// mode 0400 and the size, while the `.part` name is gone.
#[test]
fn a_commit_uploads_the_bytes_and_shows_the_committed_name() {
    let rig = SpaceRig::new("put-commit");
    let space = &rig.space;
    let dir = nonce(space);
    let part = staged(space, dir, "pack.part", b"the pack");
    space
        .rename((dir, "pack.part"), (dir, "pack"), false)
        .unwrap();
    assert_eq!(
        rig.puts.object(&format!("{DIR}/pack")).unwrap(),
        b"the pack"
    );
    let asked = rig.puts.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let line = format!("put {DIR}/pack {} 8 from ", sha(b"the pack"));
    assert!(asked[0].starts_with(&line), "{}", asked[0]);
    let committed = space.lookup(dir, "pack").unwrap().unwrap();
    assert_eq!(
        (committed.ino, committed.perm, committed.size),
        (part, 0o400, 8)
    );
    assert_eq!(space.lookup(dir, "pack.part").unwrap(), None);
}

/// Asserts the errno of each answer of the upload that is not a success (`exists_different`
/// `EEXIST`, `checksum` `EBADMSG`, `toolarge` `EFBIG`, `stage` `EIO`, a word not known `EIO`, the
/// socket's refusal `EIO`, a line that is not JSON `EIO`), each asked once; that the file stays
/// staged after each; and that the same rename then commits it.
#[test]
fn each_answer_of_the_upload_has_its_errno_and_leaves_the_file_staged() {
    let rig = SpaceRig::new("put-commit-errno");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "pack.part", b"bytes");
    let answers = [
        (failed("exists_different"), Errno::EXIST),
        (failed("checksum"), Errno::BADMSG),
        (failed("toolarge"), Errno::FBIG),
        (failed("stage"), Errno::IO),
        (failed("later_word"), Errno::IO),
        ("refused malformed".to_owned(), Errno::IO),
        ("not json".to_owned(), Errno::IO),
    ];
    for (count, (answer, errno)) in answers.into_iter().enumerate() {
        rig.puts.script.lock().unwrap().push_back(answer.clone());
        let renamed = space.rename((dir, "pack.part"), (dir, "pack"), false);
        assert_eq!(renamed, Err(errno), "{answer}");
        assert_eq!(rig.puts.count(), count + 1);
        assert_eq!(space.lookup(dir, "pack.part").unwrap().unwrap().size, 5);
        assert_eq!(space.lookup(dir, "pack").unwrap(), None);
    }
    assert!(rig.pauses.lock().unwrap().is_empty());
    space
        .rename((dir, "pack.part"), (dir, "pack"), false)
        .unwrap();
    assert_eq!(rig.puts.object(&format!("{DIR}/pack")).unwrap(), b"bytes");
}

/// Asserts that an answer that the upstream did not take the put (`upstream`) is asked again after
/// the pauses of the back-off, and that the put then made counts.
#[test]
fn an_upstream_that_does_not_take_the_put_is_asked_again() {
    let rig = SpaceRig::new("put-commit-again");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "pack.part", b"again");
    for _ in 0..3 {
        rig.puts
            .script
            .lock()
            .unwrap()
            .push_back(failed("upstream"));
    }
    space
        .rename((dir, "pack.part"), (dir, "pack"), false)
        .unwrap();
    assert_eq!(rig.puts.count(), 4);
    let digest = sha(b"again");
    let expected: Vec<Duration> = (0..3)
        .map(|tries| super::super::upload::pause_before(tries, &digest))
        .collect();
    assert_eq!(*rig.pauses.lock().unwrap(), expected);
    assert_eq!(rig.puts.object(&format!("{DIR}/pack")).unwrap(), b"again");
}

/// Asserts that a put the upstream never takes ends at the commit deadline with `EIO`, after the
/// pauses that fit before it, and leaves the file staged for the same rename.
#[test]
fn an_upstream_that_never_takes_the_put_is_eio_at_the_deadline() {
    let rig = SpaceRig::new("put-commit-deadline");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "pack.part", b"never");
    for _ in 0..100 {
        rig.puts
            .script
            .lock()
            .unwrap()
            .push_back(failed("upstream"));
    }
    assert_eq!(
        space.rename((dir, "pack.part"), (dir, "pack"), false),
        Err(Errno::IO)
    );
    let pauses = rig.pauses.lock().unwrap().clone();
    let waited: Duration = pauses.iter().sum();
    assert!(waited < DEADLINE, "{waited:?}");
    let next =
        super::super::upload::pause_before(u32::try_from(pauses.len()).unwrap(), &sha(b"never"));
    assert!(waited + next >= DEADLINE, "{waited:?} + {next:?}");
    assert_eq!(rig.puts.count(), pauses.len() + 1);
    assert_eq!(space.lookup(dir, "pack.part").unwrap().unwrap().size, 5);
}

/// Asserts that a fill socket that is not there is asked again until the deadline, then `EIO`,
/// and the file stays staged.
#[test]
fn a_fill_socket_that_is_not_there_is_eio_at_the_deadline() {
    let rig = SpaceRig::of(Rig::new("put-commit-nosocket"), 1, false);
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "pack.part", b"nowhere");
    assert_eq!(
        space.rename((dir, "pack.part"), (dir, "pack"), false),
        Err(Errno::IO)
    );
    assert!(!rig.pauses.lock().unwrap().is_empty());
    assert!(space.lookup(dir, "pack.part").unwrap().is_some());
}

/// Asserts that a `.part` name whose digest is not that of its bytes is `EBADMSG` and nothing is
/// uploaded.
#[test]
fn a_name_whose_digest_is_not_that_of_the_bytes_is_ebadmsg() {
    let rig = SpaceRig::new("put-commit-digest");
    let space = &rig.space;
    let dir = nonce(space);
    let name = format!("pack.sha256-{}.part", sha(b"other"));
    let _ = staged(space, dir, &name, b"bytes");
    assert_eq!(
        space.rename((dir, &name), (dir, "pack"), false),
        Err(Errno::BADMSG)
    );
    assert_eq!(rig.puts.count(), 0);
    let right = format!("pack.sha256-{}.part", sha(b"bytes"));
    space.rename((dir, &name), (dir, &right), false).unwrap();
    space.rename((dir, &right), (dir, "pack"), false).unwrap();
    assert_eq!(rig.puts.object(&format!("{DIR}/pack")).unwrap(), b"bytes");
}

/// Asserts that a commit made while the file's handle is open waits for its release (FUSE sends
/// the release after the close returns), and then commits the bytes.
#[test]
fn a_commit_waits_for_the_release_of_the_file() {
    let rig = SpaceRig::new("put-commit-release");
    let space = &rig.space;
    let dir = nonce(space);
    let (attr, fh) = space.create(dir, "pack.part").unwrap();
    let _ = space.write((attr.ino, fh), 0, b"late").unwrap();
    std::thread::scope(|scope| {
        let commit = scope.spawn(|| space.rename((dir, "pack.part"), (dir, "pack"), false));
        until(|| space.waits.load(Ordering::Acquire) > 0);
        assert_eq!(rig.puts.count(), 0);
        space.release(fh);
        assert_eq!(commit.join().unwrap(), Ok(()));
    });
    assert_eq!(rig.puts.object(&format!("{DIR}/pack")).unwrap(), b"late");
}

/// Asserts that a commit whose file is still open when the deadline comes is `EBUSY`, uploads
/// nothing, and leaves the file staged.
#[test]
fn a_file_still_open_at_the_deadline_is_ebusy() {
    let rig = SpaceRig::new("put-commit-busy");
    let space = &rig.space;
    let dir = nonce(space);
    let (attr, fh) = space.create(dir, "pack.part").unwrap();
    let _ = space.write((attr.ino, fh), 0, b"held").unwrap();
    std::thread::scope(|scope| {
        let commit = scope.spawn(|| space.rename((dir, "pack.part"), (dir, "pack"), false));
        until(|| space.waits.load(Ordering::Acquire) > 0);
        rig.rig
            .clock
            .advance(u64::try_from(DEADLINE.as_millis()).unwrap());
        space.note_change();
        assert_eq!(commit.join().unwrap(), Err(Errno::BUSY));
    });
    assert_eq!(rig.puts.count(), 0);
    assert_eq!(space.lookup(dir, "pack.part").unwrap().unwrap().size, 4);
    space.release(fh);
    space
        .rename((dir, "pack.part"), (dir, "pack"), false)
        .unwrap();
}

/// Asserts that a commit of a key whose other commit is uploading waits for it, and then gets
/// the upstream's answer for its own bytes: the same bytes are a success, other bytes `EEXIST`.
#[test]
fn a_commit_of_a_key_being_committed_waits_and_gets_its_own_answer() {
    let rig = SpaceRig::new("put-commit-key");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "a.part", b"same");
    let _ = staged(space, dir, "b.part", b"same");
    let _ = staged(space, dir, "c.part", b"other");
    rig.puts.hold();
    std::thread::scope(|scope| {
        let first = scope.spawn(|| space.rename((dir, "a.part"), (dir, "k"), false));
        rig.puts.wait_for_held(1);
        let second = scope.spawn(|| space.rename((dir, "b.part"), (dir, "k"), false));
        until(|| space.waits.load(Ordering::Acquire) > 0);
        rig.puts.let_go();
        assert_eq!(first.join().unwrap(), Ok(()));
        assert_eq!(second.join().unwrap(), Ok(()));
    });
    assert_eq!(rig.puts.count(), 2);
    assert_eq!(
        space.rename((dir, "c.part"), (dir, "k"), false),
        Err(Errno::EXIST)
    );
    assert_eq!(rig.puts.object(&format!("{DIR}/k")).unwrap(), b"same");
}

/// Asserts that commits of many files to their own keys at once each upload their own bytes, and
/// that of two files of other bytes committed to one key at once exactly one is the object and
/// the other is `EEXIST`, whatever the order.
#[test]
fn concurrent_commits_each_upload_their_own_bytes() {
    let rig = SpaceRig::new("put-commit-concurrent");
    let space = &rig.space;
    let dir = nonce(space);
    let names: Vec<String> = (0..16).map(|n| format!("out{n}")).collect();
    for name in &names {
        let _ = staged(space, dir, &format!("{name}.part"), name.as_bytes());
    }
    let _ = staged(space, dir, "x.part", b"x bytes");
    let _ = staged(space, dir, "y.part", b"y bytes");
    std::thread::scope(|scope| {
        let mut commits = Vec::new();
        for name in &names {
            let part = format!("{name}.part");
            commits.push(scope.spawn(move || space.rename((dir, &part), (dir, name), false)));
        }
        let x = scope.spawn(|| space.rename((dir, "x.part"), (dir, "one"), false));
        let y = scope.spawn(|| space.rename((dir, "y.part"), (dir, "one"), false));
        for commit in commits {
            assert_eq!(commit.join().unwrap(), Ok(()));
        }
        let (x, y) = (x.join().unwrap(), y.join().unwrap());
        let object = rig.puts.object(&format!("{DIR}/one")).unwrap();
        match (x, y) {
            (Ok(()), Err(Errno::EXIST)) => assert_eq!(object, b"x bytes"),
            (Err(Errno::EXIST), Ok(())) => assert_eq!(object, b"y bytes"),
            other => panic!("{other:?}"),
        }
    });
    for name in &names {
        assert_eq!(
            rig.puts.object(&format!("{DIR}/{name}")).unwrap(),
            name.as_bytes()
        );
    }
}

/// Asserts the renames that are not commits: a `.part` name to another is a change of the name;
/// two directories, a final name as the source, or a name that is not there are refused; and
/// `RENAME_NOREPLACE` refuses a name that is there.
#[test]
fn the_renames_that_are_not_commits() {
    let rig = SpaceRig::new("put-commit-renames");
    let space = &rig.space;
    let dir = nonce(space);
    let ino = staged(space, dir, "a.part", b"a");
    space
        .rename((dir, "a.part"), (dir, "b.part"), false)
        .unwrap();
    assert_eq!(space.lookup(dir, "b.part").unwrap().unwrap().ino, ino);
    let other = space.lookup(dir_ino("cas/b/build"), "other").unwrap();
    let other = other.map_or(dir_ino("cas/b/build/other"), |attr| attr.ino);
    assert_eq!(
        space.rename((dir, "b.part"), (other, "b"), false),
        Err(Errno::PERM)
    );
    let _ = staged(space, dir, "c.part", b"c");
    assert_eq!(
        space.rename((dir, "b.part"), (dir, "c.part"), true),
        Err(Errno::EXIST)
    );
    space.rename((dir, "b.part"), (dir, "b"), true).unwrap();
    assert_eq!(
        space.rename((dir, "b"), (dir, "d"), false),
        Err(Errno::PERM)
    );
    assert_eq!(
        space.rename((dir, "gone.part"), (dir, "e"), false),
        Err(Errno::NOENT)
    );
    let cas = space.lookup(ino::PUT, "cas").unwrap().unwrap().ino;
    assert_eq!(
        space.rename((cas, "a.part"), (cas, "a"), false),
        Err(Errno::PERM)
    );
}

/// Asserts that an unlink of a `.part` name removes it (the abort) and that of a committed name,
/// or outside a prefix, is `EPERM`.
#[test]
fn an_unlink_aborts_a_part_and_refuses_a_final_name() {
    let rig = SpaceRig::new("put-commit-unlink");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "a.part", b"a");
    let _ = staged(space, dir, "b.part", b"b");
    space.rename((dir, "b.part"), (dir, "b"), false).unwrap();
    space.unlink(dir, "a.part").unwrap();
    assert_eq!(space.lookup(dir, "a.part").unwrap(), None);
    assert_eq!(space.unlink(dir, "b"), Err(Errno::PERM));
    assert_eq!(space.unlink(dir, "a.part"), Err(Errno::NOENT));
    assert_eq!(space.unlink(ino::PUT, "x.part"), Err(Errno::PERM));
}

/// Asserts that the sweep does nothing while held for a successor, and removes what is due once
/// it is not.
#[test]
fn the_sweep_runs_unless_held() {
    let rig = SpaceRig::new("put-commit-sweep");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = staged(space, dir, "a.part", b"a");
    rig.rig.clock.advance(43_200_000);
    space.hold(true);
    space.sweep();
    assert!(space.lookup(dir, "a.part").unwrap().is_some());
    space.hold(false);
    space.sweep();
    assert_eq!(space.lookup(dir, "a.part").unwrap(), None);
    space.sweep();
}

/// Asserts that the sweep's thread sweeps on its own, and ends with the space.
#[test]
fn the_sweep_thread_sweeps_and_ends_with_the_space() {
    let rig = SpaceRig::new("put-commit-sweeper");
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let _ = staged(&space, dir, "a.part", b"a");
    rig.rig.clock.advance(43_200_000);
    space.start_sweep(Duration::from_millis(1)).unwrap();
    until(|| space.lookup(dir, "a.part").unwrap().is_none());
    drop(space);
}

/// Asserts that a successor is handed the directories the kernel holds, with their lookups and
/// whether `mkdir` made them, and the opens it holds with their staged files; that it takes them
/// over, so a directory resolves and a commit waits for a handed open; and that a handed open of
/// a file no longer staged, or a directory that is not one, is left out.
#[test]
fn a_successor_takes_over_the_directories_and_the_opens_it_is_handed() {
    let rig = SpaceRig::new("put-commit-handed");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = space.create(dir, "kept.part").unwrap();
    let made = space.mkdir(dir, "sub").unwrap();
    let (open, fh) = space.create(dir, "open.part").unwrap();
    let _ = space.write((open.ino, fh), 0, b"half").unwrap();
    let mut handed = space.handed();
    handed.dirs.sort_by(|a, b| a.path.cmp(&b.path));
    let paths: Vec<(Option<u64>, bool, &str)> = handed
        .dirs
        .iter()
        .map(|held| (held.lookups, held.made, held.path.as_str()))
        .collect();
    assert_eq!(
        paths,
        [
            (Some(1), false, "cas"),
            (Some(1), false, "cas/b"),
            (Some(1), false, "cas/b/build"),
            (Some(1), false, DIR),
            (Some(1), true, "cas/b/build/nonce/sub"),
        ]
    );
    assert_eq!(handed.opens.len(), 2);
    handed
        .opens
        .push((crate::server::treefs::handle::make(1, 999, false), 12345));
    handed.dirs.push(HeldDir {
        lookups: Some(1),
        made: false,
        path: "bad//path".to_owned(),
    });
    handed.dirs.push(HeldDir {
        lookups: Some(1),
        made: false,
        path: String::new(),
    });
    let next = rig.successor(2);
    let space = &next.space;
    space.take_over(Some(handed));
    assert_eq!(space.getattr(made.ino).unwrap().ino, made.ino);
    assert_eq!(space.lookup(dir, "sub").unwrap().unwrap().ino, made.ino);
    assert_eq!(space.store.open_handles().len(), 2);
    assert_eq!(space.write((open.ino, fh), 4, b"done"), Ok(4));
    std::thread::scope(|scope| {
        let commit = scope.spawn(|| space.rename((dir, "open.part"), (dir, "open"), false));
        until(|| space.waits.load(Ordering::Acquire) > 0);
        space.release(fh);
        assert_eq!(commit.join().unwrap(), Ok(()));
    });
    assert_eq!(
        next.puts.object(&format!("{DIR}/open")).unwrap(),
        b"halfdone"
    );
}

/// Asserts that a start handed nothing (after a crash) finds the directories that hold names and
/// those above them, keeps them whatever the kernel forgets, and takes over an open of the process
/// before at its first write; a directory with no name is stale.
#[test]
fn a_start_handed_nothing_finds_the_directories_of_the_names() {
    let rig = SpaceRig::new("put-commit-crash");
    let space = &rig.space;
    let dir = nonce(space);
    let (open, fh) = space.create(dir, "open.part").unwrap();
    let _ = space.write((open.ino, fh), 0, b"one").unwrap();
    let empty = space.lookup(dir, "nothing").unwrap();
    assert_eq!(empty, None);
    let lone = space
        .lookup(dir_ino("cas/b/build"), "lone")
        .unwrap()
        .unwrap();
    let next = rig.successor(2);
    let space = &next.space;
    space.take_over(None);
    let seen = space
        .lookup(dir_ino("cas/b/build"), "nonce")
        .unwrap()
        .unwrap();
    assert_eq!(seen.ino, dir);
    for path in ["cas", "cas/b", "cas/b/build", DIR] {
        space.forget(dir_ino(path), 1);
        assert!(space.getattr(dir_ino(path)).is_ok(), "{path}");
    }
    assert_eq!(space.getattr(lone.ino), Err(Errno::STALE));
    assert_eq!(space.write((open.ino, fh), 3, b"two"), Ok(3));
    assert_eq!(space.write((open.ino, fh), 6, b"!"), Ok(1));
    space.release(fh);
    space.release(fh);
    space
        .rename((dir, "open.part"), (dir, "open"), false)
        .unwrap();
    assert_eq!(
        next.puts.object(&format!("{DIR}/open")).unwrap(),
        b"onetwo!"
    );
    let old = crate::server::treefs::handle::make(1, 50, false);
    assert_eq!(space.write((open.ino, old), 0, b"x"), Err(Errno::BADF));
}

/// Asserts that what is handed over survives the text of the file: the hand-over of a space,
/// written and read back, is the same.
#[test]
fn the_hand_over_of_a_space_reads_back_as_it_was() {
    let rig = SpaceRig::new("put-commit-carry");
    let space = &rig.space;
    let dir = nonce(space);
    let _ = space.create(dir, "a.part").unwrap();
    let handed: Handed = space.handed();
    let text = super::super::carry::encode(&handed);
    assert_eq!(super::super::carry::decode(&text), Some(handed));
}
