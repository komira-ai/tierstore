//! The put ingest of a Mac (`local_cache.put_ingest`): the put directory of a job made of a plain
//! local directory, with no file system of tierstore's under it. A job's put directory is
//! `<root>/cas/b/<build>/<nonce>`; the job's task program commits its object there as it does into
//! the put directory of Linux, `pack.part` renamed to `pack.sha256-<hex>.part` (which declares the
//! SHA-256 the object's bytes must have), and that renamed to `pack`. The ingest watches the root,
//! and for each committed name whose digest was declared it puts the file's bytes, read once and
//! hashed as they pass, as the create-only object `cas/b/<build>/<nonce>/<name>` that names that
//! digest (the put the Linux put directory's commit makes, through the cache), then removes the
//! file and, when it is empty, the job's directory. What an operator sets, and what the job runtime
//! must do, is in the operations document (`crates/tierstore/docs/operations.md`, "The put
//! ingest").
//!
//! # What is seen, and when
//!
//! A plain directory keeps no record of the name between the task program's two renames, so the
//! watcher ([`Watcher`]: FSEvents on macOS, inotify on Linux) is what sees it: every path an event
//! names is looked at, and a `.sha256-<hex>.part` name is recorded as the declaration of the
//! committed name before it ([`ledger`]). A whole scan of the root ([`scan`]) runs at the start,
//! every [`SCAN_EVERY`], and whenever the watcher says it lost events or a directory appeared: it
//! finds the committed files no event led to, the declarations of `.part` names still there, and
//! the puts to try again.
//!
//! # What is stored, and what is refused
//!
//! - Only a regular file with one link directly in a job's directory, under a name without `.part`,
//!   whose digest was declared, is put ([`store`]); its key is its path under the root.
//! - Bytes that are not the declared digest (truncated, longer or changed), a key that holds other
//!   bytes, a size the upstream refuses, a link and a hard link are refused: nothing is stored, and
//!   the file is removed. So is a committed file whose digest was not declared within [`GRACE_MS`]
//!   of its first finding (one committed while no instance watched, or whose events were lost).
//! - A file that cannot be opened or read, and a put the upstream did not take, are left as they
//!   are and tried again at the next scan.
//! - `.part` names are never stored. A job's directory that has not changed for a day
//!   ([`ABANDONED_MS`]: no name made, renamed or removed in it; a write into a file it holds does
//!   not change it) is taken for a job that ended without a commit and removed with what it holds,
//!   and a build's directory left empty for as long.
//!
//! Only the instance that holds the lock of the cache directory ingests, as only it binds the fill
//! socket: one that loses the lock stops (its running puts are dropped, their files left) and keeps
//! the declarations it saw for when it takes the lock again; its successor finds the files with its
//! first scan, and refuses those whose declaration it did not see. At the halt the puts that are
//! running are let finish; none is started.

pub(in crate::server) mod config;
mod ledger;
mod names;
mod scan;
mod store;
#[cfg(target_os = "linux")]
mod watch_linux;
#[cfg(target_os = "macos")]
mod watch_macos;

#[cfg(test)]
mod ledger_tests;
#[cfg(test)]
mod names_tests;
#[cfg(test)]
mod scan_tests;
#[cfg(test)]
mod serve_edge_tests;
#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod testing;
#[cfg(target_os = "linux")]
#[cfg(test)]
mod watch_linux_tests;

use std::collections::BTreeSet;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{self, JoinSet};

use super::local_cache::{LocalCache, Mode};
use crate::{Clock, Error, PutOutcome, SystemClock, TreeDigest};
use ledger::{FileId, Ledger, Verdict};
use names::{JobDir, Name, Place, name_of, place};
pub(in crate::server) use store::Refusal;
use store::{Identity, Outcome, identity_of, put_file};
#[cfg(target_os = "linux")]
use watch_linux::Watcher;
#[cfg(target_os = "macos")]
use watch_macos::Watcher;

/// How long a committed file whose digest was not declared waits for its declaration before it is
/// refused: far longer than an event takes to arrive.
pub(super) const GRACE_MS: u64 = 60_000;

/// The time between two whole scans of the root.
const SCAN_EVERY: Duration = Duration::from_secs(30);

/// How long a job's directory, and a build's, stays unchanged before it is removed.
const ABANDONED_MS: u64 = 24 * 60 * 60 * 1000;

/// The most puts that run at once.
const MOST_PUTS: usize = 8;

/// What a watcher saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    /// A path, relative to the root, that changed: a name made, or renamed from or to.
    Path(PathBuf),
    /// A directory, relative to the root, that appeared or changed: what is in it may not have
    /// been seen.
    Dir(PathBuf),
    /// Events were dropped.
    Lost,
}

/// What the ingest did, for the tests to wait on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum Report {
    /// The ingest took the root: its watcher is in place (or could not be made) and its first scan
    /// is done.
    Watching,
    /// A whole scan of the root is done.
    Scanned,
    /// The put of the object `key` started.
    Putting { key: String },
    /// The object `key` is stored: `created` when this put made it.
    Stored { key: String, created: bool },
    /// The file of `key` is refused and removed.
    Refused { key: String, why: Refusal },
    /// The file of `key` is left, to be tried again.
    Left { key: String },
    /// The directory, relative to the root, was removed as abandoned.
    Abandoned { dir: PathBuf },
}

/// The ingest of one root into one cache.
pub(in crate::server) struct PutIngest {
    cache: LocalCache,
    root: PathBuf,
    clock: Arc<dyn Clock>,
    reports: Option<mpsc::UnboundedSender<Report>>,
}

impl PutIngest {
    /// The ingest of the root `root` into `cache`, on the clock of the machine.
    pub(in crate::server) fn new(cache: LocalCache, root: PathBuf) -> Self {
        Self {
            cache,
            root,
            clock: Arc::new(SystemClock),
            reports: None,
        }
    }

    /// The same, on `clock`, telling what it does to `reports`.
    #[cfg(test)]
    pub(in crate::server) fn telling(
        self,
        clock: Arc<dyn Clock>,
        reports: mpsc::UnboundedSender<Report>,
    ) -> Self {
        Self {
            clock,
            reports: Some(reports),
            ..self
        }
    }

    /// Ingests until `halt` is notified, while the cache holds the lock of its directory.
    pub(in crate::server) async fn serve(self, halt: Arc<Notify>) {
        let mut modes = self.cache.watch_mode();
        let mut ledger = Ledger::default();
        loop {
            let writable = *modes.borrow_and_update() == Mode::ReadWrite;
            if writable {
                let mut run = Run::new(&self, std::mem::take(&mut ledger));
                let halted = run.run(&halt, &mut modes).await;
                ledger = run.ledger;
                if halted {
                    break;
                }
                // The puts that ran were dropped with the run, and their files left; the
                // declarations are kept for the next run.
                ledger.drop_running();
                continue;
            }
            tokio::select! {
                biased;
                () = halt.notified() => break,
                _ = modes.changed() => {}
            }
        }
    }

    fn report(&self, report: Report) {
        if let Some(reports) = &self.reports {
            let _ = reports.send(report);
        }
    }
}

/// What a put that ended came to: the file, its key, the outcome and the identity of the file it
/// opened.
type Ended = (FileId, String, Outcome, Option<Identity>);

/// One run of the ingest, while the cache holds the lock.
struct Run<'a> {
    ingest: &'a PutIngest,
    ledger: Ledger,
    puts: JoinSet<Ended>,
    /// The files to put when a put comes free.
    queue: BTreeSet<FileId>,
    watcher: Option<Watcher>,
}

/// The watcher of `root`. On macOS it is made on a blocking thread: it waits for its stream to
/// tell a probe.
async fn make_watcher(root: PathBuf) -> io::Result<Watcher> {
    #[cfg(target_os = "macos")]
    {
        let made = tokio::task::spawn_blocking(move || Watcher::new(&root)).await;
        made.map_err(io::Error::other)?
    }
    #[cfg(target_os = "linux")]
    {
        Watcher::new(&root)
    }
}

/// The next events of `watcher`, or never without one.
async fn next_seen(watcher: Option<&mut Watcher>, seen: &mut Vec<Seen>) -> io::Result<()> {
    match watcher {
        Some(watcher) => watcher.next(seen).await,
        None => std::future::pending().await,
    }
}

impl<'a> Run<'a> {
    fn new(ingest: &'a PutIngest, ledger: Ledger) -> Self {
        Self {
            ingest,
            ledger,
            puts: JoinSet::new(),
            queue: BTreeSet::new(),
            watcher: None,
        }
    }

    /// Runs until the halt (true, once the running puts have ended) or a change of the mode
    /// (false: the puts are dropped, and the files left).
    async fn run(&mut self, halt: &Notify, modes: &mut watch::Receiver<Mode>) -> bool {
        self.watcher = self.watch().await;
        self.scan().await;
        if self.watcher.is_some() {
            let shown = self.ingest.root.display();
            tracing::info!("tierstore: the put ingest watches {shown}");
        }
        self.ingest.report(Report::Watching);
        let mut every = tokio::time::interval(SCAN_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let _ = every.tick().await;
        loop {
            let mut seen = Vec::new();
            tokio::select! {
                biased;
                () = halt.notified() => {
                    while let Some(ended) = self.puts.join_next().await {
                        self.ended(ended);
                    }
                    return true;
                }
                _ = modes.changed() => return false,
                Some(ended) = self.puts.join_next() => self.ended(ended),
                got = next_seen(self.watcher.as_mut(), &mut seen) => match got {
                    Ok(()) => self.take(seen).await,
                    Err(why) => {
                        tracing::warn!("tierstore: the put ingest's watcher ended: {why}");
                        self.watcher = None;
                    }
                },
                _ = every.tick() => {
                    if self.watcher.is_none() {
                        self.watcher = self.watch().await;
                    }
                    self.scan().await;
                }
            }
        }
    }

    /// The watcher of the root, or none, said, when it cannot be made.
    async fn watch(&self) -> Option<Watcher> {
        let root = &self.ingest.root;
        match make_watcher(root.clone()).await {
            Ok(watcher) => Some(watcher),
            Err(why) => {
                let shown = root.display();
                tracing::warn!("tierstore: the put ingest cannot watch {shown}: {why}");
                None
            }
        }
    }

    /// Takes what the watcher saw.
    async fn take(&mut self, seen: Vec<Seen>) {
        let mut rescan = false;
        for seen in seen {
            let rel = match seen {
                Seen::Lost | Seen::Dir(_) => {
                    rescan = true;
                    continue;
                }
                Seen::Path(rel) => rel,
            };
            let Place::File(dir, name) = place(&rel) else {
                continue;
            };
            match name_of(&name) {
                Name::Declares { name, sha256 } => self.declare(&dir, &name, sha256),
                Name::Committed => self.consider((dir, name)),
                Name::Part => {}
            }
        }
        if rescan {
            self.scan().await;
        }
    }

    /// Records a declaration, and puts the file it is for if it was found waiting for it.
    fn declare(&mut self, dir: &JobDir, name: &str, sha256: TreeDigest) {
        if self.ledger.declare(dir, name, sha256) {
            self.consider((dir.clone(), name.to_owned()));
        }
    }

    /// The path of `id`.
    fn path_of(&self, id: &FileId) -> PathBuf {
        self.ingest.root.join(id.0.rel()).join(&id.1)
    }

    /// Looks at the committed file `id`: puts it, refuses it, or leaves it for later.
    fn consider(&mut self, id: FileId) {
        let there = std::fs::symlink_metadata(self.path_of(&id));
        if !there.is_ok_and(|meta| !meta.is_dir()) {
            return;
        }
        if self.puts.len() >= MOST_PUTS {
            let _ = self.queue.insert(id);
            return;
        }
        match self.ledger.verdict(&id, self.ingest.clock.now()) {
            Verdict::Store(sha256) => self.start(id, sha256),
            Verdict::Refuse => {
                let key = id.0.key(&id.1);
                self.refused(&id, &key, &Refusal::NoDigest, None);
            }
            Verdict::Running | Verdict::Wait => {}
        }
    }

    /// Starts the put of `id`.
    fn start(&mut self, id: FileId, sha256: TreeDigest) {
        let key = id.0.key(&id.1);
        self.ingest.report(Report::Putting { key: key.clone() });
        let cache = self.ingest.cache.clone();
        let path = self.path_of(&id);
        let _ = self.puts.spawn(async move {
            // A put that panics ends as one the upstream did not take: its file is left, and its
            // entry in the ledger freed, to be put again.
            let put = AssertUnwindSafe(put_file(&cache, &key, sha256, &path)).catch_unwind();
            let (outcome, identity) = put.await.unwrap_or_else(|_| {
                let why = Error::Other("the put panicked".to_owned());
                (Outcome::Again(why), None)
            });
            (id, key, outcome, identity)
        });
    }

    /// Takes the end of a put, and starts the next one waiting.
    fn ended(&mut self, ended: Result<Ended, task::JoinError>) {
        match ended {
            Ok(ended) => self.outcome(ended),
            // Only a task that is aborted ends so (a panic is its outcome, above), and the puts
            // are aborted only with their run.
            Err(failed) => tracing::warn!("tierstore: a put of the put ingest ended: {failed}"),
        }
        while self.puts.len() < MOST_PUTS {
            let Some(next) = self.queue.pop_first() else {
                break;
            };
            self.consider(next);
        }
    }

    /// Takes the outcome of a put.
    fn outcome(&mut self, (id, key, outcome, identity): Ended) {
        match outcome {
            Outcome::Stored(outcome) => {
                self.ledger.done(&id, true);
                self.remove(&id, identity);
                let created = outcome == PutOutcome::Created;
                tracing::info!("tierstore: the put ingest stored {key}");
                self.ingest.report(Report::Stored { key, created });
            }
            Outcome::Refused(why) => {
                self.ledger.done(&id, true);
                self.refused(&id, &key, &why, identity);
            }
            Outcome::Gone => self.ledger.done(&id, true),
            Outcome::Unreadable(why) => {
                self.ledger.done(&id, false);
                tracing::warn!("tierstore: the put ingest cannot read {key}, left: {why}");
                self.ingest.report(Report::Left { key });
            }
            Outcome::Again(why) => {
                self.ledger.done(&id, false);
                tracing::warn!("tierstore: the put ingest's put of {key} failed, left: {why}");
                self.ingest.report(Report::Left { key });
            }
        }
    }

    /// Refuses the file `id` (of `key`) for `why`: removes it (the file of `identity` only, if
    /// given), and says so.
    fn refused(&mut self, id: &FileId, key: &str, why: &Refusal, identity: Option<Identity>) {
        self.remove(id, identity);
        let shown = self.path_of(id);
        let shown = shown.display();
        tracing::warn!("tierstore: the put ingest refused {shown}: {why}");
        let key = key.to_owned();
        let why = why.clone();
        self.ingest.report(Report::Refused { key, why });
    }

    /// Removes the file `id` (if `identity` is given, only while the path leads to that file: one
    /// the job put there since is left for the next scan), and its directory if that is empty now.
    fn remove(&self, id: &FileId, identity: Option<Identity>) {
        let path = self.path_of(id);
        if identity.is_none_or(|identity| identity_of(&path) == Some(identity)) {
            let _ = std::fs::remove_file(&path);
        }
        if !self.ledger.busy(&id.0) {
            let _ = std::fs::remove_dir(self.ingest.root.join(id.0.rel()));
        }
    }

    /// A whole scan of the root (see the module docs).
    async fn scan(&mut self) {
        let root = self.ingest.root.clone();
        let found = tokio::task::spawn_blocking(move || scan::scan(&root)).await;
        let found = found.unwrap_or_default();
        let now = self.ingest.clock.now();
        let old = |changed: u64| now.saturating_sub(changed) >= ABANDONED_MS;
        let live = found.jobs.keys().cloned().collect();
        self.ledger.keep_only(&live);
        for (dir, job) in found.jobs {
            for (name, sha256) in job.declared {
                let _ = self.ledger.declare(&dir, &name, sha256);
            }
            if old(job.changed) && !self.ledger.busy(&dir) {
                self.abandon(&dir.rel());
                continue;
            }
            for name in job.committed {
                self.consider((dir.clone(), name));
            }
        }
        for (build, changed) in found.builds {
            if old(changed) && std::fs::remove_dir(self.ingest.root.join(&build)).is_ok() {
                self.ingest.report(Report::Abandoned { dir: build });
            }
        }
        self.ingest.report(Report::Scanned);
    }

    /// Removes the abandoned directory `rel` and what it holds.
    fn abandon(&self, rel: &Path) {
        let path = self.ingest.root.join(rel);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                let shown = path.display();
                tracing::info!("tierstore: the put ingest removed the abandoned {shown}");
                self.ingest.report(Report::Abandoned {
                    dir: rel.to_owned(),
                });
            }
            Err(why) => {
                let shown = path.display();
                tracing::warn!("tierstore: the put ingest cannot remove {shown}: {why}");
            }
        }
    }
}
