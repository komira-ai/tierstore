//! The trees in memory: when a tree comes into memory, how many requests read it, and when it
//! leaves.
//!
//! A tree that is not in memory is read from the table of tree numbers and its manifests, and
//! flattened, when a request names one of its inodes ([`Core::tree`]). The read is made once
//! however many requests ask for the tree at the same time. The first request reads it, and
//! every other request waits for that read and takes its result, a tree or the reason it could
//! not be read. A tree of a whole build is many thousands of entries. If every request read it
//! for itself, each FUSE thread would spend seconds of CPU flattening the same tree, and the
//! kernel's queue would wait behind them.
//!
//! The lookups of a tree's inodes are counted, and a forget takes away what it names, with no
//! lock (`core/loaded.rs`). A tree leaves memory at the forget that takes its count to zero while
//! no run or lookup has it attached ([`Core::forget`]): the forget marks the tree as removed, in
//! the word that holds the count, only if the count is zero at that moment, and then takes it out
//! of the map. A lookup that finds the mark does not count on that tree. It puts the tree back,
//! or counts on the tree that is in memory, under the lock of the changes of the map
//! ([`Core::count_lookup`]). So a lookup is never lost: a count raised for a tree that had just
//! left memory would otherwise be gone with it, and the tree would leave at a later forget while
//! the kernel still held its inodes.
//!
//! A forget of more lookups than the tree has counted means that this process did not count
//! every lookup the kernel holds. That is the case after a restart or a hand-over, since
//! the kernel keeps its inodes and the counts start at zero again. Such a tree is
//! *uncounted*: no forget takes it out of memory. It leaves when no run and no lookup has it
//! attached and no request has named it for `fs.tree_idle_secs`, at the sweep for idle trees
//! ([`Core::drop_idle_uncounted`]).
//!
//! Every read of a tree after the first in this process is said on stderr at info, with its
//! entries, its time, and the requests that waited for it. A read that took
//! [`SLOW_READ_MS`] or more is said at warn.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::tree_manifest::TreeDigest;

#[cfg(any(target_os = "linux", test))]
use super::ino;
use super::placed::Unread;
use super::{Core, Errno, Loaded, locked};

/// A read that took at least this long, in milliseconds of the core's clock, is said at warn.
const SLOW_READ_MS: u64 = 1_000;

/// What a read that stopped before it ended (a panic) gives the requests that waited for it.
const STOPPED: &str = "the read of the tree stopped before it ended";

/// What a read of a tree ended with: the tree, now in memory, or why it was not read.
type Read = Result<Arc<Loaded>, Unread>;

/// A read of a tree that is running, and its end, which the requests that wait for it take.
#[derive(Default)]
pub(in crate::server::treefs) struct Flight {
    ended: Mutex<Option<Read>>,
    changed: Condvar,
    /// The requests that waited for this read.
    waited: AtomicU64,
}

/// The reads of trees that are running, by tree number, and what the reads have been.
#[derive(Default)]
pub(in crate::server::treefs) struct Reads {
    running: Mutex<HashMap<u64, Arc<Flight>>>,
    /// The requests waiting now for a read another request is making.
    waiting: AtomicU64,
    /// The tree numbers read in this process at least once, and how many times.
    done: Mutex<HashMap<u64, u64>>,
    /// Makes the next read that leads panic once it has read, before it ends: for the test of
    /// a read that stops.
    #[cfg(test)]
    pub(super) stop_next: std::sync::atomic::AtomicBool,
}

/// The end of a read that runs: it takes the flight out of the table and wakes the requests
/// that wait, with the end the read gave, or [`STOPPED`] if it gave none.
struct Ending<'a> {
    core: &'a Core,
    number: u64,
    flight: Arc<Flight>,
}

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        let _ = locked(&self.core.reads.running).remove(&self.number);
        let mut ended = locked(&self.flight.ended);
        let _ = ended.get_or_insert_with(|| Err(Unread::Failed(STOPPED.to_owned())));
        self.flight.changed.notify_all();
    }
}

impl Core {
    /// The tree `number`, as used now: from memory, or read once for every request that asks
    /// for it while it is read.
    ///
    /// # Errors
    /// [`Errno::NoEnt`] for a number the table does not know, [`Errno::Io`] for a table that
    /// cannot be read or a tree that cannot be read.
    pub(super) fn tree(&self, number: u64) -> Result<Arc<Loaded>, Errno> {
        if let Some(loaded) = self.in_memory(number) {
            return Ok(loaded);
        }
        let digest = self
            .table
            .digest_of(number)
            .map_err(|_| Errno::Io)?
            .ok_or(Errno::NoEnt)?;
        self.load(number, digest).map_err(|_| Errno::Io)
    }

    /// The tree `number` if it is in memory, as used now.
    fn in_memory(&self, number: u64) -> Option<Arc<Loaded>> {
        let loaded = self.trees.get(number)?;
        loaded.used.store(self.now(), Ordering::Release);
        Some(loaded)
    }

    /// The tree `digest`, numbered `number`: in memory, or read and kept in memory, once for
    /// every request that asks for it while it is read.
    ///
    /// # Errors
    /// Why the tree could not be read: the end of the read this request made, or of the one
    /// it waited for.
    pub(super) fn load(&self, number: u64, digest: TreeDigest) -> Read {
        let (flight, leads) = {
            let mut running = locked(&self.reads.running);
            // Looked for under the table of reads, which a read leaves only after the tree is
            // in memory: a request that comes as a read ends finds the one or the other.
            if let Some(loaded) = self.in_memory(number) {
                return Ok(loaded);
            }
            match running.get(&number) {
                Some(flight) => (Arc::clone(flight), false),
                None => {
                    let flight = Arc::new(Flight::default());
                    let _ = running.insert(number, Arc::clone(&flight));
                    (flight, true)
                }
            }
        };
        if leads {
            self.lead(number, digest, flight)
        } else {
            self.follow(&flight)
        }
    }

    /// Reads the tree for every request that waits for it, keeps it in memory, and says the
    /// read on stderr if it is not the first of the tree in this process.
    fn lead(&self, number: u64, digest: TreeDigest, flight: Arc<Flight>) -> Read {
        let ending = Ending {
            core: self,
            number,
            flight,
        };
        let start = self.now();
        let read = self
            .read_tree(&digest)
            .map(|tree| self.keep(number, digest, tree));
        #[cfg(test)]
        if self.reads.stop_next.swap(false, Ordering::AcqRel) {
            panic!("a read that stops before it ends");
        }
        let took = self.now().saturating_sub(start);
        let times = self.read_done(number);
        // Out of the table first: a request that comes from now on finds the tree in memory, or,
        // after a read that failed, reads it again rather than take this failure.
        let _ = locked(&self.reads.running).remove(&number);
        *locked(&ending.flight.ended) = Some(read.clone());
        ending.flight.changed.notify_all();
        let waited = ending.flight.waited.load(Ordering::Acquire);
        if let Some(line) = said((&digest, number), &read, (times, took, waited)) {
            if took >= SLOW_READ_MS {
                tracing::warn!("{line}");
            } else {
                tracing::info!("{line}");
            }
        }
        read
    }

    /// Counts a read of the tree `number` in this process; how many there have been of it.
    pub(super) fn read_done(&self, number: u64) -> u64 {
        let mut done = locked(&self.reads.done);
        let times = done.entry(number).or_insert(0);
        *times += 1;
        *times
    }

    /// Waits for the read of `flight` and gives its end.
    fn follow(&self, flight: &Flight) -> Read {
        let _ = flight.waited.fetch_add(1, Ordering::AcqRel);
        let _ = self.reads.waiting.fetch_add(1, Ordering::AcqRel);
        let ended = flight
            .changed
            .wait_while(locked(&flight.ended), |ended| ended.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = self.reads.waiting.fetch_sub(1, Ordering::AcqRel);
        ended
            .clone()
            .unwrap_or_else(|| Err(Unread::Failed(STOPPED.to_owned())))
    }

    /// The requests waiting now for a read of a tree that another request is making.
    #[cfg(test)]
    pub(super) fn waiting_for_reads(&self) -> u64 {
        self.reads.waiting.load(Ordering::Acquire)
    }

    /// The times the tree `number` was read in this process.
    #[cfg(test)]
    pub(super) fn times_read(&self, number: u64) -> u64 {
        locked(&self.reads.done).get(&number).copied().unwrap_or(0)
    }

    /// Keeps the tree `digest`, numbered `number`, in memory, unless it is there already: the one
    /// in memory, as used now. A tree that is not kept is freed with no lock held.
    pub(super) fn keep(&self, number: u64, digest: TreeDigest, tree: super::Tree) -> Arc<Loaded> {
        let now = self.now();
        let loaded = self.trees.keep(number, Loaded::new(digest, tree, now));
        loaded.used.store(now, Ordering::Release);
        loaded
    }

    /// Takes the tree `number` out of memory if the kernel holds no inode of it, as far as this
    /// process counted, and no run or lookup has it attached.
    pub(super) fn drop_if_forgotten(&self, number: u64) {
        let Some(loaded) = self.trees.get(number) else {
            return;
        };
        if loaded.lookups.is_forgotten() && !self.attached(number) {
            let _ = loaded
                .lookups
                .remove_if_forgotten()
                .then(|| self.take_out(number, loaded));
        }
    }

    /// Takes out of the map the tree `number`, which the caller has marked as removed, and frees
    /// it after every lock is released, unless a lookup has put it back since; whether it did.
    fn take_out(&self, number: u64, loaded: Arc<Loaded>) -> bool {
        #[cfg(test)]
        if let Some(hook) = locked(&self.hooks.marked).as_ref() {
            hook(self);
        }
        let gone = self.trees.take_out(number, &loaded);
        let taken = gone.is_some();
        drop(gone);
        drop(loaded);
        taken
    }

    /// The tree in memory, read if it is not, counting one more lookup of it.
    pub(super) fn counted(&self, number: u64) -> Result<Arc<Loaded>, Errno> {
        let loaded = self.tree(number)?;
        self.count_lookup(number, &loaded);
        Ok(loaded)
    }

    /// Counts one more lookup of the tree `number`, of which the caller holds `loaded`, if this
    /// core counts them, with no lock. If the tree is marked as removed, or left memory since the
    /// caller took it, `loaded` goes back, since the kernel holds an inode of it again; if
    /// another read of the tree is in memory now, the lookup is counted on that one.
    pub(super) fn count_lookup(&self, number: u64, loaded: &Arc<Loaded>) {
        if !self.settings.count_lookups || loaded.lookups.raise() {
            return;
        }
        self.trees.put_back(number, loaded);
    }

    /// Counts a lookup of `ino`, which a listing with entries (`READDIRPLUS`) made.
    #[cfg(any(target_os = "linux", test))]
    pub(in crate::server::treefs) fn count(&self, ino: u64) {
        #[cfg(test)]
        self.note("count");
        let _ = self.counted(ino::split(ino).0);
    }

    /// The kernel forgets `nlookup` lookups of `ino`. A forget of more than the tree counted, in
    /// a core that counts lookups, makes the tree uncounted (module docs).
    #[cfg(any(target_os = "linux", test))]
    pub(in crate::server::treefs) fn forget(&self, ino: u64, nlookup: u64) {
        #[cfg(test)]
        self.note("forget");
        let (number, _) = ino::split(ino);
        let Some(loaded) = self.trees.get(number) else {
            return;
        };
        if loaded.lookups.forget(nlookup, self.settings.count_lookups) {
            self.drop_if_forgotten(number);
        }
    }

    /// Takes out of memory every uncounted tree that no run and no lookup has attached and that
    /// no request has named for `idle` milliseconds of the core's clock; how many.
    pub(super) fn drop_idle_uncounted(&self, idle: u64) -> usize {
        let now = self.now();
        let mut dropped = 0;
        for (number, loaded) in self.trees.all() {
            let idle_now = now.saturating_sub(loaded.used.load(Ordering::Acquire)) >= idle;
            if idle_now && !self.attached(number) && loaded.lookups.remove_if_uncounted() {
                dropped += usize::from(self.take_out(number, loaded));
            }
        }
        dropped
    }
}

/// The line said on stderr for the read `read` of the tree `digest`, numbered `number`, the
/// `times`-th of it in this process, which took `took` milliseconds while `waited` requests
/// waited for it: none for the first read of a tree that took less than [`SLOW_READ_MS`].
fn said(
    (digest, number): (&TreeDigest, u64),
    read: &Read,
    (times, took, waited): (u64, u64, u64),
) -> Option<String> {
    if times == 1 && took < SLOW_READ_MS {
        return None;
    }
    let what = match read {
        Ok(loaded) => format!("{} entries", loaded.tree.len()),
        Err(why) => format!("failed: {why}"),
    };
    Some(format!(
        "tierstore fs: read the tree {digest} (number {number}, read {times} times in this \
         process): {what}, in {took} ms, while {waited} requests waited for it"
    ))
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
