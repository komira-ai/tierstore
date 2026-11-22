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
//! The count is a hint of when a tree may leave, not a hold on it: the kernel forgets inodes only
//! under memory pressure, so a tree whose inodes it keeps would stay in memory as long as the
//! process lives, and the process would grow with every tree any job ever named. So a tree that
//! no run and no lookup has attached, and that no request has named for `fs.tree_idle_secs`,
//! leaves memory at the sweep for idle trees whatever its count ([`Core::drop_idle`]). Every
//! inode number of a tree is derivable (`ino.rs`): a request that names one of its inodes after
//! that reads the tree again and is answered as before, while the tree can be read (a tree that
//! is gone by then, its root manifest no longer upstream or its placement ended, is `ENOENT`, as
//! after a restart), and a forget of a tree that is not in memory changes nothing. The trees in memory are therefore those attached, by a run or by a
//! lookup, and those detached that a request named within `fs.tree_idle_secs` and the time
//! between two sweeps (a quarter of it, at most a minute).
//!
//! A forget of more lookups than the tree has counted means that this process did not count
//! every lookup the kernel holds. That is the case after a restart or a hand-over, since the
//! kernel keeps its inodes and the counts start at zero again, and for a tree read again after
//! the sweep took it out. Such a tree is *uncounted*: no forget takes it out of memory, and it
//! leaves at the sweep as any other idle tree does.
//!
//! At each look for idle trees the trees in memory are said on stderr at info, when they have
//! changed since the line before: how many, how many of them attached, their entries and an
//! estimate of the bytes they hold ([`Core::say_memory`]).
//!
//! Every read of a tree after the first in this process is said on stderr at info, with its
//! entries, its time, and the requests that waited for it. A read that took
//! [`SLOW_READ_MS`] or more is said at warn, and so is a read that failed, the first too, once in
//! `SAY_EVERY_MS` for a tree with the count of the failed reads not said (`core/said.rs`), as is
//! a table of tree numbers that cannot be read. A tree that leaves memory says the counts of its
//! failures not said yet. A
//! request for a tree that is gone (its root manifest is not upstream, or its placement has
//! ended) is answered `ENOENT`; one for a tree that cannot be read now, `EIO`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::tree_manifest::TreeDigest;

use super::fetched::Fetched;
#[cfg(any(target_os = "linux", test))]
use super::ino;
use super::placed::Unread;
use super::said::Failing;
use super::{Core, Errno, Loaded, locked};

/// A read that took at least this long, in milliseconds of the core's clock, is said at warn.
const SLOW_READ_MS: u64 = 1_000;

/// What a read that stopped before it ended (a panic) gives the requests that waited for it.
const STOPPED: &str = "the read of the tree stopped before it ended";

/// What a read of a tree ended with: the tree, now in memory, or why it was not read.
type Read = Result<Arc<Loaded>, Unread>;

/// What the trees in memory hold, as a look says it ([`Core::say_memory`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Footprint {
    /// The trees in memory.
    trees: usize,
    /// Those of them a run or a lookup has attached.
    attached: usize,
    /// Their entries.
    entries: u64,
    /// The bytes they hold on the heap, estimated (`Tree::heap_bytes`).
    bytes: u64,
}

/// A tree taken out of memory, and the map it was taken out of, which [`Core::free`] frees after
/// every lock is released.
struct Gone {
    number: u64,
    map: Arc<HashMap<u64, Arc<Loaded>>>,
    loaded: Arc<Loaded>,
}

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
    /// [`Errno::NoEnt`] for a number the table does not know or a tree that is gone (its root
    /// manifest is not upstream, or its placement has ended), [`Errno::Io`] for a table that
    /// cannot be read, said on stderr, or a tree that cannot be read now, which the read says
    /// ([`said`]).
    pub(super) fn tree(&self, number: u64) -> Result<Arc<Loaded>, Errno> {
        if let Some(loaded) = self.in_memory(number) {
            return Ok(loaded);
        }
        let digest = match self.table.digest_of(number) {
            Ok(Some(digest)) => digest,
            Ok(None) => return Err(Errno::NoEnt),
            Err(why) => {
                let now = self.now();
                // The table is the process's, not the tree's: its lines are counted under no tree.
                if let Some(unsaid) = self.lines.failed((0, Failing::Tree), None, now) {
                    tracing::warn!(
                        "tierstore fs: cannot read the tree number {number} from the table: \
                         {why}{}",
                        unsaid_reads(unsaid)
                    );
                }
                return Err(Errno::Io);
            }
        };
        self.load(number, digest).map_err(|why| match why {
            Unread::Gone(_) => Errno::NoEnt,
            Unread::Failed(_) => Errno::Io,
        })
    }

    /// The tree `number` if it is in memory, as used now.
    pub(super) fn in_memory(&self, number: u64) -> Option<Arc<Loaded>> {
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
            .read_tree(&digest, &Fetched::default())
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
        let line = said((&digest, number), &read, (times, took, waited));
        match line {
            Some(line) if read.is_err() => {
                let now = self.now();
                let unsaid = self
                    .lines
                    .failed((number, Failing::Tree), Some(&digest), now);
                if let Some(unsaid) = unsaid {
                    tracing::warn!("{line}{}", unsaid_reads(unsaid));
                }
            }
            Some(line) if took >= SLOW_READ_MS => tracing::warn!("{line}"),
            Some(line) => tracing::info!("{line}"),
            None => {}
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
    /// it, unless a lookup has put it back since.
    fn take_out(&self, number: u64, loaded: Arc<Loaded>) {
        if let Some(gone) = self.unmap(number, loaded) {
            self.free(gone);
        }
    }

    /// Takes out of the map the tree `number`, which the caller has marked as removed, unless a
    /// lookup has put it back since: what is to be freed after every lock is released
    /// ([`Core::free`]).
    fn unmap(&self, number: u64, loaded: Arc<Loaded>) -> Option<Gone> {
        #[cfg(test)]
        if let Some(hook) = locked(&self.hooks.marked).as_ref() {
            hook(self);
        }
        let map = self.trees.take_out(number, &loaded)?;
        Some(Gone {
            number,
            map,
            loaded,
        })
    }

    /// Frees the tree `gone`, with no lock of the core held, and says the counts of its failures
    /// not said yet.
    fn free(&self, gone: Gone) {
        let Gone {
            number,
            map,
            loaded,
        } = gone;
        drop(map);
        drop(loaded);
        for line in self.lines.unsaid(self.now(), Some(number)) {
            tracing::warn!("{line}");
        }
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

    /// What the trees in memory hold now.
    pub(super) fn footprint(&self) -> Footprint {
        let trees = self.trees.all();
        let state = self.state();
        trees
            .iter()
            .fold(Footprint::default(), |sum, (number, loaded)| Footprint {
                trees: sum.trees + 1,
                attached: sum.attached + usize::from(state.attachments.is_attached(*number)),
                entries: sum.entries + loaded.tree.len() as u64,
                bytes: sum.bytes + loaded.tree.heap_bytes(),
            })
    }

    /// Says on stderr at info what the trees in memory hold, unless it is what the line before
    /// said.
    pub(super) fn say_memory(&self) {
        let now = self.footprint();
        let mut said = locked(&self.memory_said);
        if *said != Some(now) {
            *said = Some(now);
            tracing::info!("{}", memory_line(now));
        }
    }

    /// Takes out of memory every tree that no run and no lookup has attached and that no request
    /// has named for `idle` milliseconds of the core's clock, whatever lookups of it the kernel
    /// holds (module docs); how many. The trees are chosen and taken out of the map under the
    /// lifecycle lock, under which every attach attaches, so that no tree attached since it was
    /// found detached leaves; they are freed after it is released.
    pub(super) fn drop_idle(&self, idle: u64) -> usize {
        // Every tree in memory is held here until the lock is released, so that none is freed
        // under it, the trees that stay included.
        let all = self.trees.all();
        let gone: Vec<Gone> = {
            let _lifecycle = locked(&self.lifecycle);
            let now = self.now();
            all.iter()
                .filter(|(number, loaded)| {
                    now.saturating_sub(loaded.used.load(Ordering::Acquire)) >= idle
                        && !self.attached(*number)
                        && loaded.lookups.remove()
                })
                .filter_map(|(number, loaded)| self.unmap(*number, Arc::clone(loaded)))
                .collect()
        };
        drop(all);
        let dropped = gone.len();
        gone.into_iter().for_each(|gone| self.free(gone));
        dropped
    }
}

/// The line that says what the trees in memory hold: their bytes in MiB, rounded up.
fn memory_line(now: Footprint) -> String {
    format!(
        "tierstore fs: {} {} in memory, {} of them attached: {} entries, about {} MiB",
        now.trees,
        if now.trees == 1 { "tree" } else { "trees" },
        now.attached,
        now.entries,
        now.bytes.div_ceil(1 << 20),
    )
}

/// What the line of a failed read adds for the `unsaid` failed reads of the tree before it that
/// were not said.
fn unsaid_reads(unsaid: u64) -> String {
    match unsaid {
        0 => String::new(),
        1 => "; 1 more failed read of this tree since the last line".to_owned(),
        n => format!("; {n} more failed reads of this tree since the last line"),
    }
}

/// The line said on stderr for the read `read` of the tree `digest`, numbered `number`, the
/// `times`-th of it in this process, which took `took` milliseconds while `waited` requests
/// waited for it: none for the first read of a tree that was read in less than
/// [`SLOW_READ_MS`]. A read that failed is always said, at warn.
fn said(
    (digest, number): (&TreeDigest, u64),
    read: &Read,
    (times, took, waited): (u64, u64, u64),
) -> Option<String> {
    if times == 1 && took < SLOW_READ_MS && read.is_ok() {
        return None;
    }
    let what = match read {
        Ok(loaded) => format!("{} entries", loaded.tree.len()),
        Err(why @ Unread::Gone(_)) => format!("gone: {why}"),
        Err(why @ Unread::Failed(_)) => format!("failed: {why}"),
    };
    Some(format!(
        "tierstore fs: read the tree {digest} (number {number}, read {times} times in this \
         process): {what}, in {took} ms, while {waited} requests waited for it"
    ))
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "memory_idle_tests.rs"]
mod idle_tests;
