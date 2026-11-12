//! The FUSE frontend: decodes the kernel's requests into the calls of the core (`core.rs`) and
//! encodes the answers. It decides nothing: what a lookup, an open or a listing answers is the
//! core's; every request that would change a tree is answered `EROFS`, and the extended
//! attributes `ENOSYS`, so that the kernel stops asking after the first. Where `fs.put` is
//! enabled, `put/` and every inode below it are the put directory's, whose requests go to
//! `fuse_put.rs`.
//!
//! The mount (`docs/design/virtual-build-fs.md` section 6.3): `default_permissions` (the kernel
//! checks the modes, which the core reports), `allow_other` (jobs run as other users), the
//! capabilities `FUSE_CACHE_SYMLINKS`, `FUSE_PARALLEL_DIROPS` and `FUSE_DO_READDIRPLUS`, and
//! `max_background` 1024.
//!
//! `init` also asks for `FUSE_HAS_RESEND` where the kernel offers it (Linux 6.9 and later), so that a
//! process that takes the connection over can have the requests the one before read and did not
//! answer sent again (`launch.rs`). A read or a release that names a handle an earlier process gave
//! out is the core's to make sense of (`core/adopt.rs`), and `destroy` tells the process that the
//! readers have stopped.
//!
//! Two modes, chosen in `init` from what the kernel offers (D20). With passthrough (the kernel
//! offers `FUSE_PASSTHROUGH`, the process holds `CAP_SYS_ADMIN` in the initial namespace, and
//! `fs.passthrough` is not false) the connection's `max_stack_depth` is 1, so that the job's
//! overlay can stack on the mount, and an open of a non-empty file registers the cache file with
//! the kernel (`FUSE_DEV_IOC_BACKING_OPEN`) and answers `FOPEN_PASSTHROUGH` with the blob's one
//! id: reads then never reach this process. Without it (the 6.8 mode) opens are cached opens
//! (`FOPEN_KEEP_CACHE`) and every read the page cache misses is answered here from the cache
//! file. An empty file is a cached open in both.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BackingId, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, SessionACL, TimeOrNow,
    WriteFlags,
};

use super::backing::Registered;
use super::core::{self, Attr, Core, Kind, Looked, Opened};
use super::fuse_put;
use super::ino;
use super::put::space::PutSpace;

/// The background requests the kernel may have outstanding at once.
const MAX_BACKGROUND: u16 = 1024;

/// The name the mount shows in `/proc/mounts`.
const NAME: &str = "tierstore-fs";

/// The fuser errno of the core's.
fn errno(error: core::Errno) -> Errno {
    match error {
        core::Errno::NoEnt => Errno::ENOENT,
        core::Errno::Acces => Errno::EACCES,
        core::Errno::NotDir => Errno::ENOTDIR,
        core::Errno::IsDir => Errno::EISDIR,
        core::Errno::Inval => Errno::EINVAL,
        core::Errno::Io => Errno::EIO,
        core::Errno::RoFs => Errno::EROFS,
    }
}

/// The attributes the kernel is sent.
fn file_attr(attr: &Attr) -> FileAttr {
    let when = UNIX_EPOCH + Duration::from_secs(core::FIXED_MTIME);
    let kind = match attr.kind {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
        Kind::Symlink => FileType::Symlink,
    };
    FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: when,
        mtime: when,
        ctime: when,
        crtime: when,
        kind,
        perm: attr.perm,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// What a lookup of a name that is not there answers: an inode number of 0.
const ABSENT: Attr = Attr {
    ino: 0,
    kind: Kind::File,
    size: 0,
    perm: 0,
    nlink: 0,
    uid: 0,
    gid: 0,
};

/// The options of the mount, for the users `acl` lets in.
pub(super) fn config(threads: usize, allow_other: bool) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(NAME.to_owned()),
        MountOption::Subtype(NAME.to_owned()),
        MountOption::DefaultPermissions,
    ];
    config.acl = if allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    config.n_threads = Some(threads);
    // One descriptor read by every thread: a clone would fail its requests if it were released
    // (D12).
    config.clone_fd = false;
    config
}

/// How the bytes of the files are served, as `INIT` decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// The kernel reads the cache files itself.
    Passthrough,
    /// Cached opens; reads reach this process. The words say why not passthrough.
    Cached(&'static str),
}

impl Mode {
    /// The mode for a kernel that offers passthrough or not, a setting that asks for it or not,
    /// and a process that may register backing files or not. A server that asks for passthrough
    /// without `CAP_SYS_ADMIN` would fail every open with `EIO`, so it does not ask.
    pub(super) fn chosen(offered: bool, wanted: bool, privileged: bool) -> Self {
        if !wanted {
            Self::Cached("fs.passthrough is false")
        } else if !offered {
            Self::Cached("the kernel does not offer passthrough")
        } else if !privileged {
            Self::Cached("the process lacks CAP_SYS_ADMIN")
        } else {
            Self::Passthrough
        }
    }
}

/// Whether this process holds `CAP_SYS_ADMIN` in the initial user namespace, which is what
/// `FUSE_DEV_IOC_BACKING_OPEN` asks: the capability in the effective set, and a user namespace
/// that is the initial one. A process that cannot read either holds none.
fn privileged() -> bool {
    let capable = rustix::thread::capabilities(None).is_ok_and(|sets| {
        sets.effective
            .contains(rustix::thread::CapabilitySet::SYS_ADMIN)
    });
    capable
        && std::fs::read_to_string("/proc/self/uid_map")
            .is_ok_and(|map| initial_user_namespace(&map))
}

/// Whether the text of `/proc/self/uid_map` is that of the initial user namespace, which maps the
/// whole range of ids to themselves in one line. A namespace of a container or of `unshare -U`
/// maps less, or elsewhere, and its `CAP_SYS_ADMIN` is not the one the kernel asks for.
fn initial_user_namespace(uid_map: &str) -> bool {
    let mut lines = uid_map.lines();
    let first = lines
        .next()
        .map(|line| line.split_whitespace().collect::<Vec<_>>());
    lines.next().is_none() && first.is_some_and(|fields| fields == ["0", "0", "4294967295"])
}

/// The number in the text `{:?}` makes of a [`BackingId`] (`... backing_id: 7 }`): `fuser` 0.18
/// has no other way to read it, and the backing table records it.
fn id_in(debug: &str) -> Option<u32> {
    let (_, after) = debug.split_once("backing_id: ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Registers `file` with the kernel as a backing file, in answer to the open `reply` is for.
fn register(reply: &ReplyOpen, file: &File) -> io::Result<Registered> {
    let id = reply.open_backing(file)?;
    let raw = id_in(&format!("{id:?}"))
        .ok_or_else(|| io::Error::other("fuser does not print the backing id as it did"))?;
    Ok(Registered {
        id: raw,
        guard: Arc::new(id),
    })
}

/// The filesystem the kernel sees.
pub(super) struct Frontend {
    core: Arc<Core>,
    /// The put directory, where it is served (the core's).
    put: Option<Arc<PutSpace>>,
    /// Whether to ask for passthrough if the kernel offers it (`fs.passthrough`).
    passthrough: bool,
    /// Told when the session ends and the kernel's requests are no longer read.
    ended: Option<Sender<()>>,
}

impl Frontend {
    /// A frontend over `core`.
    pub(super) fn new(core: Arc<Core>, passthrough: bool) -> Self {
        Self {
            put: core.put().cloned(),
            core,
            passthrough,
            ended: None,
        }
    }

    /// The answer to a request that neither a tree nor the put directory does, on `ino`: `EPERM`
    /// in the put directory, `EROFS` elsewhere.
    fn refusal(&self, ino: INodeNo) -> Errno {
        if self.put_of(ino).is_some() {
            Errno::EPERM
        } else {
            Errno::EROFS
        }
    }

    /// The put directory, when `ino` is one of its inodes and it is served.
    fn put_of(&self, ino: INodeNo) -> Option<&PutSpace> {
        let put = self.put.as_deref()?;
        ino::put_of(ino.0).map(|_| put)
    }

    /// The same, sending on `ended` when the session ends: when its readers have stopped and
    /// every request they held has been passed to the core.
    pub(super) fn telling(mut self, ended: Sender<()>) -> Self {
        self.ended = Some(ended);
        self
    }
}

/// A reply that can be answered with an error.
trait Fail {
    fn fail(self, error: Errno);
}

macro_rules! fail {
    ($($reply:ty),*) => {
        $(impl Fail for $reply {
            fn fail(self, error: Errno) {
                self.error(error);
            }
        })*
    };
}

fail!(
    ReplyAttr,
    ReplyData,
    ReplyDirectory,
    ReplyDirectoryPlus,
    ReplyEmpty,
    ReplyEntry,
    ReplyOpen
);

/// Answers `reply` by `done` with what the core gave, or with the errno of its refusal.
fn finish<T, R: Fail>(result: Result<T, core::Errno>, reply: R, done: impl FnOnce(T, R)) {
    match result {
        Ok(value) => done(value, reply),
        Err(error) => reply.fail(errno(error)),
    }
}

impl Filesystem for Frontend {
    fn destroy(&mut self) {
        // The receiver is gone when nobody waits for the end.
        let _ = self.ended.as_ref().map(|ended| ended.send(()));
    }

    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        for flag in [
            InitFlags::FUSE_CACHE_SYMLINKS,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_READDIRPLUS_AUTO,
        ] {
            // A kernel that does not offer one runs without it; each is a saving.
            let _ = config.add_capabilities(flag);
        }
        let _ = config.set_max_background(MAX_BACKGROUND);
        let offered = config.capabilities();
        let passthrough = offered.contains(InitFlags::FUSE_PASSTHROUGH);
        let resend = offered.contains(InitFlags::FUSE_HAS_RESEND);
        // So that a process that takes the connection over can have the requests the one before it
        // had read and not answered sent again (kernel 6.9 and later; a kernel that does not offer
        // it is not asked).
        let _ = config.add_capabilities(InitFlags::FUSE_HAS_RESEND);
        let mode = Mode::chosen(passthrough, self.passthrough, privileged());
        if mode == Mode::Passthrough {
            // Offered, so these succeed; a stack depth of 1 lets the job's overlay stack on the mount.
            let _ = config.add_capabilities(InitFlags::FUSE_PASSTHROUGH);
            let _ = config.set_max_stack_depth(1);
            self.core.enable_passthrough();
        }
        tracing::info!(passthrough, resend, ?mode, "tierstore fs: kernel offers");
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if let Some(put) = self.put_of(parent) {
            return fuse_put::lookup(put, parent.0, name, reply);
        }
        if let Some(put) = self
            .put
            .as_deref()
            .filter(|_| parent.0 == ino::ROOT && name == "put")
        {
            return match put.getattr(ino::PUT) {
                Ok(attr) => {
                    reply.entry(&Duration::ZERO, &fuse_put::file_attr(&attr), Generation(0))
                }
                Err(error) => reply.error(Errno::from_i32(error.raw_os_error())),
            };
        }
        let Some(name) = name.to_str() else {
            return reply.error(Errno::ENOENT);
        };
        finish(
            self.core.lookup(parent.0, name),
            reply,
            |looked, reply| match looked {
                Looked::Found(entry) => {
                    reply.entry(&entry.ttl, &file_attr(&entry.attr), Generation(0))
                }
                // An inode number of 0 is `ENOENT` that the kernel keeps for `ttl`.
                Looked::Absent(ttl) => reply.entry(&ttl, &file_attr(&ABSENT), Generation(0)),
            },
        );
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        if let Some(put) = self.put_of(ino) {
            return put.forget(ino.0, nlookup);
        }
        self.core.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if let Some(put) = self.put_of(ino) {
            return fuse_put::attr(put.getattr(ino.0), reply);
        }
        finish(self.core.getattr(ino.0), reply, |entry, reply| {
            reply.attr(&entry.ttl, &file_attr(&entry.attr));
        });
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        // The kernel asks only for a link, and the put directory has none.
        finish(self.core.readlink(ino.0), reply, |target, reply| {
            reply.data(target.as_bytes())
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if let Some(put) = self.put_of(ino) {
            return fuse_put::open(put.open(ino.0), reply);
        }
        let writing = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let opened = self
            .core
            .open(ino.0, writing, &mut |file| register(&reply, file));
        match opened {
            Err(error) => reply.error(errno(error)),
            Ok(Opened::Cached(fh)) => {
                reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
            }
            Ok(Opened::Backed { fh, backing }) => match backing.guard::<BackingId>() {
                Some(id) => reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), id),
                None => {
                    // Only this frontend registers ids, so this is not seen.
                    self.core.release(fh);
                    reply.error(Errno::EIO);
                }
            },
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if self.put_of(ino).is_some() {
            return reply.error(Errno::EACCES);
        }
        finish(
            self.core.read_open(ino.0, fh.0, offset, size),
            reply,
            |data, reply| reply.data(&data),
        );
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(put) = self.put_of(ino) {
            put.release(fh.0);
            return reply.ok();
        }
        self.core.release_open(ino.0, fh.0);
        reply.ok();
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.put_of(ino) {
            Some(put) => fuse_put::write(put, (ino.0, fh.0), offset, data, reply),
            None => reply.error(Errno::EROFS),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Nothing is kept to flush: a write reaches the store before it is answered; the flush
        // says the open is being closed. A tree is never written, and the kernel stops asking
        // after the first `ENOSYS`, as before.
        match self.put_of(ino) {
            Some(put) => {
                put.flush(fh.0);
                reply.ok();
            }
            None => reply.error(Errno::ENOSYS),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // The commit is the rename; a staged file needs no sync to be one.
        match self.put_of(ino) {
            Some(_) => reply.ok(),
            None => reply.error(Errno::ENOSYS),
        }
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        match self.put_of(ino) {
            Some(put) => fuse_put::statfs(put, reply),
            None => reply.statfs(0, 0, 0, 0, 0, 512, 255, 0),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if let Some(put) = self.put_of(ino) {
            return fuse_put::open(put.opendir(ino.0), reply);
        }
        finish(self.core.opendir(ino.0), reply, |(), reply| {
            reply.opened(FileHandle(0), FopenFlags::FOPEN_CACHE_DIR);
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if let Some(put) = self.put_of(ino) {
            return fuse_put::readdir(put, ino.0, offset, reply);
        }
        let listed = self.core.readdir(ino.0, offset, &mut |next, item| {
            let kind = file_attr(&item.entry.attr).kind;
            !reply.add(INodeNo(item.entry.attr.ino), next, kind, &item.name)
        });
        finish(listed, reply, |(), reply| reply.ok());
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        if let Some(put) = self.put_of(ino) {
            return fuse_put::readdirplus(put, ino.0, offset, reply);
        }
        let core = &self.core;
        let listed = core.readdir(ino.0, offset, &mut |next, item| {
            let attr = file_attr(&item.entry.attr);
            let ttl = item.entry.ttl;
            let full = reply.add(
                INodeNo(attr.ino.0),
                next,
                &item.name,
                &ttl,
                &attr,
                Generation(0),
            );
            // The kernel takes a lookup of each entry it was sent, but of `.` and `..`.
            if !full && !matches!(item.name.as_str(), "." | "..") {
                core.count(attr.ino.0);
            }
            !full
        });
        finish(listed, reply, |(), reply| reply.ok());
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(put) = self.put_of(ino) else {
            return reply.error(Errno::EROFS);
        };
        let others = [
            uid.is_some(),
            gid.is_some(),
            atime.is_some(),
            mtime.is_some(),
            ctime.is_some(),
            crtime.is_some(),
            chgtime.is_some(),
            bkuptime.is_some(),
            flags.is_some(),
        ]
        .contains(&true);
        fuse_put::attr(put.setattr(ino.0, (mode, size), others), reply);
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(self.refusal(parent));
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.put_of(parent) {
            Some(put) => fuse_put::mkdir(put, parent.0, name, reply),
            None => reply.error(Errno::EROFS),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.put_of(parent) {
            Some(put) => fuse_put::unlink(put, parent.0, name, reply),
            None => reply.error(Errno::EROFS),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(self.refusal(parent));
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(self.refusal(parent));
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        match self.put_of(parent) {
            Some(put) => {
                fuse_put::rename(put, (parent.0, name), (newparent.0, newname), flags, reply)
            }
            None => reply.error(Errno::EROFS),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(self.refusal(ino));
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.put_of(parent) {
            Some(put) => fuse_put::create(put, parent.0, name, reply),
            None => reply.error(Errno::EROFS),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(self.refusal(ino));
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(self.refusal(ino));
    }
}

#[cfg(test)]
#[path = "fuse_tests.rs"]
mod tests;
