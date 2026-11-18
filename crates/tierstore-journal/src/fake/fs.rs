//! [`MemFs`]: an entry's files in memory, with the durable and the volatile state of each file
//! and of the directory kept apart, so a test can crash it at any operation.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::lock;
use crate::files::{EntryFiles, FileKind};

/// An operation of [`EntryFiles`], for counts, faults and the crash point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Op {
    /// [`EntryFiles::read`] (and [`MemFs::read_range`]).
    Read,
    /// [`EntryFiles::write_at`].
    WriteAt,
    /// [`EntryFiles::set_len`].
    SetLen,
    /// [`EntryFiles::sync_data`].
    SyncData,
    /// [`EntryFiles::punch_data`].
    PunchData,
    /// [`EntryFiles::replace_snapshot`].
    ReplaceSnapshot,
    /// [`EntryFiles::sync_dir`].
    SyncDir,
}

impl Op {
    /// Whether the operation changes the files (a read does not, and is no crash point).
    #[must_use]
    pub const fn mutates(self) -> bool {
        !matches!(self, Self::Read)
    }
}

/// Which writes not yet synced a power loss keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// None: only what was synced survives.
    Nothing,
    /// Every one, whole (a device that wrote everything it was given, but acknowledged no
    /// flush).
    Everything,
    /// Each one by a deterministic choice from the seed and its order: dropped, kept whole, or
    /// (a write) kept torn, as a prefix of its bytes.
    Seeded(u64),
}

/// How the process that used the files ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crash {
    /// The process died; the kernel did not: every write is kept, synced or not, and what was
    /// not synced is still not durable (a later power loss may lose it).
    Process,
    /// The machine lost power (or the kernel crashed): what was synced survives, and of the
    /// rest what [`Keep`] chooses. The restart is in another boot.
    Power(Keep),
}

#[derive(Debug, Clone)]
enum Change {
    Write { offset: u64, bytes: Vec<u8> },
    SetLen(u64),
    Punch { offset: u64, len: u64 },
}

impl Change {
    fn apply(&self, file: &mut Vec<u8>, keep: Option<usize>) {
        match self {
            Self::Write { offset, bytes } => {
                let take = keep.unwrap_or(bytes.len()).min(bytes.len());
                write_into(file, *offset, &bytes[..take]);
            }
            Self::SetLen(len) => file.resize(to_usize(*len), 0),
            Self::Punch { offset, len } => {
                let start = to_usize(*offset).min(file.len());
                let end = to_usize(offset.saturating_add(*len)).min(file.len());
                file[start..end].fill(0);
            }
        }
    }
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn write_into(file: &mut Vec<u8>, offset: u64, bytes: &[u8]) {
    let start = to_usize(offset);
    let end = start + bytes.len();
    if file.len() < end {
        file.resize(end, 0);
    }
    file[start..end].copy_from_slice(bytes);
}

/// `len` bytes of `b` from `offset`, zeros past its end.
fn range_of(b: &[u8], offset: u64, len: u64) -> Vec<u8> {
    let start = to_usize(offset).min(b.len());
    let end = to_usize(offset.saturating_add(len)).min(b.len());
    let mut out = b[start..end].to_vec();
    out.resize(to_usize(len), 0);
    out
}

#[derive(Debug, Clone, Default)]
struct Inode {
    durable: Vec<u8>,
    current: Vec<u8>,
    /// Changes since the last sync, each with its order number.
    pending: Vec<(u64, Change)>,
}

#[derive(Debug, Clone)]
enum DirChange {
    /// A name was created for the inode.
    Link(FileKind, usize),
    /// The snapshot temp's inode was renamed over the snapshot.
    Rename(usize),
}

#[derive(Debug, Clone, Copy)]
struct Fault {
    op: Op,
    file: Option<FileKind>,
    /// Matching calls to let through before the one that fails.
    skip: u64,
    kind: io::ErrorKind,
}

#[derive(Debug, Clone, Default)]
struct State {
    inodes: Vec<Inode>,
    names: BTreeMap<FileKind, usize>,
    durable_names: BTreeMap<FileKind, usize>,
    dir_pending: Vec<(u64, DirChange)>,
    /// The order number of the next change.
    next_change: u64,
    counts: BTreeMap<(Op, Option<FileKind>), u64>,
    faults: Vec<Fault>,
    /// The mutating calls that may still take effect before the process is dead.
    die_after: Option<u64>,
    mutations: u64,
    dead: bool,
}

impl State {
    /// Admits one call: fails it when the process is dead, when its crash point is reached, or
    /// when a fault matches; else counts it.
    fn admit(&mut self, op: Op, file: Option<FileKind>) -> io::Result<()> {
        if self.dead {
            return Err(io::Error::other("the process is dead"));
        }
        if op.mutates() && self.die_after == Some(0) {
            self.dead = true;
            return Err(io::Error::other("the process died here"));
        }
        let mut failed = None;
        for (i, f) in self.faults.iter_mut().enumerate() {
            if f.op == op && (f.file.is_none() || f.file == file) {
                if f.skip == 0 {
                    failed = Some((i, f.kind));
                    break;
                }
                f.skip -= 1;
            }
        }
        if let Some((i, kind)) = failed {
            self.faults.remove(i);
            return Err(io::Error::new(kind, format!("injected {op:?} failure")));
        }
        if op.mutates() {
            self.mutations += 1;
            self.die_after = self.die_after.map(|n| n - 1);
        }
        *self.counts.entry((op, file)).or_default() += 1;
        Ok(())
    }

    fn inode_mut(&mut self, file: FileKind) -> Option<&mut Inode> {
        let i = *self.names.get(&file)?;
        self.inodes.get_mut(i)
    }

    /// The inode of `file`, created with a pending name if it does not exist.
    fn inode_or_create(&mut self, file: FileKind) -> &mut Inode {
        let i = match self.names.get(&file) {
            Some(i) => *i,
            None => {
                let i = self.inodes.len();
                self.inodes.push(Inode::default());
                self.names.insert(file, i);
                let n = self.take_change();
                self.dir_pending.push((n, DirChange::Link(file, i)));
                i
            }
        };
        &mut self.inodes[i]
    }

    fn take_change(&mut self) -> u64 {
        let n = self.next_change;
        self.next_change += 1;
        n
    }

    fn change(&mut self, file: FileKind, change: Change) {
        let n = self.take_change();
        let inode = self.inode_or_create(file);
        change.apply(&mut inode.current, None);
        inode.pending.push((n, change));
    }

    /// The state after a power loss that keeps what `keep` chooses.
    fn power_loss(&self, keep: Keep) -> Self {
        let choice = |n: u64| -> Option<Option<usize>> {
            match keep {
                Keep::Nothing => None,
                Keep::Everything => Some(None),
                Keep::Seeded(seed) => match splitmix(seed ^ n.wrapping_mul(0x9e37_79b9)) % 3 {
                    0 => None,
                    1 => Some(None),
                    // A torn write: a prefix of a length chosen from the seed too.
                    r => Some(Some(to_usize(splitmix(
                        seed.wrapping_add(n).wrapping_add(r),
                    )))),
                },
            }
        };
        let inodes = self
            .inodes
            .iter()
            .map(|inode| {
                let mut kept = inode.durable.clone();
                for (n, c) in &inode.pending {
                    if let Some(prefix) = choice(*n) {
                        let prefix = match (prefix, c) {
                            (Some(p), Change::Write { bytes, .. }) => Some(p % bytes.len().max(1)),
                            _ => None,
                        };
                        c.apply(&mut kept, prefix);
                    }
                }
                Inode {
                    durable: kept.clone(),
                    current: kept,
                    pending: Vec::new(),
                }
            })
            .collect();
        let mut names = self.durable_names.clone();
        for (n, c) in &self.dir_pending {
            if choice(*n).is_some() {
                match c {
                    DirChange::Link(file, i) => {
                        names.insert(*file, *i);
                    }
                    DirChange::Rename(i) => {
                        names.remove(&FileKind::SnapshotTemp);
                        names.insert(FileKind::Snapshot, *i);
                    }
                }
            }
        }
        Self {
            inodes,
            durable_names: names.clone(),
            names,
            next_change: self.next_change,
            ..Self::default()
        }
    }
}

/// splitmix64: a deterministic stream of choices from a seed, with no dependency.
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// An entry's files in memory. Each file has the bytes a read sees (the page cache) and the
/// bytes that would survive a power loss (synced); a sync makes them equal. A name created or a
/// rename done is volatile until [`EntryFiles::sync_dir`]. Clones share one file system.
///
/// A test injects a failure of the `n`th next call of an operation ([`MemFs::fail`]), or kills
/// the process at a point ([`MemFs::die_after`]: after that many more mutating calls, every
/// call fails), and then takes the file system a restart sees ([`MemFs::crash`]). It counts the
/// calls of each operation on each file ([`MemFs::count`]).
#[derive(Debug, Clone, Default)]
pub struct MemFs {
    state: Arc<Mutex<State>>,
}

impl MemFs {
    /// An empty file system.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The `nth` next call (0 is the next one) of `op` on `file` (or on any file, `None`) fails
    /// with `kind` and has no effect.
    pub fn fail(&self, op: Op, file: Option<FileKind>, nth: u64, kind: io::ErrorKind) {
        lock(&self.state).faults.push(Fault {
            op,
            file,
            skip: nth,
            kind,
        });
    }

    /// After `n` more mutating calls take effect, the process is dead: every later call fails
    /// and has no effect.
    pub fn die_after(&self, n: u64) {
        lock(&self.state).die_after = Some(n);
    }

    /// Whether the process is dead ([`MemFs::die_after`]).
    #[must_use]
    pub fn dead(&self) -> bool {
        lock(&self.state).dead
    }

    /// The mutating calls that took effect.
    #[must_use]
    pub fn mutations(&self) -> u64 {
        lock(&self.state).mutations
    }

    /// The calls of `op` on `file` that took effect (`None` for the directory's operations).
    #[must_use]
    pub fn count(&self, op: Op, file: Option<FileKind>) -> u64 {
        lock(&self.state)
            .counts
            .get(&(op, file))
            .copied()
            .unwrap_or(0)
    }

    /// The file system a restart sees after `how`: the counts, faults and crash point are
    /// cleared, and the process is alive again.
    #[must_use]
    pub fn crash(&self, how: Crash) -> Self {
        let s = lock(&self.state);
        let next = match how {
            Crash::Process => State {
                inodes: s.inodes.clone(),
                names: s.names.clone(),
                durable_names: s.durable_names.clone(),
                dir_pending: s.dir_pending.clone(),
                next_change: s.next_change,
                ..State::default()
            },
            Crash::Power(keep) => s.power_loss(keep),
        };
        Self {
            state: Arc::new(Mutex::new(next)),
        }
    }

    /// The bytes a read of `file` sees, or `None` when it does not exist.
    #[must_use]
    pub fn current(&self, file: FileKind) -> Option<Vec<u8>> {
        let s = lock(&self.state);
        let i = *s.names.get(&file)?;
        Some(s.inodes[i].current.clone())
    }

    /// The bytes of `file` that a power loss now would keep with [`Keep::Nothing`], or `None`
    /// when its name would not survive.
    #[must_use]
    pub fn durable(&self, file: FileKind) -> Option<Vec<u8>> {
        let s = lock(&self.state);
        let i = *s.durable_names.get(&file)?;
        Some(s.inodes[i].durable.clone())
    }

    /// Whether `len` bytes of `file` from `offset` are durable as a read sees them: the synced
    /// bytes equal the current ones (bytes past a file's end read as zeros) and the name is
    /// durable.
    #[must_use]
    pub fn is_durable(&self, file: FileKind, offset: u64, len: u64) -> bool {
        let s = lock(&self.state);
        let (Some(i), Some(d)) = (s.names.get(&file), s.durable_names.get(&file)) else {
            return false;
        };
        i == d
            && range_of(&s.inodes[*i].current, offset, len)
                == range_of(&s.inodes[*i].durable, offset, len)
    }

    /// `len` bytes of `file` from `offset` as a read sees them (zeros past the end); a counted
    /// [`Op::Read`] that faults and the crash point apply to. The entry's `pread`.
    pub fn read_range(&self, file: FileKind, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut s = lock(&self.state);
        s.admit(Op::Read, Some(file))?;
        let Some(&i) = s.names.get(&file) else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        Ok(range_of(&s.inodes[i].current, offset, len))
    }

    /// XORs `mask` into the byte at `offset` of `file`, durable and current alike: bit rot, or a
    /// device that acknowledged a flush it did not do. A missing file or an offset past the end
    /// changes nothing.
    pub fn corrupt(&self, file: FileKind, offset: u64, mask: u8) {
        let mut s = lock(&self.state);
        if let Some(inode) = s.inode_mut(file) {
            for b in [&mut inode.durable, &mut inode.current] {
                if let Some(byte) = b.get_mut(to_usize(offset)) {
                    *byte ^= mask;
                }
            }
        }
    }
}

impl EntryFiles for MemFs {
    fn read(&self, file: FileKind) -> io::Result<Option<Vec<u8>>> {
        let mut s = lock(&self.state);
        s.admit(Op::Read, Some(file))?;
        Ok(s.names.get(&file).map(|i| s.inodes[*i].current.clone()))
    }

    fn write_at(&self, file: FileKind, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let mut s = lock(&self.state);
        s.admit(Op::WriteAt, Some(file))?;
        s.change(
            file,
            Change::Write {
                offset,
                bytes: bytes.to_vec(),
            },
        );
        Ok(())
    }

    fn set_len(&self, file: FileKind, len: u64) -> io::Result<()> {
        let mut s = lock(&self.state);
        s.admit(Op::SetLen, Some(file))?;
        s.change(file, Change::SetLen(len));
        Ok(())
    }

    fn sync_data(&self, file: FileKind) -> io::Result<()> {
        let mut s = lock(&self.state);
        let Some(&i) = s.names.get(&file) else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        s.admit(Op::SyncData, Some(file))?;
        let inode = &mut s.inodes[i];
        inode.durable = inode.current.clone();
        inode.pending.clear();
        Ok(())
    }

    fn punch_data(&self, offset: u64, len: u64) -> io::Result<()> {
        let mut s = lock(&self.state);
        if !s.names.contains_key(&FileKind::Data) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        s.admit(Op::PunchData, Some(FileKind::Data))?;
        s.change(FileKind::Data, Change::Punch { offset, len });
        Ok(())
    }

    fn replace_snapshot(&self) -> io::Result<()> {
        let mut s = lock(&self.state);
        let Some(&i) = s.names.get(&FileKind::SnapshotTemp) else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        s.admit(Op::ReplaceSnapshot, None)?;
        s.names.remove(&FileKind::SnapshotTemp);
        s.names.insert(FileKind::Snapshot, i);
        let n = s.take_change();
        s.dir_pending.push((n, DirChange::Rename(i)));
        Ok(())
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut s = lock(&self.state);
        s.admit(Op::SyncDir, None)?;
        s.durable_names = s.names.clone();
        s.dir_pending.clear();
        Ok(())
    }
}
