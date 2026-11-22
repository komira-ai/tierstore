//! The rename of the put directory: a `.part` name to another, a change of the name; to a final
//! name, the commit (see the module above). A frontend gives [`PutSpace::rename_with`] the answer
//! to send, which is called once, with the result:
//!
//! - what is decided at once (two directories, a name not there, `RENAME_NOREPLACE`, a rename of
//!   the name alone, a file still open) is answered on the calling thread;
//! - a commit, which waits for the release of the file's handle (FUSE sends it after the close
//!   returns, so it is another request) or for another commit of the file or key, and then
//!   uploads the file (up to the commit deadline), runs on a thread of its own, `fs-commit`, so
//!   that it holds none of the threads that serve the requests it waits for; if that thread cannot
//!   be made, it runs on the calling thread.
//!
//! A commit's thread answers its request itself, after the readers that read it may have stopped,
//! so the stop of the readers does not wait for it. A hand-over does ([`PutSpace::drain_commits`]):
//! the request is this process's to answer, the kernel does not send it again to the successor,
//! and the commit writes the journal and the stage, which the successor reads once it serves. Each
//! commit ends by its deadline (`fs.put.commit_deadline_secs`), the upload included; one that waits
//! for the release of its file ends at once, `EBUSY`, when the hand-over holds the space
//! ([`PutSpace::hold`]): the release goes to the successor and would never reach it.

use std::sync::{Arc, Condvar, Mutex};

use rustix::io::Errno;

use super::super::changes::Topic;
use super::super::commit::{HANDLE_OPEN, KEY_RUNNING};
use super::super::error::PutError;
use super::super::names::{PutDir, is_part};
use super::super::store::{Rename, StagedStore};
use super::PutSpace;
use crate::server::treefs::locked;

/// The count of the commits whose thread runs, and what wakes a wait for none.
#[derive(Default)]
pub(super) struct Commits {
    count: Mutex<usize>,
    idle: Condvar,
}

/// A commit counted in [`Commits`] until it is dropped: after its answer, or when its thread could
/// not be made.
struct Counted(Arc<Commits>);

impl Counted {
    fn begin(commits: &Arc<Commits>) -> Self {
        *locked(&commits.count) += 1;
        Self(Arc::clone(commits))
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        let mut count = locked(&self.0.count);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.0.idle.notify_all();
        }
    }
}

/// A rename that passed the checks that need no wait.
#[derive(Debug)]
struct Plan {
    dir: PutDir,
    /// The path of the directory.
    parent: String,
    /// The `.part` name.
    name: String,
    /// The name it is renamed to.
    new: String,
    /// The key of the new name.
    key: String,
    /// The number of the staged file.
    id: u64,
    /// When the commit ends, on the clock, in milliseconds.
    deadline: u64,
}

impl PutSpace {
    /// Renames `name` of `parent` to `new` of `new_parent` and calls `done` with the result, once,
    /// on this thread or, for a commit that has to wait or upload, on the thread of its own that
    /// runs it. `noreplace` refuses a `new` that is there (`RENAME_NOREPLACE`).
    ///
    /// The result is `EPERM` for two directories or one not at or under a prefix, `EEXIST` for a
    /// `new` that is there with `noreplace`, `EBUSY` for a file still open, or still busy at the
    /// deadline, `EIO` when the upload did not finish by it, and the errnos of the store's rename
    /// and of the upload's answer.
    pub(in crate::server) fn rename_with<F>(
        self: &Arc<Self>,
        from: (u64, &str),
        to: (u64, &str),
        noreplace: bool,
        done: F,
    ) where
        F: FnOnce(Result<(), Errno>) + Send + 'static,
    {
        let plan = match self.plan(from, to, noreplace) {
            Ok(plan) => plan,
            Err(error) => return done(Err(error)),
        };
        let first = if is_part(&plan.new) {
            self.store
                .rename_part(&plan.dir, &plan.name, &plan.new)
                .map(|()| true)
        } else {
            self.store
                .probe_commit(&plan.dir, &plan.name, &plan.new)
                .map(|()| false)
        };
        match first {
            Ok(true) => done(Ok(())),
            Ok(false) => self.hand_off(plan, done),
            Err(PutError::Busy(why)) if self.waits_for(&plan, why) => self.hand_off(plan, done),
            Err(PutError::Busy(why)) => done(Err(Self::refuse(&plan, why))),
            Err(error) => done(Err(super::errno(&error))),
        }
    }

    /// The same, answered on the calling thread, which waits for as long as the rename does.
    #[cfg(test)]
    pub(in crate::server) fn rename(
        &self,
        from: (u64, &str),
        to: (u64, &str),
        noreplace: bool,
    ) -> Result<(), Errno> {
        let plan = self.plan(from, to, noreplace)?;
        self.run(&plan)
    }

    /// The checks of a rename that need no wait, and the deadline of a commit from now.
    fn plan(
        &self,
        (parent, name): (u64, &str),
        (new_parent, new): (u64, &str),
        noreplace: bool,
    ) -> Result<Plan, Errno> {
        if parent != new_parent {
            return Err(Errno::PERM);
        }
        let parent = self.dir_path(parent)?;
        let dir = self.writable(&parent)?;
        if noreplace && self.store.find(&dir, new).is_some() {
            return Err(Errno::EXIST);
        }
        let key =
            StagedStore::checked_names(&dir, name, new).map_err(|error| super::errno(&error))?;
        let (id, _) = self.store.find(&dir, name).ok_or(Errno::NOENT)?;
        let deadline = self
            .clock
            .now()
            .saturating_add(super::millis(self.deadline));
        Ok(Plan {
            dir,
            parent,
            name: name.to_owned(),
            new: new.to_owned(),
            key,
            id,
            deadline,
        })
    }

    /// Whether a rename that the store found busy for `why` is waited for: a file still open is
    /// not (only one being closed, whose release FUSE sends after the close returns), nor is a
    /// busy file waited for by a job that holds it open; another commit of the file or key is.
    fn waits_for(&self, plan: &Plan, why: &str) -> bool {
        why != HANDLE_OPEN || self.closing(plan.id)
    }

    /// Whether a commit busy for `why` waits for a release this process will not be sent: the
    /// space is held for a successor, which reads every request from the stop of the readers on.
    fn release_never_comes(&self, why: &str) -> bool {
        why == HANDLE_OPEN && self.held.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The answer for a rename refused as busy at once.
    fn refuse(plan: &Plan, why: &str) -> Errno {
        tracing::warn!(
            "tierstore fs: the commit of {}/{}: {why}",
            plan.parent,
            plan.new
        );
        Errno::BUSY
    }

    /// Runs the rename on a thread of its own, or on this one if that cannot be made.
    fn hand_off<F>(self: &Arc<Self>, plan: Plan, done: F)
    where
        F: FnOnce(Result<(), Errno>) + Send + 'static,
    {
        // The job stays here until a thread takes it, so that a thread that is not made leaves it.
        let job = Arc::new(Mutex::new(Some((plan, done))));
        let (space, slot) = (Arc::clone(self), Arc::clone(&job));
        let counted = Counted::begin(&self.commits);
        let spawned = if self.no_threads.load(std::sync::atomic::Ordering::Acquire) {
            Err(std::io::Error::other("no thread was made"))
        } else {
            std::thread::Builder::new()
                .name("fs-commit".to_owned())
                .spawn(move || {
                    // Counted until the answer is written.
                    let _counted = counted;
                    let _ = locked(&slot)
                        .take()
                        .map(|(plan, done)| done(space.run(&plan)));
                })
        };
        if let Err(error) = spawned {
            tracing::warn!(
                "tierstore fs: no thread for a commit, it runs on the request's: {error}"
            );
            let _ = locked(&job)
                .take()
                .map(|(plan, done)| done(self.run(&plan)));
        }
    }

    /// Waits until every commit that runs on a thread of its own has answered its request: for a
    /// hand-over, before the state the successor reads is written. Each ends by its deadline.
    pub(in crate::server) fn drain_commits(&self) {
        #[cfg(test)]
        if *locked(&self.commits.count) > 0 {
            let hook = locked(&self.before_drain).clone();
            let _ = hook.map(|hook| hook());
        }
        let _idle = self
            .commits
            .idle
            .wait_while(locked(&self.commits.count), |count| *count > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// The commits that run on a thread of their own and have not answered yet.
    #[cfg(test)]
    pub(in crate::server) fn commits_running(&self) -> usize {
        *locked(&self.commits.count)
    }

    /// Renames until it is done: waits while the file or the key is busy for a while, then
    /// uploads the file and records the answer.
    fn run(&self, plan: &Plan) -> Result<(), Errno> {
        let commit = loop {
            let seen = self.changes.seen();
            #[cfg(test)]
            let _ = self.tries.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            match self.store.rename(&plan.dir, &plan.name, &plan.new) {
                Ok(Rename::Renamed) => return Ok(()),
                Ok(Rename::Commit(commit)) => break commit,
                Err(PutError::Busy(why)) => {
                    if !self.waits_for(plan, why) || self.release_never_comes(why) {
                        return Err(Self::refuse(plan, why));
                    }
                    let topic = if why == KEY_RUNNING {
                        Topic::Key(&plan.key)
                    } else {
                        Topic::File(plan.id)
                    };
                    #[cfg(test)]
                    {
                        let hook = locked(&self.before_wait).clone();
                        let _ = hook.map(|hook| hook());
                    }
                    if !self
                        .changes
                        .wait(topic, seen, plan.deadline, self.clock.as_ref())
                    {
                        return Err(Self::refuse(plan, why));
                    }
                }
                Err(error) => {
                    // A commit that failed after it began has ended: tell the waits for it.
                    self.changes
                        .notify(&[Topic::File(plan.id), Topic::Key(&plan.key)]);
                    return Err(super::errno(&error));
                }
            }
        };
        let outcome = self.uploader.put(
            (commit.key(), commit.sha256(), commit.size()),
            &commit.path(),
            plan.deadline,
        );
        let finished = commit.finish(outcome).map_err(|error| super::errno(&error));
        self.changes
            .notify(&[Topic::File(plan.id), Topic::Key(&plan.key)]);
        finished
    }
}

#[cfg(test)]
#[path = "space_rename_tests.rs"]
mod tests;
