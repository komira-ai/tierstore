//! The FUSE frontend: decodes the kernel's requests into the calls of the core (`core.rs`) and
//! encodes the answers. It decides nothing: what a lookup, an open or a listing answers is the
//! core's; every request that would change a tree is answered `EROFS`, and the extended
//! attributes `ENOSYS`, so that the kernel stops asking after the first.
//!
//! The mount (`docs/design/virtual-build-fs.md` section 6.3): `default_permissions` (the kernel
//! checks the modes, which the core reports), `allow_other` (jobs run as other users), the
//! capabilities `FUSE_CACHE_SYMLINKS`, `FUSE_PARALLEL_DIROPS` and `FUSE_DO_READDIRPLUS`, and
//! `max_background` 1024. Opens are cached opens (`FOPEN_KEEP_CACHE`): every read the page cache
//! misses is answered here from the cache file; the kernel here offers no passthrough in the
//! mode this release runs (the 6.8 mode).

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption,
    OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
    SessionACL, TimeOrNow,
};

use super::core::{self, Attr, Core, Entry, Kind, Looked};

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

/// The kernel type of an entry.
fn kind_of(entry: &Entry) -> FileType {
    file_attr(&entry.attr).kind
}

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

/// The filesystem the kernel sees.
pub(super) struct Frontend {
    core: Arc<Core>,
}

impl Frontend {
    /// A frontend over `core`.
    pub(super) fn new(core: Arc<Core>) -> Self {
        Self { core }
    }
}

impl Filesystem for Frontend {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        for flag in [
            InitFlags::FUSE_CACHE_SYMLINKS,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_DO_READDIRPLUS,
        ] {
            // A kernel that does not offer one runs without it; each is a saving.
            let _ = config.add_capabilities(flag);
        }
        let _ = config.set_max_background(MAX_BACKGROUND);
        let offered = config.capabilities();
        tracing::info!(
            "tierstore fs: the kernel offers passthrough: {}, resend: {}; this release runs \
             with cached opens",
            offered.contains(InitFlags::FUSE_PASSTHROUGH),
            offered.contains(InitFlags::FUSE_HAS_RESEND)
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            return reply.error(Errno::ENOENT);
        };
        match self.core.lookup(parent.0, name) {
            Ok(Looked::Found(entry)) => {
                reply.entry(&entry.ttl, &file_attr(&entry.attr), Generation(0));
            }
            Ok(Looked::Absent(ttl)) => {
                // An inode number of 0 is `ENOENT` that the kernel keeps for `ttl`.
                let absent = file_attr(&Attr {
                    ino: 0,
                    kind: Kind::File,
                    size: 0,
                    perm: 0,
                    nlink: 0,
                    uid: 0,
                    gid: 0,
                });
                reply.entry(&ttl, &absent, Generation(0));
            }
            Err(error) => reply.error(errno(error)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.core.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.core.getattr(ino.0) {
            Ok(entry) => reply.attr(&entry.ttl, &file_attr(&entry.attr)),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.core.readlink(ino.0) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let writing = flags.acc_mode() != OpenAccMode::O_RDONLY;
        match self.core.open(ino.0, writing) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.core.read(fh.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.core.release(fh.0);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.core.opendir(ino.0) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::FOPEN_CACHE_DIR),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let listed = self.core.readdir(ino.0, offset, &mut |next, item| {
            !reply.add(
                INodeNo(item.entry.attr.ino),
                next,
                kind_of(&item.entry),
                &item.name,
            )
        });
        match listed {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let core = &self.core;
        let listed = core.readdir(ino.0, offset, &mut |next, item| {
            let attr = file_attr(&item.entry.attr);
            let full = reply.add(
                INodeNo(item.entry.attr.ino),
                next,
                &item.name,
                &item.entry.ttl,
                &attr,
                Generation(0),
            );
            // The kernel takes a lookup of each entry it was sent, but of `.` and `..`.
            if !full && item.name != "." && item.name != ".." {
                core.count(item.entry.attr.ino);
            }
            !full
        });
        match listed {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(error)),
        }
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

    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }
}

#[cfg(test)]
#[path = "fuse_tests.rs"]
mod tests;
