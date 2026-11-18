//! The backing table: for each blob that some open file is served from by kernel passthrough, the
//! backing id the kernel was given for it and how many opens use it (`docs/design/virtual-build-fs.md`
//! sections 6.4 and 7.1).
//!
//! The kernel binds an inode with an open passthrough file to one backing file for as long as any
//! open of it lives, and fails a second open that comes with another id. So a blob has one id for
//! as long as any open of it lives, in whatever tree and under whatever name, and the table is
//! where that is decided: an open finds the live id and counts itself on it, or is told to register
//! one (the first open), and every other open of that blob waits until it has. The last release
//! drops the id, which closes it.
//!
//! The table is written to `<runtime dir>/backing`, one line `<sha256> <id> <opens>` for each live
//! blob, by a rename, after each first open and each last release and without `fsync`: it has to
//! outlive the process and not the machine. Each change is numbered, and the file is written by a
//! thread of the table's own (`fs-backing`), outside the lock of the table and outside every lock a
//! request waits on: a write takes every change made before it, and the changes made while one
//! write is in progress are written together by the next. A first open returns once the file holds
//! its change or a later one: the kernel then has an open with that id, which a successor must know
//! (an inode open across the hand-over would otherwise get a new id and the open would fail with
//! `EIO`; linux E6, case 3b), and a process that stops before the write never answered the open,
//! which the kernel sends to the successor again. A last release does not wait for the write: its
//! id is closed before any write, so its wait would change nothing a successor reads. Every open
//! that waits for a write is woken by the end of that write, all at once, and none of them waits
//! for another's turn at a lock; so the requests of the mount do not queue behind the file. A
//! successor that takes over the connection needs the file before its first open, and reads it
//! ([`saved`], [`BackingTable::adopt`]). A table whose thread could not be started writes the file
//! in the thread of the open or release that changed it, before it returns.
//! The `opens` of a line is the count at that write, which is a lower bound of the opens there are
//! (an open of a blob that has an id is not written). A process that stops reading to hand the
//! connection over writes every line again with the exact count and a fourth word, `exact`
//! ([`BackingTable::write_exact`]); a successor trusts the count of such a line, and keeps the id
//! of any other for as long as the connection lives, since closing it on a count that may be short
//! could leave an open file of the blob beside an open of another id. A line may end in words a
//! later release adds; they are ignored. The fixture is `fs_backing_table.txt`.
//!
//! The releases of opens that an earlier process made reach the successor with the handles that
//! process gave out ([`BackingTable::release_foreign`]); the id of the blob is closed when the last
//! of them has gone.
//!
//! The ids are limited to `fs.max_backing_ids`. An open of a blob with no live id, while that many
//! ids are live or being registered, does not wait for a release (the release is a request that
//! needs a free worker, and every worker might be parked in such an open): it is a cached open,
//! counted in `spilled`. While any such open of the blob lives, every other open of it is cached
//! too, so that no inode has a passthrough open and a cached one at once.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

use crate::tree_manifest::TreeDigest;

use super::locked;

/// The name of the file in the runtime directory.
pub(super) const FILE: &str = "backing";

/// What closes the backing id when the last reference to it is dropped.
pub(super) type Guard = Arc<dyn Any + Send + Sync>;

/// A backing id the kernel was given, as an open is answered with it.
#[derive(Clone)]
pub(super) struct Backing {
    /// The number the kernel gave.
    pub id: u32,
    guard: Guard,
}

impl std::fmt::Debug for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backing").field("id", &self.id).finish()
    }
}

impl Backing {
    /// What closes the id, if it is a `T`: the frontend gives it a type when it registers the id.
    pub(super) fn guard<T: Any>(&self) -> Option<&T> {
        self.guard.downcast_ref()
    }
}

/// What an open calls to register the id of a cache file with the kernel.
pub(super) type Registrar<'a> = dyn FnMut(&File) -> io::Result<Registered> + 'a;

/// A backing id just registered.
pub(super) struct Registered {
    /// The number the kernel gave.
    pub id: u32,
    /// Closes the id when dropped.
    pub guard: Guard,
}

/// The opens of a blob that an earlier process made and that have not been released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Foreign {
    /// None.
    None,
    /// This many, exactly.
    Exact(u32),
    /// At least this many, perhaps more, since the earlier process did not write the count down
    /// at its end: the id is kept.
    Unknown(u32),
}

/// A blob with a live id.
struct Live {
    id: u32,
    guard: Guard,
    /// The opens of this process.
    opens: u32,
    foreign: Foreign,
}

/// A line of the file: a blob with a live id in an earlier process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Saved {
    /// The blob.
    pub blob: TreeDigest,
    /// Its backing id.
    pub id: u32,
    /// The opens, if the line says that the count is exact (otherwise at least this many).
    pub opens: u32,
    /// Whether the line says the count is exact.
    pub exact: bool,
}

/// The lines of the file at `path`: none for a file that is not there. A line that is not
/// `<sha256> <id> <opens>` is said and left out (its id stays open in the kernel until the
/// connection ends).
///
/// # Errors
/// What the system said when the file exists and cannot be read.
pub(super) fn saved(path: &Path) -> io::Result<Vec<Saved>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(why) => return Err(why),
    };
    let mut lines = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split(' ').collect();
        let parsed = match words[..] {
            [blob, id, opens, ref rest @ ..] => TreeDigest::from_hex(blob)
                .zip(id.parse().ok())
                .zip(opens.parse().ok())
                .map(|((blob, id), opens)| Saved {
                    blob,
                    id,
                    opens,
                    exact: rest.first() == Some(&"exact"),
                }),
            _ => None,
        };
        match parsed {
            Some(saved) => lines.push(saved),
            None => tracing::warn!("tierstore fs: a line of the backing table is not read: {line}"),
        }
    }
    Ok(lines)
}

#[derive(Default)]
struct Inner {
    live: HashMap<TreeDigest, Live>,
    /// The blobs whose id is being registered by an open that was told to.
    registering: HashSet<TreeDigest>,
    /// The cached opens of blobs that had no id when the limit was reached, by blob.
    spilled: HashMap<TreeDigest, u32>,
    /// The opens that wait for a registration.
    waiting: usize,
    /// The number of the latest change to the lines of the file.
    changes: u64,
}

/// A line of the file as the table holds it: the blob, its id, its opens, and whether that count
/// is known exactly.
type Line = (TreeDigest, u32, u32, bool);

/// The table.
pub(super) struct BackingTable {
    file: PathBuf,
    most: usize,
    inner: Mutex<Inner>,
    changed: Condvar,
    /// Held by the one thread that writes the file, never while `inner` is held.
    writing: Mutex<()>,
    /// The changes asked to be in the file and the ones it holds, which the writer thread shares.
    writes: Arc<Writes>,
}

/// The changes asked to be in the file and written there, shared by the table and its writer thread.
#[derive(Default)]
struct Writes {
    progress: Mutex<Progress>,
    /// Told when a change is asked to be written, and when the table is dropped.
    asked: Condvar,
    /// Told when a write ends.
    wrote: Condvar,
}

/// The state of the writes.
#[derive(Default)]
struct Progress {
    /// The number of the latest change asked to be in the file.
    asked: u64,
    /// The number of the latest change the file holds.
    written: u64,
    /// Whether the writer thread runs; if not, the thread that asks writes.
    writer: bool,
    /// Whether the table was dropped, which ends the writer thread.
    gone: bool,
    /// The writes made since the table was made, the first one included.
    #[cfg(test)]
    count: u64,
}

/// What an open of a blob is told to do.
pub(super) enum Slot {
    /// The blob has a live id, and this open is counted on it.
    Live(Backing),
    /// The blob has none: register one, and give it to [`Ticket::registered`].
    Register(Ticket),
    /// The blob has none and the limit is reached (or cached opens of it live): open it cached.
    /// This open is counted, and ended by [`BackingTable::release`] like a passthrough one.
    Spill,
}

/// The right to register the id of one blob, held until it is registered; dropped, it lets the
/// opens that wait for the blob go on.
pub(super) struct Ticket {
    table: Arc<BackingTable>,
    blob: TreeDigest,
}

impl BackingTable {
    /// An empty table, at most `most` ids, kept in `file`, which is written empty now.
    pub(super) fn new(file: PathBuf, most: usize) -> Arc<Self> {
        Self::made(file, most, Inner::default()).started()
    }

    /// A table that holds `inner`, its file not written yet and no writer thread started.
    fn made(file: PathBuf, most: usize, inner: Inner) -> Arc<Self> {
        Arc::new(Self {
            file,
            most,
            inner: Mutex::new(inner),
            changed: Condvar::new(),
            writing: Mutex::new(()),
            writes: Arc::default(),
        })
    }

    /// Writes the file as the table is now, and starts the writer thread.
    fn started(self: Arc<Self>) -> Arc<Self> {
        self.started_by(|write| {
            std::thread::Builder::new()
                .name("fs-backing".to_owned())
                .spawn(write)
                .map(drop)
        })
    }

    /// Writes the file as the table is now, and has `spawn` start the writer thread. A thread that
    /// cannot be started is said, and the table writes the file in the thread that changes it.
    fn started_by(
        self: Arc<Self>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> Arc<Self> {
        self.write(false);
        let writes = Arc::clone(&self.writes);
        let table = Arc::downgrade(&self);
        match spawn(Box::new(move || writer(&writes, &table))) {
            Ok(()) => locked(&self.writes.progress).writer = true,
            Err(why) => {
                let file = self.file.display();
                tracing::warn!(
                    "tierstore fs: no thread writes the backing table {file}, each change does: {why}"
                );
            }
        }
        self
    }

    /// A table as [`BackingTable::adopt`] makes it when its writer thread cannot be started.
    #[cfg(test)]
    pub(super) fn without_writer(
        file: PathBuf,
        most: usize,
        saved: &[Saved],
        wrap: &dyn Fn(u32) -> Guard,
    ) -> Arc<Self> {
        Self::adopted(file, most, saved, wrap)
            .started_by(|_| Err(io::Error::other("no thread for a test")))
    }

    /// A table of the ids an earlier process of this connection registered, `saved` (the lines of
    /// its file), at most `most` ids; `wrap` makes the guard that closes an id when it is dropped.
    /// More lines than `most` are all kept: they are open in the kernel.
    pub(super) fn adopt(
        file: PathBuf,
        most: usize,
        saved: &[Saved],
        wrap: &dyn Fn(u32) -> Guard,
    ) -> Arc<Self> {
        Self::adopted(file, most, saved, wrap).started()
    }

    /// A table of the ids `saved`, its file not written yet and no writer thread started.
    fn adopted(
        file: PathBuf,
        most: usize,
        saved: &[Saved],
        wrap: &dyn Fn(u32) -> Guard,
    ) -> Arc<Self> {
        let live = saved
            .iter()
            .map(|line| {
                let foreign = match (line.exact, line.opens) {
                    (true, 0) => Foreign::None,
                    (true, opens) => Foreign::Exact(opens),
                    (false, opens) => Foreign::Unknown(opens),
                };
                let live = Live {
                    id: line.id,
                    guard: wrap(line.id),
                    opens: 0,
                    foreign,
                };
                (line.blob, live)
            })
            .collect();
        let inner = Inner {
            live,
            ..Inner::default()
        };
        Self::made(file, most, inner)
    }

    /// Counts an open of `blob`: on its live id, or, if it has none and there is room for one, by
    /// the right to register it; as a cached open if there is no room. Waits only for another
    /// open's registration of the same blob, which ends with that open's own fill.
    pub(super) fn open(self: &Arc<Self>, blob: &TreeDigest) -> Slot {
        let mut inner = locked(&self.inner);
        loop {
            if let Some(live) = inner.live.get_mut(blob) {
                live.opens += 1;
                return Slot::Live(Backing {
                    id: live.id,
                    guard: Arc::clone(&live.guard),
                });
            }
            if let Some(count) = inner.spilled.get_mut(blob) {
                *count += 1;
                return Slot::Spill;
            }
            if !inner.registering.contains(blob) {
                if inner.live.len() + inner.registering.len() >= self.most {
                    let _ = inner.spilled.insert(*blob, 1);
                    return Slot::Spill;
                }
                let _ = inner.registering.insert(*blob);
                return Slot::Register(Ticket {
                    table: Arc::clone(self),
                    blob: *blob,
                });
            }
            inner.waiting += 1;
            inner = self
                .changed
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
            inner.waiting -= 1;
        }
    }

    /// Ends one open of `blob`; the last one drops its id.
    pub(super) fn release(&self, blob: &TreeDigest) {
        let mut inner = locked(&self.inner);
        if let Some(count) = inner.spilled.get_mut(blob) {
            *count -= 1;
            if *count == 0 {
                let _ = inner.spilled.remove(blob);
            }
            return;
        }
        let Some(live) = inner.live.get_mut(blob) else {
            return;
        };
        live.opens -= 1;
        let change = Self::drop_if_unused(&mut inner, blob);
        drop(inner);
        self.dropped(change);
    }

    /// Ends one open of `blob` that an earlier process made; the last one of all drops its id. An
    /// id whose earlier opens were not counted exactly is kept.
    pub(super) fn release_foreign(&self, blob: &TreeDigest) {
        let mut inner = locked(&self.inner);
        let Some(live) = inner.live.get_mut(blob) else {
            return;
        };
        live.foreign = match live.foreign {
            Foreign::Exact(opens) if opens > 1 => Foreign::Exact(opens - 1),
            Foreign::Exact(_) => Foreign::None,
            other => other,
        };
        let change = Self::drop_if_unused(&mut inner, blob);
        drop(inner);
        self.dropped(change);
    }

    /// Drops the id of `blob` if no open of this process or of an earlier one uses it, and gives
    /// the number of that change. The id is closed here, with the lock held, so that the ids open
    /// in the kernel are never more than `most`.
    fn drop_if_unused(inner: &mut Inner, blob: &TreeDigest) -> Option<u64> {
        let unused = inner
            .live
            .get(blob)
            .is_some_and(|live| live.opens == 0 && live.foreign == Foreign::None);
        if !unused {
            return None;
        }
        // Dropping the guard closes the id.
        let _ = inner.live.remove(blob);
        inner.changes += 1;
        Some(inner.changes)
    }

    /// Asks for the file to hold the change [`BackingTable::drop_if_unused`] made, if it made one,
    /// and does not wait for it: the id is closed already.
    fn dropped(&self, change: Option<u64>) {
        if let Some(change) = change {
            self.ask(change, false);
        }
    }

    /// Writes the file with every count as it is now and `exact` on each line whose count is: for
    /// a process that has stopped reading requests and is about to hand the connection over.
    pub(super) fn write_exact(&self) {
        self.write(true);
    }

    /// Writes the file with no line marked `exact`.
    pub(super) fn write_inexact(&self) {
        self.write(false);
    }

    /// Keeps every live id open in the kernel after this table is dropped: one reference to each
    /// guard is leaked, so the guard is never dropped and never closes its id. For a process that
    /// leaves the connection to its successor.
    pub(super) fn abandon(&self) {
        for live in locked(&self.inner).live.values() {
            std::mem::forget(Arc::clone(&live.guard));
        }
    }

    /// Asks for the file to hold the change numbered `change` or a later one, and with `wait`
    /// returns once it does. The writer thread writes it; a waiting thread holds no lock while it
    /// waits, and every thread that waits for a write is woken at its end. A table with no writer
    /// thread writes it here, unless a write that started after the change has done so.
    fn ask(&self, change: u64, wait: bool) {
        let mut progress = locked(&self.writes.progress);
        progress.asked = progress.asked.max(change);
        if !progress.writer {
            drop(progress);
            self.write_asked();
            return;
        }
        self.writes.asked.notify_one();
        if wait {
            let _written = self
                .writes
                .wrote
                .wait_while(progress, |progress| progress.written < change)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Writes the file if a change asked to be in it is not.
    fn write_asked(&self) {
        let _writing = locked(&self.writing);
        let behind = {
            let progress = locked(&self.writes.progress);
            progress.asked > progress.written
        };
        if behind {
            self.wrote(self.write_held(false));
        }
    }

    /// Writes the file as the table is now.
    fn write(&self, exact: bool) {
        let _writing = locked(&self.writing);
        self.wrote(self.write_held(exact));
    }

    /// Records that the file holds the change numbered `change`, and wakes the threads that wait
    /// for it.
    fn wrote(&self, change: u64) {
        {
            let mut progress = locked(&self.writes.progress);
            progress.written = progress.written.max(change);
            #[cfg(test)]
            {
                progress.count += 1;
            }
        }
        self.writes.wrote.notify_all();
    }

    /// Writes the file as the table is now, with `writing` held, and gives the number of the
    /// change it holds; a file that cannot be written is said and the table goes on. With `exact`
    /// the lines whose count is known say so. The lock of the table is held only to copy the lines.
    fn write_held(&self, exact: bool) -> u64 {
        let (lines, change) = {
            let inner = locked(&self.inner);
            (Self::lines(&inner), inner.changes)
        };
        let mut lines: Vec<String> = lines
            .into_iter()
            .map(|(blob, id, opens, known)| {
                let mark = if exact && known { " exact" } else { "" };
                format!("{blob} {id} {opens}{mark}\n")
            })
            .collect();
        // The hex of the blob leads each line, and no two lines have one blob.
        lines.sort_unstable();
        let text = lines.concat();
        let mut new = self.file.clone().into_os_string();
        new.push(".new");
        let written = std::fs::write(&new, text).and_then(|()| std::fs::rename(&new, &self.file));
        if let Err(why) = written {
            let file = self.file.display();
            tracing::warn!("tierstore fs: cannot write the backing table {file}: {why}");
        }
        change
    }

    /// The lines of the file, as the table holds them now.
    fn lines(inner: &Inner) -> Vec<Line> {
        inner
            .live
            .iter()
            .map(|(blob, live)| {
                let (foreign, known) = match live.foreign {
                    Foreign::None => (0, true),
                    Foreign::Exact(opens) => (opens, true),
                    Foreign::Unknown(opens) => (opens, false),
                };
                (*blob, live.id, live.opens + foreign, known)
            })
            .collect()
    }

    /// The blobs with a live id.
    #[cfg(test)]
    pub(super) fn live(&self) -> usize {
        locked(&self.inner).live.len()
    }

    /// The cached opens of `blob` that the limit caused.
    #[cfg(test)]
    pub(super) fn spilled(&self, blob: &TreeDigest) -> u32 {
        locked(&self.inner).spilled.get(blob).copied().unwrap_or(0)
    }

    /// Whether no thread holds the lock of the table now: an open would not wait for it.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn unlocked(&self) -> bool {
        self.inner.try_lock().is_ok()
    }

    /// The opens that wait now.
    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        locked(&self.inner).waiting
    }

    /// Returns once the file holds every change made so far.
    #[cfg(test)]
    pub(super) fn flushed(&self) {
        let change = locked(&self.inner).changes;
        self.ask(change, true);
    }

    /// The writes of the file made since the table was made, the first one included.
    #[cfg(test)]
    pub(super) fn writes_made(&self) -> u64 {
        locked(&self.writes.progress).count
    }

    /// What the table shares with its writer thread: held by the thread for as long as it runs.
    #[cfg(test)]
    pub(super) fn shared(&self) -> Weak<impl Sized + use<>> {
        Arc::downgrade(&self.writes)
    }

    /// The opens counted on `blob`.
    #[cfg(test)]
    pub(super) fn opens(&self, blob: &TreeDigest) -> u32 {
        locked(&self.inner)
            .live
            .get(blob)
            .map_or(0, |live| live.opens)
    }
}

impl Ticket {
    /// Records the id that was registered for the blob, counting this open on it.
    pub(super) fn registered(self, registered: Registered) -> Backing {
        let backing = Backing {
            id: registered.id,
            guard: Arc::clone(&registered.guard),
        };
        let change = {
            let mut inner = locked(&self.table.inner);
            let _ = inner.registering.remove(&self.blob);
            let _ = inner.live.insert(
                self.blob,
                Live {
                    id: registered.id,
                    guard: registered.guard,
                    opens: 1,
                    foreign: Foreign::None,
                },
            );
            inner.changes += 1;
            inner.changes
        };
        // The opens that wait for this blob find its id now, before the file is written.
        self.table.changed.notify_all();
        self.table.ask(change, true);
        backing
    }
}

impl Drop for Ticket {
    /// Lets the opens that wait for the blob go on.
    fn drop(&mut self) {
        let _ = locked(&self.table.inner).registering.remove(&self.blob);
        self.table.changed.notify_all();
    }
}

impl Drop for BackingTable {
    /// Ends the writer thread.
    fn drop(&mut self) {
        locked(&self.writes.progress).gone = true;
        self.writes.asked.notify_all();
    }
}

/// The writer thread of the table `table`: writes the file each time a change is asked to be in
/// it and is not, every change made by then at once, until the table is dropped.
fn writer(writes: &Writes, table: &Weak<BackingTable>) {
    loop {
        {
            let progress = locked(&writes.progress);
            let progress = writes
                .asked
                .wait_while(progress, |progress| {
                    progress.asked <= progress.written && !progress.gone
                })
                .unwrap_or_else(PoisonError::into_inner);
            if progress.gone {
                return;
            }
        }
        // A table dropped since has said so: the next wait ends the thread.
        let _ = table.upgrade().map(|table| table.write_asked());
    }
}

#[cfg(test)]
#[path = "backing_tests.rs"]
mod tests;
