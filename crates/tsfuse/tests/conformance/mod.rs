//! The library harness of the kernel conformance suite (`docs/design/fetch-cache-serve.md` 5.2,
//! "Kernel conformance tests", row T-F7; the index of the suite is
//! `docs/design/tsfuse-conformance.md`). Where `tests/kernel/mod.rs` answers the kernel by hand,
//! this harness serves through the library itself: a mount made by [`Mounter`], INIT run by
//! [`Session::init`] with the library's [`InitConfig`], cloned readers, and [`Session::serve`]
//! with [`Model`], an [`Fs`] that answers an expected tree ([`TreeSpec`]) the way the tree mount
//! does: LOOKUP, GETATTR, READDIR and READDIRPLUS (paged to the request's size), READLINK, STATFS,
//! OPEN and OPENDIR as a check asks, and READ by a closure the check gives (inline through a
//! [`tsfuse::ReplyPath`] with [`answer_read`], or parked).
//!
//! Every wait is on an event ([`Events::wait_for`], a caller's system call returning, a child
//! ending), never on a timer. [`Mounted`]'s drop aborts the connection where it may (as root) and
//! unmounts lazily, and [`Served`]'s drop drops every parked replier (each answers `EIO`) and
//! stops the readers, so a failed check never leaves a caller of this process waiting in the
//! kernel.
#![allow(dead_code, reason = "each test binary uses a part of the helpers")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::num::NonZeroUsize;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tierstore_testkit::dir::TestDir;
use tsfuse::abi::{
    Attr, AttrOut, DirPushError, DirentBuf, DirentPlusBuf, EntryKind, EntryOut, InitConfig,
    OpenOut, Operation, StatfsOut,
};
use tsfuse::mount::{Mount, MountError, MountOptions, Mounter};
use tsfuse::{
    Completion, DevFd, Errno, Fs, Handled, ReaderEnd, Replier, ReplyPath, Request, Serving,
    Session, Tokens,
};

use crate::kernel::{skip, unavailable};

/// The root's inode.
pub const ROOT: u64 = 1;
/// How long names and attributes stay valid: long, so a check sees no LOOKUP or GETATTR it did
/// not cause (the tree is immutable).
pub const VALID: Duration = Duration::from_secs(3600);
/// The modification time of every node, seconds.
pub const MTIME: u64 = 1_000_000;
/// `FOPEN_DIRECT_IO`: every `pread` of such a file is READs with a waiting caller.
pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
/// The cell of the design (section 2): 1 MiB.
pub const CELL: u64 = 1 << 20;

/// errno values the checks compare with.
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EACCES: i32 = 13;
    pub const EBUSY: i32 = 16;
    pub const ENAMETOOLONG: i32 = 36;
    pub const ECONNABORTED: i32 = 103;
    pub const ENOTCONN: i32 = 107;
}

/// What a node of the expected tree is.
#[derive(Debug)]
pub enum Kind {
    /// A directory and its children, in the order a listing returns them.
    Dir(Vec<(Vec<u8>, u64)>),
    /// A regular file of `size` bytes whose content is the data file `data` (the size the
    /// kernel sees is `size`, whatever the data file holds).
    File {
        size: u64,
        data: Option<Arc<OwnedFd>>,
    },
    /// A symbolic link to `target`.
    Symlink(Vec<u8>),
}

/// One node: its parent and what it is.
#[derive(Debug)]
pub struct Node {
    pub parent: u64,
    pub kind: Kind,
}

/// The tree a [`Model`] serves and a check compares the kernel's answers with: inode numbers
/// from [`ROOT`] up, in the order the nodes were added.
#[derive(Debug)]
pub struct TreeSpec {
    nodes: BTreeMap<u64, Node>,
}

impl Default for TreeSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeSpec {
    /// A tree of the root alone.
    #[must_use]
    pub fn new() -> Self {
        let root = Node {
            parent: ROOT,
            kind: Kind::Dir(Vec::new()),
        };
        Self {
            nodes: BTreeMap::from([(ROOT, root)]),
        }
    }

    fn add(&mut self, parent: u64, name: &str, kind: Kind) -> u64 {
        let ino = self.nodes.keys().next_back().unwrap() + 1;
        match &mut self.nodes.get_mut(&parent).unwrap().kind {
            Kind::Dir(children) => children.push((name.as_bytes().to_vec(), ino)),
            other => panic!("{parent} is not a directory: {other:?}"),
        }
        self.nodes.insert(ino, Node { parent, kind });
        ino
    }

    /// Adds the directory `name` to `parent`; its inode.
    pub fn dir(&mut self, parent: u64, name: &str) -> u64 {
        self.add(parent, name, Kind::Dir(Vec::new()))
    }

    /// Adds the file `name` of `size` bytes, its content read from `data`; its inode.
    pub fn file(&mut self, parent: u64, name: &str, size: u64, data: Option<OwnedFd>) -> u64 {
        let data = data.map(Arc::new);
        self.add(parent, name, Kind::File { size, data })
    }

    /// Adds the symbolic link `name` to `target`; its inode.
    pub fn symlink(&mut self, parent: u64, name: &str, target: &str) -> u64 {
        self.add(parent, name, Kind::Symlink(target.as_bytes().to_vec()))
    }

    /// The node `ino`.
    #[must_use]
    pub fn node(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    /// The child `name` of `parent`.
    #[must_use]
    pub fn lookup(&self, parent: u64, name: &[u8]) -> Option<u64> {
        match &self.nodes.get(&parent)?.kind {
            Kind::Dir(children) => children.iter().find(|(n, _)| n == name).map(|(_, i)| *i),
            _ => None,
        }
    }

    /// The entry kind of `ino`.
    #[must_use]
    pub fn kind(&self, ino: u64) -> Option<EntryKind> {
        Some(match self.nodes.get(&ino)?.kind {
            Kind::Dir(_) => EntryKind::Directory,
            Kind::File { .. } => EntryKind::File,
            Kind::Symlink(_) => EntryKind::Symlink,
        })
    }

    /// The listing of directory `ino` as READDIR returns it: `.`, `..`, then the children; each
    /// entry's name, inode and kind.
    #[must_use]
    pub fn entries(&self, ino: u64) -> Option<Vec<(Vec<u8>, u64, EntryKind)>> {
        let node = self.nodes.get(&ino)?;
        let Kind::Dir(children) = &node.kind else {
            return None;
        };
        let mut all = vec![
            (b".".to_vec(), ino, EntryKind::Directory),
            (b"..".to_vec(), node.parent, EntryKind::Directory),
        ];
        for (name, child) in children {
            all.push((name.clone(), *child, self.kind(*child).unwrap()));
        }
        Some(all)
    }

    /// The attributes of `ino`: a directory `r-xr-xr-x` with a link per child directory, a file
    /// `r--r--r--` of its size, a link of its target's length; owned by this process's user.
    #[must_use]
    pub fn attr(&self, ino: u64) -> Option<Attr> {
        let (mode, size, nlink) = match &self.nodes.get(&ino)?.kind {
            Kind::Dir(children) => {
                let dirs = children
                    .iter()
                    .filter(|(_, c)| self.kind(*c) == Some(EntryKind::Directory))
                    .count();
                (0o40_555, 0, 2 + dirs as u32)
            }
            Kind::File { size, .. } => (0o100_444, *size, 1),
            Kind::Symlink(target) => (0o120_777, target.len() as u64, 1),
        };
        Some(Attr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: MTIME,
            mtime: MTIME,
            ctime: MTIME,
            mode,
            nlink,
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            blksize: 4096,
            ..Attr::default()
        })
    }

    /// The size and the data file of file `ino`.
    #[must_use]
    pub fn data(&self, ino: u64) -> Option<(u64, &OwnedFd)> {
        match &self.nodes.get(&ino)?.kind {
            Kind::File {
                size,
                data: Some(data),
            } => Some((*size, data)),
            _ => None,
        }
    }

    /// The bytes a read of `len` bytes of file `ino` at `offset` must return: those of its data
    /// file, cut at the file's size.
    #[must_use]
    pub fn bytes(&self, ino: u64, offset: u64, len: u64) -> Vec<u8> {
        let (size, data) = self.data(ino).unwrap();
        let end = (offset + len).min(size);
        let mut buf = vec![0u8; end.saturating_sub(offset) as usize];
        let mut done = 0;
        while done < buf.len() {
            let n = rustix::io::pread(data, &mut buf[done..], offset + done as u64).unwrap();
            assert!(n > 0, "the data file of {ino} holds its whole size");
            done += n;
        }
        buf
    }
}

/// A memfd holding `bytes`.
#[must_use]
pub fn memfd(name: &str, bytes: &[u8]) -> OwnedFd {
    let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut done = 0;
    while done < bytes.len() {
        done += rustix::io::write(&fd, &bytes[done..]).unwrap();
    }
    fd
}

/// `len` bytes of a pattern that differs per `seed` and per offset (no two pages alike).
#[must_use]
pub fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i % 251) as u8) ^ seed.wrapping_mul(37) ^ ((i >> 12) as u8))
        .collect()
}

/// The SHA-256 of `bytes`, hex.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How [`Model`] answers OPEN of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Open {
    /// Success with these `FOPEN_*` flags.
    Flags(u32),
    /// `ENOSYS`: under `NO_OPEN_SUPPORT` the kernel sends no OPEN again (the tree mount, 5.3).
    Enosys,
}

/// How [`Model`] answers OPENDIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opendir {
    /// Success.
    Ok,
    /// `ENOSYS`: under `NO_OPENDIR_SUPPORT` the kernel sends no OPENDIR again.
    Enosys,
}

/// A READ as the file system saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Read {
    pub unique: u64,
    pub ino: u64,
    pub offset: u64,
    pub size: u32,
}

/// A READDIR or READDIRPLUS as the file system saw it, and how many entries it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirRead {
    /// `true` for READDIRPLUS.
    pub plus: bool,
    pub ino: u64,
    pub offset: u64,
    pub size: u32,
    pub sent: usize,
}

/// A parked READ: its replier and the reader thread that read it.
pub struct Parked {
    pub read: Read,
    pub replier: Replier,
    pub reader: String,
}

/// How a completion went, kept by value (an `io::Error` is not `Clone`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Done {
    Written,
    AlreadyAnswered,
    DeviceError(Option<i32>),
}

impl From<&Completion> for Done {
    fn from(c: &Completion) -> Self {
        match c {
            Completion::Written => Self::Written,
            Completion::AlreadyAnswered => Self::AlreadyAnswered,
            Completion::DeviceError(e) => Self::DeviceError(e.raw_os_error()),
        }
    }
}

/// What the file system saw and did, watched by the check.
#[derive(Default)]
pub struct State {
    /// Requests per opcode that reached [`Fs::handle`].
    pub ops: BTreeMap<u32, usize>,
    /// Every READ, in the order read.
    pub reads: Vec<Read>,
    /// Every READDIR and READDIRPLUS, in the order read.
    pub dirs: Vec<DirRead>,
    /// READs parked by the check's closure.
    pub parked: Vec<Parked>,
    /// How each READ answered inline or completed went, by its unique, and whether it was
    /// spliced.
    pub completions: Vec<(u64, Done, bool)>,
    /// Counters a check keeps for itself.
    pub counts: BTreeMap<&'static str, usize>,
}

impl State {
    /// Requests of `opcode` handled.
    #[must_use]
    pub fn count(&self, opcode: u32) -> usize {
        self.ops.get(&opcode).copied().unwrap_or(0)
    }
}

/// [`State`] and the condition variable a waiter watches it with.
#[derive(Default)]
pub struct Events {
    state: Mutex<State>,
    changed: Condvar,
}

impl Events {
    /// Changes the state and wakes every waiter.
    pub fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = f(&mut self.state.lock().unwrap());
        self.changed.notify_all();
        r
    }

    /// Reads the state.
    pub fn with<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock().unwrap())
    }

    /// Waits until `f` returns `Some`, on a change of the state.
    pub fn wait_for<R>(&self, mut f: impl FnMut(&mut State) -> Option<R>) -> R {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(r) = f(&mut state) {
                return r;
            }
            state = self.changed.wait(state).unwrap();
        }
    }
}

/// The opcodes the checks count (`enum fuse_opcode` of the pinned `abi/fuse.h`).
pub mod op {
    pub const LOOKUP: u32 = 1;
    pub const GETATTR: u32 = 3;
    pub const READLINK: u32 = 5;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FLUSH: u32 = 25;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const READDIRPLUS: u32 = 44;
}

/// What a check does with a READ: the tree, the read, its replier and the events.
pub type OnRead = dyn Fn(&TreeSpec, Read, Replier, &Events) -> Handled + Send + Sync;

/// The file system of the suite: [`TreeSpec`] answered inline, except READ, which `on_read`
/// handles.
pub struct Model {
    pub tree: Arc<TreeSpec>,
    pub open: Open,
    pub opendir: Opendir,
    pub events: Arc<Events>,
    pub on_read: Box<OnRead>,
}

impl Model {
    fn readdir(&self, node: u64, offset: u64, size: u32, plus: bool, replier: Replier) {
        let Some(entries) = self.tree.entries(node) else {
            let _ = replier.error(Errno(20));
            return;
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let mut list = DirentBuf::new(size as usize);
        let mut list_plus = DirentPlusBuf::new(size as usize);
        let mut sent = 0;
        for (i, (name, ino, kind)) in entries.iter().enumerate().skip(start) {
            // The offset that resumes after this entry is the index of the next one.
            let off = i as u64 + 1;
            let pushed = if plus {
                let entry = if name == b"." || name == b".." {
                    EntryOut::new(0, 0, Duration::ZERO, Duration::ZERO, Attr::default())
                } else {
                    EntryOut::new(*ino, 1, VALID, VALID, self.tree.attr(*ino).unwrap())
                };
                list_plus.push(*ino, off, *kind, name, entry)
            } else {
                list.push(*ino, off, *kind, name)
            };
            match pushed {
                Ok(()) => sent += 1,
                Err(DirPushError::Full) => break,
                Err(e) => panic!("the entry {name:?} of the tree is refused: {e}"),
            }
        }
        self.events.update(|s| {
            s.dirs.push(DirRead {
                plus,
                ino: node,
                offset,
                size,
                sent,
            });
        });
        let bytes = if plus {
            list_plus.as_bytes()
        } else {
            list.as_bytes()
        };
        let _ = replier.ok(&[bytes]);
    }
}

impl Fs for Model {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let node = req.header.nodeid;
        self.events
            .update(|s| *s.ops.entry(req.header.opcode).or_default() += 1);
        let tree = &self.tree;
        let _ = match Operation::parse(&req) {
            Ok(Operation::Lookup { name }) => match tree.lookup(node, name) {
                Some(ino) => {
                    let entry = EntryOut::new(ino, 1, VALID, VALID, tree.attr(ino).unwrap());
                    replier.ok(&[entry.as_bytes()])
                }
                None => replier.error(Errno(errno::ENOENT)),
            },
            Ok(Operation::Getattr(_)) => match tree.attr(node) {
                Some(attr) => replier.ok(&[AttrOut::new(VALID, attr).as_bytes()]),
                None => replier.error(Errno(errno::ENOENT)),
            },
            Ok(Operation::Readlink) => match tree.node(node).map(|n| &n.kind) {
                Some(Kind::Symlink(target)) => replier.ok(&[target]),
                _ => replier.error(Errno(22)),
            },
            Ok(Operation::Open(_)) => match self.open {
                Open::Flags(open_flags) => {
                    let out = OpenOut {
                        fh: node,
                        open_flags,
                        backing_id: 0,
                    };
                    replier.ok(&[out.as_bytes()])
                }
                Open::Enosys => replier.error(Errno::ENOSYS),
            },
            Ok(Operation::Opendir(_)) => match self.opendir {
                Opendir::Ok => replier.ok(&[OpenOut::default().as_bytes()]),
                Opendir::Enosys => replier.error(Errno::ENOSYS),
            },
            Ok(Operation::Readdir(r)) => {
                self.readdir(node, r.offset, r.size, false, replier);
                return Handled::Done;
            }
            Ok(Operation::Readdirplus(r)) => {
                self.readdir(node, r.offset, r.size, true, replier);
                return Handled::Done;
            }
            Ok(Operation::Read(r)) => {
                let read = Read {
                    unique: req.header.unique,
                    ino: node,
                    offset: r.offset,
                    size: r.size,
                };
                self.events.update(|s| s.reads.push(read));
                return (self.on_read)(tree, read, replier, &self.events);
            }
            Ok(Operation::Statfs) => replier.ok(&[StatfsOut::default().as_bytes()]),
            Ok(Operation::Flush(_) | Operation::Release(_) | Operation::Releasedir(_)) => {
                replier.ok(&[])
            }
            _ => replier.error(Errno::ENOSYS),
        };
        Handled::Done
    }
}

/// What [`answer_read`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answered {
    pub done: Done,
    pub spliced: bool,
    pub len: usize,
}

/// Answers `read` from the data file of its node through `path`, the bytes cut at the file's
/// size (a READ at or past the end is answered with no data), and records the completion.
pub fn answer_read(
    path: &ReplyPath,
    tree: &TreeSpec,
    read: Read,
    replier: Replier,
    events: &Events,
) -> Answered {
    let (size, data) = tree.data(read.ino).unwrap();
    let len = size.saturating_sub(read.offset).min(u64::from(read.size)) as usize;
    let answered = match path.fill(replier, data.as_fd(), read.offset, len) {
        Ok(filled) => {
            let (len, spliced) = (filled.len(), filled.is_spliced());
            let done = Done::from(&filled.commit());
            Answered { done, spliced, len }
        }
        Err(failed) => {
            let done = Done::from(&failed.replier.error(Errno::EIO));
            Answered {
                done,
                spliced: false,
                len: 0,
            }
        }
    };
    events.update(|s| {
        s.completions
            .push((read.unique, answered.done, answered.spliced));
    });
    answered
}

/// The name of the current thread (a reader is `tsfuse-reader-<index>`).
#[must_use]
pub fn reader_name() -> String {
    thread::current().name().unwrap_or("").to_owned()
}

/// Parks `read` in the state's `parked` list.
pub fn park(read: Read, replier: Replier, events: &Events) -> Handled {
    let reader = reader_name();
    events.update(|s| {
        s.parked.push(Parked {
            read,
            replier,
            reader,
        });
    });
    Handled::Parked
}

/// A mount of the suite; dropping it aborts the connection (as root; the abort file is root's),
/// so a failed check ends every caller still waiting, and unmounts lazily.
pub struct Mounted {
    mount: Option<Mount>,
    /// The mount point.
    pub mp: PathBuf,
    /// The mount point as `/proc/self/mountinfo` names it (symbolic links resolved before the
    /// mount, since resolving it afterwards would go into the mount).
    pub real_mp: PathBuf,
    /// The scratch directory holding the mount point.
    pub dir: TestDir,
}

/// The options of every mount of the suite: the tree's, without `allow_other` (which an
/// unprivileged `fusermount3` refuses unless `/etc/fuse.conf` allows it).
#[must_use]
pub fn tree_options(fsname: &str) -> MountOptions {
    MountOptions {
        allow_other: false,
        ..MountOptions::tree(fsname)
    }
}

impl Mounted {
    /// Mounts in a scratch directory named after `name` with `options`, through `mount` (for
    /// example [`Mounter::mount`]), runs INIT with `config` and binds clones up to `readers`.
    /// `None` (said on stderr) where no FUSE mount can be made.
    pub fn with(
        name: &str,
        options: &MountOptions,
        config: &InitConfig,
        readers: usize,
        mount: impl FnOnce(&Mounter, &Path, &MountOptions) -> Result<(DevFd, Mount), MountError>,
    ) -> Option<(Self, Session)> {
        if let Some(why) = unavailable() {
            skip(why);
            return None;
        }
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let mp = dir.path().join("mnt");
        std::fs::create_dir(&mp).unwrap();
        let real_mp = std::fs::canonicalize(&mp).unwrap();
        let (device, made) = mount(&Mounter::default(), &mp, options).unwrap();
        let mounted = Self {
            mount: Some(made),
            mp,
            real_mp,
            dir,
        };
        let mut session = Session::init(device, config).unwrap();
        session
            .clone_readers(NonZeroUsize::new(readers).unwrap())
            .unwrap();
        assert_eq!(session.readers(), readers);
        Some((mounted, session))
    }

    /// [`Self::with`] through [`Mounter::mount`] (`mount(2)` as root, else `fusermount3`), the
    /// suite's options and the tree's INIT terms.
    #[must_use]
    pub fn new(name: &str, readers: usize) -> Option<(Self, Session)> {
        Self::with(
            name,
            &tree_options("tsfuse-conformance"),
            &InitConfig::tree(),
            readers,
            |m, mp, o| m.mount(mp, o),
        )
    }

    /// The path of `rel` in the mount.
    #[must_use]
    pub fn path(&self, rel: &str) -> PathBuf {
        self.mp.join(rel)
    }

    /// The mount.
    #[must_use]
    pub fn mount(&self) -> &Mount {
        self.mount.as_ref().unwrap()
    }

    /// Takes the mount out of the guard (its drop then does nothing).
    pub fn take(&mut self) -> Mount {
        self.mount.take().unwrap()
    }

    /// Unmounts (not lazily); the readers then end.
    pub fn unmount(mut self) {
        self.take().unmount(false).unwrap();
    }

    /// The mount's entry in `/proc/self/mountinfo`.
    #[must_use]
    pub fn entry(&self) -> Option<MountEntry> {
        mount_entry(&self.real_mp)
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(mount) = self.mount.take() {
            let _ = mount.abort();
            let _ = mount.unmount(true);
        }
    }
}

/// A served session. Dropping it before [`Self::join`] (a failed check) drops every parked
/// replier (each answers `EIO`) and stops the readers.
pub struct Served {
    serving: Option<Serving>,
    /// What the file system saw.
    pub events: Arc<Events>,
    /// The session's table of registered requests.
    pub tokens: Tokens,
}

impl Served {
    /// The serving session.
    #[must_use]
    pub fn serving(&self) -> &Serving {
        self.serving.as_ref().unwrap()
    }

    /// Waits for every reader to end: how each ended.
    pub fn join(mut self) -> Vec<ReaderEnd> {
        self.serving.take().unwrap().join().unwrap()
    }

    /// Hands the connection over ([`Serving::hand_over`]): how each reader ended.
    pub fn hand_over(mut self) -> Vec<ReaderEnd> {
        self.serving.take().unwrap().hand_over().unwrap()
    }

    /// Takes every parked READ.
    pub fn take_parked(&self) -> Vec<Parked> {
        self.events.update(|s| std::mem::take(&mut s.parked))
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            self.events.update(|s| s.parked.clear());
            serving.stopper().stop();
        }
    }
}

/// Serves `session` with a [`Model`] of `tree`.
pub fn serve(
    session: Session,
    tree: Arc<TreeSpec>,
    open: Open,
    opendir: Opendir,
    on_read: impl Fn(&TreeSpec, Read, Replier, &Events) -> Handled + Send + Sync + 'static,
) -> Served {
    let events = Arc::new(Events::default());
    let tokens = session.tokens().clone();
    let model = Model {
        tree,
        open,
        opendir,
        events: events.clone(),
        on_read: Box::new(on_read),
    };
    Served {
        serving: Some(session.serve(Arc::new(model)).unwrap()),
        events,
        tokens,
    }
}

/// A caller of the mount on a thread of its own: it opens a file, reads it once and closes it.
pub struct Caller {
    result: Receiver<io::Result<Vec<u8>>>,
    thread: JoinHandle<()>,
}

impl Caller {
    /// What its read returned.
    pub fn result(&self) -> io::Result<Vec<u8>> {
        self.result.recv().unwrap()
    }

    /// The bytes its read returned.
    pub fn bytes(&self) -> Vec<u8> {
        self.result().unwrap()
    }

    /// Waits until it has closed the file (its FLUSH answered), so an unmount finds it idle.
    pub fn join(self) {
        self.thread.join().unwrap();
    }
}

/// Opens `path` and reads `len` bytes at `offset` with one `pread` on a thread of its own; the
/// result is sent before the file is closed, since the close sends a FLUSH that needs a free
/// reader.
#[must_use]
pub fn pread(path: PathBuf, offset: u64, len: usize) -> Caller {
    let (tx, result) = channel();
    let thread = thread::spawn(move || {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                tx.send(Err(e)).unwrap();
                return;
            }
        };
        let mut buf = vec![0u8; len];
        let got = file.read_at(&mut buf, offset).map(|n| {
            buf.truncate(n);
            buf
        });
        tx.send(got).unwrap();
        drop(file);
    });
    Caller { result, thread }
}

/// The command that runs the test `name` of this binary alone, as a child process (the test is
/// `#[ignore]`d, so only a parent that asks for it runs it).
#[must_use]
pub fn child(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        name,
        "--ignored",
        "--test-threads=1",
        "--nocapture",
    ]);
    command
}

/// One mount of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// The file system type (`fuse.tsfuse`).
    pub fstype: String,
    /// The source (the `fsname`).
    pub source: String,
    /// The per-mount options (`ro`, `nosuid`, `nodev`, ...).
    pub options: Vec<String>,
    /// The super block options (`user_id=`, `group_id=`, `default_permissions`, ...).
    pub super_options: Vec<String>,
}

/// A field of `/proc/self/mountinfo` with its octal escapes (`\040` for a space) undone.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal {
            Some(d) => {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The entry of the mount at `mp` (as the kernel names it), the last one if several are
/// stacked, from `/proc/self/mountinfo`.
#[must_use]
pub fn mount_entry(mp: &Path) -> Option<MountEntry> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    table.lines().rev().find_map(|line| {
        let (head, tail) = line.split_once(" - ")?;
        let head: Vec<&str> = head.split(' ').collect();
        let tail: Vec<&str> = tail.split(' ').collect();
        (Path::new(&unescape(head.get(4)?)) == mp).then(|| MountEntry {
            fstype: tail.first().copied().unwrap_or("").to_owned(),
            source: unescape(tail.get(1).copied().unwrap_or("")),
            options: head
                .get(5)
                .unwrap_or(&"")
                .split(',')
                .map(String::from)
                .collect(),
            super_options: tail
                .get(2)
                .unwrap_or(&"")
                .split(',')
                .map(String::from)
                .collect(),
        })
    })
}

/// The running kernel's release, `(major, minor)`, from `/proc/sys/kernel/osrelease`.
#[must_use]
pub fn kernel_release() -> (u32, u32) {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
    let mut parts = release.trim().split(['.', '-']);
    let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (next(), next())
}

/// Whether the running kernel is at least `major.minor`.
#[must_use]
pub fn kernel_at_least(major: u32, minor: u32) -> bool {
    kernel_release() >= (major, minor)
}

/// Whether this process runs as root.
#[must_use]
pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}
