//! The commit that runs off the request's thread: it holds no thread while it waits for a release
//! or for another commit, a change wakes only the commits that wait for it, a release between a
//! commit's busy answer and its wait is not lost, and what is decided at once is answered on the
//! calling thread.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, ThreadId};

use rustix::io::Errno;

use super::super::super::changes::{Changes, Topic};
use super::super::super::fake::failed;
use super::super::super::testing::{DEADLINE, SpaceRig, warnings};
use super::super::PutSpace;
use crate::server::treefs::ino::{self, PutIno};
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

/// Creates `name` in `dir`, writes `bytes` and flushes the open, which is not released: a file
/// being closed. Gives its number and the open's.
fn closing(space: &PutSpace, dir: u64, name: &str, bytes: &[u8]) -> (u64, u64) {
    let (attr, fh) = space.create(dir, name).unwrap();
    assert_eq!(space.write((attr.ino, fh), 0, bytes), Ok(bytes.len()));
    space.flush(fh);
    let Some(PutIno::File(id)) = ino::put_of(attr.ino) else {
        panic!("a file");
    };
    (id, fh)
}

/// Creates `name` in `dir`, writes `bytes` and releases the open.
fn staged(space: &PutSpace, dir: u64, name: &str, bytes: &[u8]) {
    let (_, fh) = closing(space, dir, name, bytes);
    space.release(fh);
}

/// What a rename's answer was and the thread it was given on.
type Answer = (Result<(), Errno>, ThreadId, Option<String>);

/// Starts the rename `name` to `new` in `dir`; the receiver gets its answer.
fn rename(space: &Arc<PutSpace>, dir: u64, name: &str, new: &str) -> Receiver<Answer> {
    let (send, answers) = mpsc::channel();
    space.rename_with((dir, name), (dir, new), false, move |result| {
        let here = thread::current();
        send.send((result, here.id(), here.name().map(str::to_owned)))
            .unwrap();
    });
    answers
}

/// A commit whose file is being closed (flushed, not released) returns to its caller, which then
/// serves the release on the same thread, with no other thread of the caller's to do it: the
/// commit completes and uploads the bytes. A commit that held the calling thread could not be
/// released by it, and would end `EBUSY` at the deadline.
#[test]
fn a_commit_waiting_for_a_release_does_not_keep_the_calling_thread() {
    let rig = SpaceRig::new("put-rename-release");
    let puts = Arc::clone(&rig.puts);
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let (_, fh) = closing(&space, dir, "pack.part", b"late");
    let answers = rename(&space, dir, "pack.part", "pack");
    until(|| space.changes.waits.load(Ordering::Acquire) > 0);
    assert_eq!(answers.try_recv().unwrap_err(), TryRecvError::Empty);
    assert_eq!(puts.count(), 0);
    space.release(fh);
    let (result, on, name) = answers.recv().unwrap();
    assert_eq!(result, Ok(()));
    assert_ne!(on, thread::current().id());
    assert_eq!(name.as_deref(), Some("fs-commit"));
    assert_eq!(puts.object(&format!("{DIR}/pack")).unwrap(), b"late");
}

/// A release wakes the commit of its own file alone: with commits of several files waiting, each
/// for the release of its own, the release of one tells one shard of waits and one commit tries
/// again, while the others sleep on and are still pending; each release then completes its own.
#[test]
fn a_release_wakes_only_the_commit_of_its_file() {
    let rig = SpaceRig::new("put-rename-wakes");
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let mut commits = Vec::new();
    for n in 0..4 {
        let (_, fh) = closing(&space, dir, &format!("out{n}.part"), b"bytes");
        let answers = rename(&space, dir, &format!("out{n}.part"), &format!("out{n}"));
        commits.push((fh, answers));
    }
    let topics: Vec<usize> = (0..4)
        .flat_map(|id| {
            let key = format!("{DIR}/out{id}");
            [
                Changes::index(Topic::File(id)),
                Changes::index(Topic::Key(&key)),
            ]
        })
        .collect();
    let mut distinct = topics.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), topics.len(), "{topics:?}");
    until(|| space.changes.waits.load(Ordering::Acquire) >= 4);
    assert_eq!(space.tries.load(Ordering::Acquire), 4);
    let (fh, answers) = &commits[2];
    space.release(*fh);
    assert_eq!(answers.recv().unwrap().0, Ok(()));
    assert_eq!(space.tries.load(Ordering::Acquire), 5);
    assert_eq!(space.changes.told.load(Ordering::Acquire), 1);
    for (n, (_, answers)) in commits.iter().enumerate().filter(|(n, _)| *n != 2) {
        assert_eq!(answers.try_recv().unwrap_err(), TryRecvError::Empty, "{n}");
    }
    for (n, (fh, answers)) in commits.iter().enumerate().filter(|(n, _)| *n != 2) {
        space.release(*fh);
        assert_eq!(answers.recv().unwrap().0, Ok(()), "{n}");
    }
    assert_eq!(space.tries.load(Ordering::Acquire), 8);
    assert_eq!(space.changes.told.load(Ordering::Acquire), 4);
}

/// A release that happens after a commit found its file busy and before it waits is not lost: the
/// commit does not sleep, tries again and completes.
#[test]
fn a_release_between_the_busy_answer_and_the_wait_is_not_lost() {
    let rig = SpaceRig::new("put-rename-lost");
    let puts = Arc::clone(&rig.puts);
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let (_, fh) = closing(&space, dir, "pack.part", b"between");
    let weak = Arc::downgrade(&space);
    *space.before_wait.lock().unwrap() = Some(Arc::new(move || {
        if let Some(space) = weak.upgrade() {
            space.release(fh);
        }
    }));
    let answers = rename(&space, dir, "pack.part", "pack");
    assert_eq!(answers.recv().unwrap().0, Ok(()));
    assert_eq!(space.changes.waits.load(Ordering::Acquire), 0);
    assert_eq!(space.tries.load(Ordering::Acquire), 2);
    assert_eq!(puts.object(&format!("{DIR}/pack")).unwrap(), b"between");
}

/// A commit whose file is not released by the deadline answers `EBUSY` from its thread, uploads
/// nothing and leaves the file staged for the same rename.
#[test]
fn a_commit_not_released_by_the_deadline_is_ebusy() {
    let rig = SpaceRig::new("put-rename-deadline");
    let (puts, clock) = (Arc::clone(&rig.puts), rig.rig.clock.clone());
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let (id, fh) = closing(&space, dir, "pack.part", b"held");
    let answers = rename(&space, dir, "pack.part", "pack");
    until(|| space.changes.waits.load(Ordering::Acquire) > 0);
    clock.advance(u64::try_from(DEADLINE.as_millis()).unwrap());
    space.changes.notify(&[Topic::File(id)]);
    assert_eq!(answers.recv().unwrap().0, Err(Errno::BUSY));
    assert_eq!(puts.count(), 0);
    assert_eq!(space.lookup(dir, "pack.part").unwrap().unwrap().size, 4);
    space.release(fh);
    assert_eq!(
        rename(&space, dir, "pack.part", "pack").recv().unwrap().0,
        Ok(())
    );
}

/// A commit whose file is still open (never flushed) is answered `EBUSY` before `rename_with`
/// returns, on the calling thread, with no wait and no thread made.
#[test]
fn a_commit_of_a_file_still_open_is_answered_at_once_on_the_calling_thread() {
    let rig = SpaceRig::new("put-rename-open");
    let (_log, warnings) = warnings();
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let (attr, fh) = space.create(dir, "pack.part").unwrap();
    let _ = space.write((attr.ino, fh), 0, b"open").unwrap();
    let answers = rename(&space, dir, "pack.part", "pack");
    let (result, on, name) = answers.try_recv().unwrap();
    assert_eq!(result, Err(Errno::BUSY));
    assert_eq!(on, thread::current().id());
    assert_ne!(name.as_deref(), Some("fs-commit"));
    assert_eq!(space.changes.waits.load(Ordering::Acquire), 0);
    assert_eq!(space.tries.load(Ordering::Acquire), 0);
    assert_eq!(
        warnings.text(),
        format!("tierstore fs: the commit of {DIR}/pack: a handle of the file is open\n")
    );
}

/// What is refused without a wait is answered on the calling thread: two directories (`EPERM`), a
/// directory the kernel was not told of (`ESTALE`), a name that is there with no-replace
/// (`EEXIST`), a rename to a `.part` name that is staged (`EEXIST`), a name that is not there
/// (`ENOENT`); and a rename of the name alone is done there too.
#[test]
fn what_is_decided_at_once_is_answered_on_the_calling_thread() {
    let rig = SpaceRig::new("put-rename-at-once");
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    staged(&space, dir, "a.part", b"a");
    staged(&space, dir, "b.part", b"b");
    let here = thread::current().id();
    let mut answered = Vec::new();
    let mut ask = |from: &str, to: &str, noreplace: bool, (parent, new_parent): (u64, u64)| {
        let (send, answers) = mpsc::channel();
        space.rename_with((parent, from), (new_parent, to), noreplace, move |result| {
            send.send((result, thread::current().id())).unwrap();
        });
        answered.push(answers.try_recv().unwrap());
    };
    let unknown = super::super::dir_ino("cas/b/never-told");
    ask("a.part", "x", false, (dir, dir + 1));
    ask("a.part", "x", false, (unknown, unknown));
    ask("a.part", "b.part", true, (dir, dir));
    ask("a.part", "b.part", false, (dir, dir));
    ask("gone.part", "x", false, (dir, dir));
    ask("a.part", "c.part", false, (dir, dir));
    assert_eq!(
        answered,
        [
            (Err(Errno::PERM), here),
            (Err(Errno::STALE), here),
            (Err(Errno::EXIST), here),
            (Err(Errno::EXIST), here),
            (Err(Errno::NOENT), here),
            (Ok(()), here),
        ]
    );
    assert_eq!(space.changes.waits.load(Ordering::Acquire), 0);
}

/// An upload that fails answers the store's errno from the commit's thread and leaves the file
/// staged; a commit of the same key that waited for it then goes on and gets its own answer.
#[test]
fn a_failed_upload_wakes_the_commit_of_its_key() {
    let rig = SpaceRig::new("put-rename-key");
    let puts = Arc::clone(&rig.puts);
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    staged(&space, dir, "a.part", b"first");
    staged(&space, dir, "b.part", b"second");
    puts.hold();
    let first = rename(&space, dir, "a.part", "k");
    puts.wait_for_held(1);
    let second = rename(&space, dir, "b.part", "k");
    until(|| space.changes.waits.load(Ordering::Acquire) > 0);
    puts.script.lock().unwrap().push_back(failed("checksum"));
    puts.let_go();
    assert_eq!(first.recv().unwrap().0, Err(Errno::BADMSG));
    assert_eq!(second.recv().unwrap().0, Ok(()));
    assert_eq!(space.lookup(dir, "a.part").unwrap().unwrap().size, 5);
    assert_eq!(puts.object(&format!("{DIR}/k")).unwrap(), b"second");
}

/// A rename of the name alone, made while the commit of the file uploads, waits for that commit
/// on a thread of its own and ends with what the store then says: the file is gone from its
/// `.part` name (`ENOENT`).
#[test]
fn a_rename_of_the_name_waits_for_the_commit_of_the_file() {
    let rig = SpaceRig::new("put-rename-part-waits");
    let puts = Arc::clone(&rig.puts);
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    staged(&space, dir, "a.part", b"bytes");
    puts.hold();
    let commit = rename(&space, dir, "a.part", "a");
    puts.wait_for_held(1);
    let renamed = rename(&space, dir, "a.part", "b.part");
    until(|| space.changes.waits.load(Ordering::Acquire) > 0);
    assert_eq!(renamed.try_recv().unwrap_err(), TryRecvError::Empty);
    puts.let_go();
    assert_eq!(commit.recv().unwrap().0, Ok(()));
    assert_eq!(renamed.recv().unwrap().0, Err(Errno::NOENT));
}

/// A commit whose thread cannot be made runs on the calling thread, which answers when it is done.
#[test]
fn a_commit_whose_thread_cannot_be_made_runs_on_the_calling_thread() {
    let rig = SpaceRig::new("put-rename-inline");
    let (_log, warnings) = warnings();
    let puts = Arc::clone(&rig.puts);
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    staged(&space, dir, "pack.part", b"inline");
    space.no_threads.store(true, Ordering::Release);
    let answers = rename(&space, dir, "pack.part", "pack");
    let (result, on, name) = answers.try_recv().unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(on, thread::current().id());
    assert_ne!(name.as_deref(), Some("fs-commit"));
    assert_eq!(puts.object(&format!("{DIR}/pack")).unwrap(), b"inline");
    assert_eq!(
        warnings.text(),
        "tierstore fs: no thread for a commit, it runs on the request's: no thread was made\n"
    );
}

/// A commit that ends in an error gives its answer once (the digest of its name is not that of its
/// bytes).
#[test]
fn a_commit_that_ends_in_an_error_answers_once() {
    let rig = SpaceRig::new("put-rename-error");
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let name = format!(
        "pack.sha256-{}.part",
        super::super::super::testing::sha(b"x")
    );
    staged(&space, dir, &name, b"bytes");
    let answers = rename(&space, dir, &name, "pack");
    assert_eq!(answers.recv().unwrap().0, Err(Errno::BADMSG));
    assert_eq!(answers.recv().unwrap_err(), mpsc::RecvError);
}

/// A hand-over's drain of the commits waits for a commit whose upload is still running, and
/// returns only once the commit has answered its request and uploaded the file: the request is
/// this process's to answer, and the journal the commit writes is what the successor reads. The
/// upload is let go from the gap where the drain is about to wait. Catches a hand-over that leaves
/// a job's commit to a process that is about to end.
#[test]
fn a_drain_waits_for_a_commit_that_uploads_and_its_answer() {
    let rig = SpaceRig::new("put-rename-drain");
    let puts = Arc::clone(&rig.puts);
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    staged(&space, dir, "pack.part", b"in flight");
    puts.hold();
    let answers = rename(&space, dir, "pack.part", "pack");
    puts.wait_for_held(1);
    assert_eq!(space.commits_running(), 1);
    let waited = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (gate, count) = (Arc::clone(&puts), Arc::clone(&waited));
    *space.before_drain.lock().unwrap() = Some(Arc::new(move || {
        let _ = count.fetch_add(1, Ordering::AcqRel);
        gate.let_go();
    }));
    space.drain_commits();
    assert_eq!(waited.load(Ordering::Acquire), 1);
    assert_eq!(space.commits_running(), 0);
    let (result, _, name) = answers.try_recv().unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(name.as_deref(), Some("fs-commit"));
    assert_eq!(puts.object(&format!("{DIR}/pack")).unwrap(), b"in flight");
}

/// A drain with no commit running returns at once, without waiting; a commit answered at once on
/// the calling thread, one that ran there because its thread could not be made, and one that ended
/// in an error are not left counted. Catches a count that a commit not run on a thread of its own
/// leaves behind, which would hold every later hand-over for ever.
#[test]
fn a_drain_returns_at_once_when_no_commit_runs() {
    let rig = SpaceRig::new("put-rename-drain-none");
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let waited = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = Arc::clone(&waited);
    *space.before_drain.lock().unwrap() = Some(Arc::new(move || {
        let _ = count.fetch_add(1, Ordering::AcqRel);
    }));
    space.drain_commits();
    staged(&space, dir, "a.part", b"a");
    let renamed = rename(&space, dir, "a.part", "b.part");
    assert_eq!(renamed.try_recv().unwrap().0, Ok(()));
    let name = format!(
        "bad.sha256-{}.part",
        super::super::super::testing::sha(b"x")
    );
    staged(&space, dir, &name, b"bytes");
    assert_eq!(
        rename(&space, dir, &name, "bad").recv().unwrap().0,
        Err(Errno::BADMSG)
    );
    staged(&space, dir, "c.part", b"inline");
    space.no_threads.store(true, Ordering::Release);
    assert_eq!(
        rename(&space, dir, "c.part", "c").try_recv().unwrap().0,
        Ok(())
    );
    until(|| space.commits_running() == 0);
    space.drain_commits();
    assert_eq!(waited.load(Ordering::Acquire), 0);
}

/// A hold for a hand-over ends at once a commit that waits for the release of its file, `EBUSY`:
/// the readers have stopped and the release goes to the successor, so it would never reach this
/// process, and the drain of the commits, which the successor's start waits for, would otherwise
/// last the whole commit deadline. Catches a hand-over held up by a commit that cannot end before
/// its deadline.
#[test]
fn a_hold_ends_a_commit_that_waits_for_a_release_at_once() {
    let rig = SpaceRig::new("put-rename-held");
    let space = Arc::new(rig.space);
    let dir = nonce(&space);
    let (_, _fh) = closing(&space, dir, "pack.part", b"never released");
    let answers = rename(&space, dir, "pack.part", "pack");
    until(|| space.changes.waits.load(Ordering::Acquire) > 0);
    assert_eq!(answers.try_recv().unwrap_err(), TryRecvError::Empty);
    space.hold(true);
    assert_eq!(answers.recv().unwrap().0, Err(Errno::BUSY));
    space.drain_commits();
    assert_eq!(space.commits_running(), 0);
    // While held, a new commit of a file still being closed does not wait either.
    let (_, _other) = closing(&space, dir, "late.part", b"late");
    assert_eq!(
        rename(&space, dir, "late.part", "late").recv().unwrap().0,
        Err(Errno::BUSY)
    );
}
