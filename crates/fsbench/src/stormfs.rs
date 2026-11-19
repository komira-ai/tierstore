//! The file system of the storm (`fsbench storm`, [`crate::storm`]): a READ of a *cold* file is
//! parked, as a tree mount parks a READ of a cell that is not in the node cache yet, and answered
//! a fixed time later, as the fetch of that cell would complete (5.2 item 4, "Parked reads hold
//! ring entries").
//!
//! The tree is flat: the root (inode 1), the file `probe` (inode 2, [`PROBE_SIZE`] bytes) that
//! the metadata probes `stat`, and the cold files `c0`, `c1`, ... (inode [`COLD_INO`] + k, each
//! [`COLD_SIZE`] bytes) that the storm threads read, one each, at offsets never read before, so
//! that every read misses the page cache and reaches the server. Every byte reads as zero.
//! Entries and attributes are kept by the kernel for no time at all, so every `stat` of `probe`
//! is a request to the server (a LOOKUP, or a GETATTR): the metadata whose latency the storm
//! measures. `OPEN` is answered (and `OPENDIR`, `READDIR` and the rest `ENOSYS`: nothing lists
//! the tree), with `FOPEN_DIRECT_IO` for a cold file when the storm's reads
//! are direct ([`ColdRead::Direct`]: each `pread` is one foreground READ with a waiting caller)
//! and with no flag when they are buffered ([`ColdRead::Buffered`], the tree mount's: the READ is
//! the kernel's readahead, a background request).
//!
//! **Parking.** A READ of a cold file is put in the [`Parked`] queue with the time it is due,
//! `park` after it arrived, and the parker thread ([`StormFs::park_loop`]) answers each one when
//! it is due, from a thread that is not the reader that read it (on io_uring the request holds
//! its ring entry until then). A `park` of zero answers inline. Every other request is answered
//! inline. The counts of parked and answered READs are kept ([`StormCounts`]).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use tsfuse::abi::{Attr, AttrOut, EntryOut, OpenOut, Operation, ReadIn};
use tsfuse::{Errno, Fs, Handled, Replier, Request};

/// `FOPEN_DIRECT_IO`: the kernel sends every read of the file to the server, bypassing the page
/// cache.
pub const FOPEN_DIRECT_IO: u32 = 1;
/// `ENOENT`.
const ENOENT: Errno = Errno(2);
/// `ENOTDIR`.
const ENOTDIR: Errno = Errno(20);
/// `EISDIR`.
const EISDIR: Errno = Errno(21);

/// The inode of the root.
pub const ROOT_INO: u64 = 1;
/// The inode of `probe`.
pub const PROBE_INO: u64 = 2;
/// The inode of `c0`; `ck` is this plus `k`.
pub const COLD_INO: u64 = 1000;
/// The longest the parker waits before it looks at the clock again.
const MAX_WAIT: Duration = Duration::from_millis(10);
/// The bytes of `probe`.
pub const PROBE_SIZE: u64 = 4096;
/// The bytes of each cold file: 1 TiB, so a storm thread never runs out of offsets it has not
/// read.
pub const COLD_SIZE: u64 = 1 << 40;

/// How the storm threads read their cold files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColdRead {
    /// Through the page cache, as a tree mount is read: each read of a page not cached is a
    /// readahead READ, a background request.
    Buffered,
    /// `FOPEN_DIRECT_IO`: each `pread` is one foreground READ with a waiting caller.
    Direct,
}

impl ColdRead {
    /// The label a record keeps.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Buffered => "buffered",
            Self::Direct => "direct",
        }
    }
}

/// The READs parked and not answered yet, by the time each is due (nanoseconds of the storm's
/// clock) and then by arrival.
#[derive(Debug)]
pub struct Parked<T> {
    by_due: BTreeMap<(u64, u64), T>,
    next: u64,
}

impl<T> Default for Parked<T> {
    fn default() -> Self {
        Self {
            by_due: BTreeMap::new(),
            next: 0,
        }
    }
}

impl<T> Parked<T> {
    /// Parks `item` until `due`.
    pub fn push(&mut self, due: u64, item: T) {
        self.by_due.insert((due, self.next), item);
        self.next += 1;
    }

    /// The time the first parked item is due, `None` when none is parked.
    #[must_use]
    pub fn next_due(&self) -> Option<u64> {
        self.by_due.keys().next().map(|&(due, _)| due)
    }

    /// Takes every item due at `now` or before, the earliest first.
    pub fn take_due(&mut self, now: u64) -> Vec<T> {
        let Some(after) = now.checked_add(1) else {
            return self.take_all();
        };
        let later = self.by_due.split_off(&(after, 0));
        std::mem::replace(&mut self.by_due, later)
            .into_values()
            .collect()
    }

    /// Takes every item, due or not, the earliest first.
    pub fn take_all(&mut self) -> Vec<T> {
        std::mem::take(&mut self.by_due).into_values().collect()
    }

    /// The items parked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_due.len()
    }

    /// Whether none is parked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_due.is_empty()
    }
}

/// The counts of the cold READs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StormCounts {
    /// READs of a cold file received.
    pub cold_reads: u64,
    /// Of those, answered.
    pub answered: u64,
    /// Parked now (received and not answered).
    pub parked: u64,
    /// Metadata requests (LOOKUP and GETATTR) answered.
    pub metadata: u64,
}

/// A parked READ: its replier and the bytes to answer.
#[derive(Debug)]
struct Waiting {
    replier: Replier,
    len: usize,
}

/// The clock the file system parks by: nanoseconds since some start.
pub trait Clock: Send + Sync {
    /// Now.
    fn now(&self) -> u64;
}

/// The storm's file system.
pub struct StormFs<C> {
    cold_files: u64,
    read: ColdRead,
    park_ns: u64,
    clock: C,
    parked: Mutex<Parked<Waiting>>,
    changed: Condvar,
    stopping: AtomicBool,
    cold_reads: AtomicU64,
    answered: AtomicU64,
    metadata: AtomicU64,
    zeros: Vec<u8>,
}

impl<C> std::fmt::Debug for StormFs<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StormFs")
            .field("cold_files", &self.cold_files)
            .field("read", &self.read)
            .field("park_ns", &self.park_ns)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> StormFs<C> {
    /// A file system of `cold_files` cold files read as `read`, whose cold READs are answered
    /// `park` after they arrive by `clock`; replies are at most `max_read` bytes.
    #[must_use]
    pub fn new(cold_files: u64, read: ColdRead, park: Duration, clock: C, max_read: usize) -> Self {
        Self {
            cold_files,
            read,
            park_ns: u64::try_from(park.as_nanos()).unwrap_or(u64::MAX),
            clock,
            parked: Mutex::new(Parked::default()),
            changed: Condvar::new(),
            stopping: AtomicBool::new(false),
            cold_reads: AtomicU64::new(0),
            answered: AtomicU64::new(0),
            metadata: AtomicU64::new(0),
            zeros: vec![0; max_read],
        }
    }

    /// The counts so far.
    pub fn counts(&self) -> StormCounts {
        let parked = self.lock().len() as u64;
        StormCounts {
            cold_reads: self.cold_reads.load(Ordering::Relaxed),
            answered: self.answered.load(Ordering::Relaxed),
            parked,
            metadata: self.metadata.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Parked<Waiting>> {
        // A thread that panicked holding the lock left the queue whole: every change is one call.
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Answers the READs due by the clock now; returns how many it answered.
    pub fn answer_due(&self) -> usize {
        let due = self.lock().take_due(self.clock.now());
        self.answer(due)
    }

    fn answer(&self, waiting: Vec<Waiting>) -> usize {
        let n = waiting.len();
        for w in waiting {
            // The caller may be gone (an INTERRUPT answered it, or the mount ended): the
            // reply has nobody to reach, and the READ is over either way.
            let _ = w.replier.ok(&[&self.zeros[..w.len]]);
            self.answered.fetch_add(1, Ordering::Relaxed);
        }
        n
    }

    /// The parker: answers each parked READ when it is due, waiting on the queue between, until
    /// [`StormFs::stop`]; then answers whatever is still parked, at once.
    pub fn park_loop(&self) {
        loop {
            self.answer_due();
            let mut guard = self.lock();
            if self.stopping.load(Ordering::Acquire) {
                let rest = guard.take_all();
                drop(guard);
                self.answer(rest);
                return;
            }
            let wait = match guard.next_due() {
                None => MAX_WAIT,
                Some(due) => {
                    Duration::from_nanos(due.saturating_sub(self.clock.now())).min(MAX_WAIT)
                }
            };
            // Woken by a READ parked earlier than every other, by the stop, or by the time: at
            // the latest after `MAX_WAIT`, so a clock that jumps is followed.
            drop(
                self.changed
                    .wait_timeout(guard, wait)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }

    /// Ends [`StormFs::park_loop`] once it has answered every READ parked.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let _guard = self.lock();
        self.changed.notify_all();
    }

    fn attr(&self, ino: u64) -> Option<Attr> {
        let (mode, size, nlink) = match ino {
            ROOT_INO => (0o40_555, 0, 2),
            PROBE_INO => (0o100_444, PROBE_SIZE, 1),
            n if self.is_cold(n) => (0o100_444, COLD_SIZE, 1),
            _ => return None,
        };
        Some(Attr {
            ino,
            size,
            blocks: size.div_ceil(512),
            mode,
            nlink,
            blksize: 4096,
            ..Attr::default()
        })
    }

    fn is_cold(&self, ino: u64) -> bool {
        ino.checked_sub(COLD_INO)
            .is_some_and(|k| k < self.cold_files)
    }

    /// The inode and attributes of `name` in directory `parent`.
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, Attr), Errno> {
        if parent != ROOT_INO {
            return Err(match self.attr(parent) {
                Some(_) => ENOTDIR,
                None => ENOENT,
            });
        }
        let ino = match name {
            b"probe" => Some(PROBE_INO),
            _ => cold_index(name).map(|k| COLD_INO.saturating_add(k)),
        };
        ino.and_then(|ino| Some((ino, self.attr(ino)?)))
            .ok_or(ENOENT)
    }

    fn read(&self, ino: u64, arg: ReadIn, replier: Replier) -> Handled {
        let Some(attr) = self.attr(ino) else {
            let _ = replier.error(ENOENT);
            return Handled::Done;
        };
        if ino == ROOT_INO {
            let _ = replier.error(EISDIR);
            return Handled::Done;
        }
        let len = (attr.size.saturating_sub(arg.offset))
            .min(u64::from(arg.size))
            .min(self.zeros.len() as u64) as usize;
        if ino == PROBE_INO {
            let _ = replier.ok(&[&self.zeros[..len]]);
            return Handled::Done;
        }
        self.cold_reads.fetch_add(1, Ordering::Relaxed);
        let mut parked = self.lock();
        // After the stop no parker is left to answer it: the check is under the lock the
        // parker's last take holds, so a READ is either taken there or answered here.
        if self.park_ns == 0 || self.stopping.load(Ordering::Acquire) {
            drop(parked);
            self.answer(vec![Waiting { replier, len }]);
            return Handled::Done;
        }
        let due = self.clock.now().saturating_add(self.park_ns);
        let first = parked.next_due().is_none_or(|d| due < d);
        parked.push(due, Waiting { replier, len });
        if first {
            self.changed.notify_all();
        }
        Handled::Parked
    }
}

impl<C: Clock> Fs for StormFs<C> {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let ino = req.header.nodeid;
        let _ = match Operation::parse(&req) {
            Ok(Operation::Lookup { name }) => {
                self.metadata.fetch_add(1, Ordering::Relaxed);
                match self.lookup(ino, name) {
                    Ok((child, attr)) => {
                        let out = EntryOut::new(child, 0, Duration::ZERO, Duration::ZERO, attr);
                        replier.ok(&[out.as_bytes()])
                    }
                    Err(e) => replier.error(e),
                }
            }
            Ok(Operation::Getattr(_)) => {
                self.metadata.fetch_add(1, Ordering::Relaxed);
                match self.attr(ino) {
                    Some(attr) => replier.ok(&[AttrOut::new(Duration::ZERO, attr).as_bytes()]),
                    None => replier.error(ENOENT),
                }
            }
            Ok(Operation::Open(_)) => {
                let flags = match (self.is_cold(ino), self.read) {
                    (true, ColdRead::Direct) => FOPEN_DIRECT_IO,
                    _ => 0,
                };
                let out = OpenOut {
                    fh: 0,
                    open_flags: flags,
                    backing_id: 0,
                };
                replier.ok(&[out.as_bytes()])
            }
            Ok(Operation::Read(arg)) => return self.read(ino, arg, replier),
            Ok(Operation::Release(_)) => replier.ok(&[]),
            Ok(_) => replier.error(Errno::ENOSYS),
            Err(e) => replier.error(e.errno()),
        };
        Handled::Done
    }
}

/// `k` of a cold file's name `ck` (`k` in decimal, with no sign and no leading zero).
fn cold_index(name: &[u8]) -> Option<u64> {
    let k: u64 = std::str::from_utf8(name.strip_prefix(b"c")?)
        .ok()?
        .parse()
        .ok()?;
    (format!("c{k}").as_bytes() == name).then_some(k)
}

#[cfg(test)]
#[path = "stormfs_tests.rs"]
mod tests;
