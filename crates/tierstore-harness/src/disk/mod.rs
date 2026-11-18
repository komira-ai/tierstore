//! A fake disk with a volatile and a durable layer and crash points, and (in [`drive`]) a fake
//! drive on the injected clock (section 14: "a fake drive and a fake storage node with scripted
//! service times, hangs, errors and crashes").
//!
//! [`FakeDisk`] is a synchronous, cloneable handle on one namespace of files named by path
//! strings such as `"seg/0001.seg"`; the directory of a path is the part before its last `/`
//! (`""` for the root), and directories exist implicitly. Each file is an inode with
//! *volatile* contents (what reads see) and *durable* contents (what survives a crash); each
//! directory has a volatile and a durable map from name to inode.
//!
//! - `write_at`, `set_len` and `punch_hole` change volatile contents; [`FakeDisk::fdatasync`]
//!   copies one inode's volatile contents to its durable ones.
//! - `create`, `rename` and `unlink` change a directory's volatile entries;
//!   [`FakeDisk::fsync_dir`] copies that directory's entries to its durable map. A file whose
//!   contents were synced but whose directory entry never was is gone after a restart.
//! - [`FakeDisk::crash`] makes every later op fail with [`DiskError::Crashed`];
//!   [`FakeDisk::restart`] rebuilds the volatile layer from the durable one and clears pending
//!   faults.
//! - [`FakeDisk::inject`] and [`FakeDisk::inject_any`] script a [`Fault`] at the nth future op.
//!   Every op counts, reads included. [`crash_points`] uses them to run a protocol with a crash
//!   before (and optionally after) each of its ops in turn.
//!
//! The drive's model is in [`drive`].

pub mod drive;

pub use drive::{DriveOutcome, DriveStep, FakeDrive};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The kind of a disk op, for the op log and for fault injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpKind {
    /// `create`.
    Create,
    /// `write_at`.
    WriteAt,
    /// `read_at` and `read_all`.
    ReadAt,
    /// `len`.
    Len,
    /// `set_len`.
    SetLen,
    /// `punch_hole`.
    PunchHole,
    /// `fdatasync`.
    Fdatasync,
    /// `rename`.
    Rename,
    /// `unlink`.
    Unlink,
    /// `fsync_dir`.
    FsyncDir,
    /// `exists`.
    Exists,
    /// `list`.
    List,
}

/// What an injected fault does to the op it hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The op has no effect and the disk crashes; the op fails with [`DiskError::Crashed`].
    CrashBefore,
    /// The op takes effect, then the disk crashes; the op still fails with
    /// [`DiskError::Crashed`].
    CrashAfter,
    /// The op has no effect and fails with [`DiskError::Eio`]; the disk stays up.
    Eio,
}

/// Why a disk op failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiskError {
    /// The disk has crashed and not been restarted.
    #[error("the disk has crashed")]
    Crashed,
    /// An injected I/O error.
    #[error("EIO in {op:?} on {path:?}")]
    Eio {
        /// The op that failed.
        op: OpKind,
        /// The path the op was given.
        path: String,
    },
    /// The path does not name a file.
    #[error("no such file: {0:?}")]
    NotFound(String),
    /// The path already names a file.
    #[error("file exists: {0:?}")]
    Exists(String),
}

impl From<DiskError> for std::io::Error {
    /// `Eio` is raw OS error 5, `NotFound` and `Exists` their io kinds, `Crashed` an other-kind
    /// error carrying its message.
    fn from(e: DiskError) -> Self {
        match e {
            DiskError::Eio { .. } => Self::from_raw_os_error(5),
            DiskError::NotFound(_) => Self::new(std::io::ErrorKind::NotFound, e),
            DiskError::Exists(_) => Self::new(std::io::ErrorKind::AlreadyExists, e),
            DiskError::Crashed => Self::other(e),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Inode {
    volatile: Vec<u8>,
    durable: Vec<u8>,
}

type Dir = BTreeMap<String, u64>;

#[derive(Debug, Clone)]
struct PendingFault {
    /// `None` matches an op of any kind.
    kind: Option<OpKind>,
    /// Matching ops still to pass before the fault fires.
    remaining: usize,
    fault: Fault,
}

#[derive(Debug, Clone, Default)]
struct Inner {
    next_ino: u64,
    inodes: BTreeMap<u64, Inode>,
    volatile_dirs: BTreeMap<String, Dir>,
    durable_dirs: BTreeMap<String, Dir>,
    crashed: bool,
    log: Vec<(OpKind, String)>,
    faults: Vec<PendingFault>,
    fdatasyncs: BTreeMap<String, u64>,
    fdatasync_total: u64,
    fsync_dirs: u64,
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

impl Inner {
    fn ino(&self, path: &str) -> Result<u64, DiskError> {
        let (dir, name) = split(path);
        self.volatile_dirs
            .get(dir)
            .and_then(|d| d.get(name))
            .copied()
            .ok_or_else(|| DiskError::NotFound(path.to_owned()))
    }

    fn inode_mut(&mut self, path: &str) -> Result<&mut Inode, DiskError> {
        let ino = self.ino(path)?;
        Ok(self.inodes.entry(ino).or_default())
    }

    fn take_fault(&mut self, kind: OpKind) -> Option<Fault> {
        let mut fired = None;
        self.faults.retain_mut(|p| {
            if p.kind.is_some_and(|k| k != kind) {
                return true;
            }
            if p.remaining > 0 {
                p.remaining -= 1;
                return true;
            }
            if fired.is_none() {
                fired = Some(p.fault);
                return false;
            }
            true
        });
        fired
    }

    fn create(&mut self, path: &str) -> Result<(), DiskError> {
        if self.ino(path).is_ok() {
            return Err(DiskError::Exists(path.to_owned()));
        }
        let (dir, name) = split(path);
        self.next_ino += 1;
        let ino = self.next_ino;
        self.inodes.insert(ino, Inode::default());
        self.volatile_dirs
            .entry(dir.to_owned())
            .or_default()
            .insert(name.to_owned(), ino);
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str, noreplace: bool) -> Result<(), DiskError> {
        let ino = self.ino(from)?;
        if noreplace && self.ino(to).is_ok() {
            return Err(DiskError::Exists(to.to_owned()));
        }
        self.unlink(from)?;
        let (dir, name) = split(to);
        self.volatile_dirs
            .entry(dir.to_owned())
            .or_default()
            .insert(name.to_owned(), ino);
        Ok(())
    }

    fn unlink(&mut self, path: &str) -> Result<(), DiskError> {
        self.ino(path)?;
        let (dir, name) = split(path);
        self.volatile_dirs
            .entry(dir.to_owned())
            .or_default()
            .remove(name);
        Ok(())
    }

    fn restart(&mut self) {
        self.crashed = false;
        self.faults.clear();
        self.volatile_dirs = self.durable_dirs.clone();
        let live: Vec<u64> = self
            .durable_dirs
            .values()
            .flat_map(|d| d.values().copied())
            .collect();
        self.inodes.retain(|ino, _| live.contains(ino));
        for inode in self.inodes.values_mut() {
            inode.volatile = inode.durable.clone();
        }
    }
}

/// A fake disk: a shared handle (clones are the same disk; [`FakeDisk::fork`] is a copy).
#[derive(Debug, Clone, Default)]
pub struct FakeDisk {
    inner: Arc<Mutex<Inner>>,
}

impl FakeDisk {
    /// An empty disk.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs one op: refuses it when crashed, logs it, applies a due fault, else runs `f`.
    fn run<T>(
        &self,
        kind: OpKind,
        path: &str,
        f: impl FnOnce(&mut Inner) -> Result<T, DiskError>,
    ) -> Result<T, DiskError> {
        let mut s = self.lock();
        if s.crashed {
            return Err(DiskError::Crashed);
        }
        s.log.push((kind, path.to_owned()));
        match s.take_fault(kind) {
            None => f(&mut s),
            Some(Fault::Eio) => Err(DiskError::Eio {
                op: kind,
                path: path.to_owned(),
            }),
            Some(Fault::CrashBefore) => {
                s.crashed = true;
                Err(DiskError::Crashed)
            }
            Some(Fault::CrashAfter) => {
                let _ = f(&mut s);
                s.crashed = true;
                Err(DiskError::Crashed)
            }
        }
    }

    /// A copy of this disk (both layers, the log and the counts; no shared state).
    #[must_use]
    pub fn fork(&self) -> Self {
        Self {
            inner: Arc::new(Mutex::new(self.lock().clone())),
        }
    }

    /// Creates an empty file; [`DiskError::Exists`] when the path names one.
    ///
    /// # Errors
    /// `Exists`, or a crash or injected fault.
    pub fn create(&self, path: &str) -> Result<(), DiskError> {
        self.run(OpKind::Create, path, |s| s.create(path))
    }

    /// Writes `bytes` at `offset`, extending the file with zeros when `offset` is past its end.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn write_at(&self, path: &str, offset: usize, bytes: &[u8]) -> Result<(), DiskError> {
        self.run(OpKind::WriteAt, path, |s| {
            let data = &mut s.inode_mut(path)?.volatile;
            let end = offset + bytes.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[offset..end].copy_from_slice(bytes);
            Ok(())
        })
    }

    /// Reads up to `len` bytes at `offset`; a read past the end is short (or empty).
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn read_at(&self, path: &str, offset: usize, len: usize) -> Result<Vec<u8>, DiskError> {
        self.run(OpKind::ReadAt, path, |s| {
            let data = &s.inode_mut(path)?.volatile;
            let start = offset.min(data.len());
            let end = offset.saturating_add(len).min(data.len());
            Ok(data[start..end].to_vec())
        })
    }

    /// The whole volatile contents of a file.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn read_all(&self, path: &str) -> Result<Vec<u8>, DiskError> {
        self.run(OpKind::ReadAt, path, |s| {
            Ok(s.inode_mut(path)?.volatile.clone())
        })
    }

    /// The size of a file.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn len(&self, path: &str) -> Result<usize, DiskError> {
        self.run(OpKind::Len, path, |s| Ok(s.inode_mut(path)?.volatile.len()))
    }

    /// Sets the size of a file: shorter cuts, longer pads with zeros.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn set_len(&self, path: &str, len: usize) -> Result<(), DiskError> {
        self.run(OpKind::SetLen, path, |s| {
            s.inode_mut(path)?.volatile.resize(len, 0);
            Ok(())
        })
    }

    /// Zeroes `len` bytes at `offset` (clamped to the file); the size is kept.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn punch_hole(&self, path: &str, offset: usize, len: usize) -> Result<(), DiskError> {
        self.run(OpKind::PunchHole, path, |s| {
            let data = &mut s.inode_mut(path)?.volatile;
            let start = offset.min(data.len());
            let end = offset.saturating_add(len).min(data.len());
            data[start..end].fill(0);
            Ok(())
        })
    }

    /// Makes the contents of a file durable.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn fdatasync(&self, path: &str) -> Result<(), DiskError> {
        self.run(OpKind::Fdatasync, path, |s| {
            let inode = s.inode_mut(path)?;
            inode.durable = inode.volatile.clone();
            *s.fdatasyncs.entry(path.to_owned()).or_default() += 1;
            s.fdatasync_total += 1;
            Ok(())
        })
    }

    /// Renames `from` to `to`, replacing `to` unless `noreplace` (renameat2 `RENAME_NOREPLACE`),
    /// which fails with `Exists` when `to` exists.
    ///
    /// # Errors
    /// `NotFound` for `from`, `Exists`, or a crash or injected fault.
    pub fn rename(&self, from: &str, to: &str, noreplace: bool) -> Result<(), DiskError> {
        self.run(OpKind::Rename, from, |s| s.rename(from, to, noreplace))
    }

    /// Removes a file's directory entry.
    ///
    /// # Errors
    /// `NotFound`, or a crash or injected fault.
    pub fn unlink(&self, path: &str) -> Result<(), DiskError> {
        self.run(OpKind::Unlink, path, |s| s.unlink(path))
    }

    /// Makes the entries of directory `dir` durable.
    ///
    /// # Errors
    /// A crash or injected fault.
    pub fn fsync_dir(&self, dir: &str) -> Result<(), DiskError> {
        self.run(OpKind::FsyncDir, dir, |s| {
            let entries = s.volatile_dirs.get(dir).cloned().unwrap_or_default();
            s.durable_dirs.insert(dir.to_owned(), entries);
            s.fsync_dirs += 1;
            Ok(())
        })
    }

    /// Whether the path names a file.
    ///
    /// # Errors
    /// A crash or injected fault.
    pub fn exists(&self, path: &str) -> Result<bool, DiskError> {
        self.run(OpKind::Exists, path, |s| Ok(s.ino(path).is_ok()))
    }

    /// Every file path that starts with `prefix`, sorted.
    ///
    /// # Errors
    /// A crash or injected fault.
    pub fn list(&self, prefix: &str) -> Result<Vec<String>, DiskError> {
        self.run(OpKind::List, prefix, |s| {
            let mut paths: Vec<String> = s
                .volatile_dirs
                .iter()
                .flat_map(|(dir, names)| names.keys().map(move |n| join(dir, n)))
                .filter(|p| p.starts_with(prefix))
                .collect();
            paths.sort();
            Ok(paths)
        })
    }

    /// Crashes the disk now: every op fails with `Crashed` until [`FakeDisk::restart`].
    pub fn crash(&self) {
        self.lock().crashed = true;
    }

    /// Whether the disk is crashed.
    #[must_use]
    pub fn is_crashed(&self) -> bool {
        self.lock().crashed
    }

    /// Brings the disk up from its durable layer: unsynced data, creates, renames and unlinks
    /// are undone, a durable name keeps its inode's durable contents, pending faults are
    /// dropped.
    pub fn restart(&self) {
        self.lock().restart();
    }

    /// Scripts `fault` for the `nth` future op of `kind` (0 is the next one).
    pub fn inject(&self, kind: OpKind, nth: usize, fault: Fault) {
        self.lock().faults.push(PendingFault {
            kind: Some(kind),
            remaining: nth,
            fault,
        });
    }

    /// Scripts `fault` for the `nth` future op of any kind (0 is the next one).
    pub fn inject_any(&self, nth: usize, fault: Fault) {
        self.lock().faults.push(PendingFault {
            kind: None,
            remaining: nth,
            fault,
        });
    }

    /// Every op attempted while the disk was up, in order: its kind and its path (the source
    /// for a rename, the directory for an `fsync_dir`, the prefix for a `list`).
    #[must_use]
    pub fn ops(&self) -> Vec<(OpKind, String)> {
        self.lock().log.clone()
    }

    /// How many `fdatasync`s of `path` took effect.
    #[must_use]
    pub fn fdatasyncs(&self, path: &str) -> u64 {
        self.lock().fdatasyncs.get(path).copied().unwrap_or(0)
    }

    /// How many `fdatasync`s of any path took effect.
    #[must_use]
    pub fn total_fdatasyncs(&self) -> u64 {
        self.lock().fdatasync_total
    }

    /// How many `fsync_dir`s took effect.
    #[must_use]
    pub fn fsync_dir_count(&self) -> u64 {
        self.lock().fsync_dirs
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// One crash point of [`crash_points`]: the op index (counted from the start of the run) and
/// whether the crash came before or after the op took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashPoint {
    /// The index of the op, from 0.
    pub index: usize,
    /// `true` when the op took effect before the crash.
    pub after: bool,
}

/// Runs `run` against a fresh [`FakeDisk::fork`] of `start`, crashing before op `k` for
/// `k = 0, 1, 2, ...` (and after op `k` too when `also_after`), until a run finishes with the
/// disk still up. After each crashed run the disk is restarted and handed to `check` with its
/// [`CrashPoint`]. Returns how many crash points it tried (the run that completes is not one).
///
/// `run` must make the same ops on every run (it is a protocol, not a loop on the clock); it
/// need not look at its errors, the crash flag of the disk decides.
pub fn crash_points(
    start: &FakeDisk,
    also_after: bool,
    mut run: impl FnMut(&FakeDisk),
    mut check: impl FnMut(CrashPoint, &FakeDisk),
) -> usize {
    let mut tried = 0;
    let mut try_point = |index: usize, after: bool| -> bool {
        let disk = start.fork();
        let fault = if after {
            Fault::CrashAfter
        } else {
            Fault::CrashBefore
        };
        disk.inject_any(index, fault);
        run(&disk);
        if !disk.is_crashed() {
            return false;
        }
        disk.restart();
        check(CrashPoint { index, after }, &disk);
        tried += 1;
        true
    };
    let mut index = 0;
    while try_point(index, false) {
        if also_after {
            try_point(index, true);
        }
        index += 1;
    }
    tried
}
