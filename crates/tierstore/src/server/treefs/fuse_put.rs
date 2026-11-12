//! The FUSE frontend of the put directory: the kernel's requests on the inodes of `put/` decoded
//! into the calls of [`PutSpace`] (`put/space.rs`), and the answers encoded. It decides nothing but
//! what FUSE alone has: every answer has a timeout of zero (the store changes under the kernel); a
//! create is answered with `FOPEN_DIRECT_IO`, so that each write reaches the store with its offset,
//! no page of a staged file is cached, and a file is never mapped; `rename` takes only
//! `RENAME_NOREPLACE` of its flags; `flush` tells the put directory that a descriptor was closed
//! and `fsync` does nothing (the commit is the rename); `statfs` is the stage's volume; and what
//! the put directory does not do (`link`, `symlink`, `mknod`, `rmdir`, setting or removing an
//! extended attribute) is `EPERM`, while reading the extended attributes is `ENOSYS`, as on the
//! trees (the kernel then answers `EOPNOTSUPP` without asking again).

use std::ffi::OsStr;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite,
};

use super::put::space::{PutAttr, PutSpace};

/// The block size `statfs` and the attributes report.
const BLOCK: u32 = 4096;

/// The longest name.
const NAME_MAX: u32 = 255;

/// Every answer below `put/` may be kept for no time.
const NOW: Duration = Duration::ZERO;

/// The fuser errno of `error`.
fn errno(error: rustix::io::Errno) -> Errno {
    Errno::from_i32(error.raw_os_error())
}

/// The attributes the kernel is sent.
pub(super) fn file_attr(attr: &PutAttr) -> FileAttr {
    let when = UNIX_EPOCH + Duration::from_millis(attr.ms);
    FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: when,
        mtime: when,
        ctime: when,
        crtime: when,
        kind: if attr.dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: attr.perm,
        nlink: if attr.dir { 2 } else { 1 },
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: BLOCK,
        flags: 0,
    }
}

/// The attributes of a name that is not there: an inode number of 0, which the kernel takes as
/// `ENOENT` and keeps for no time.
fn absent() -> FileAttr {
    let mut attr = file_attr(&PutAttr {
        ino: 0,
        dir: false,
        size: 0,
        perm: 0,
        uid: 0,
        gid: 0,
        ms: 0,
    });
    attr.nlink = 0;
    attr
}

/// `name` as text; a name that is not UTF-8 is not a name of the put directory.
fn text(name: &OsStr) -> Result<&str, Errno> {
    name.to_str().ok_or(Errno::EINVAL)
}

/// Answers a lookup.
pub(super) fn lookup(space: &PutSpace, parent: u64, name: &OsStr, reply: ReplyEntry) {
    let looked = text(name).and_then(|name| space.lookup(parent, name).map_err(errno));
    match looked {
        Ok(Some(attr)) => reply.entry(&NOW, &file_attr(&attr), Generation(0)),
        Ok(None) => reply.entry(&NOW, &absent(), Generation(0)),
        Err(error) => reply.error(error),
    }
}

/// Answers a request whose answer is the attributes of the inode.
pub(super) fn attr(result: Result<PutAttr, rustix::io::Errno>, reply: ReplyAttr) {
    match result {
        Ok(attr) => reply.attr(&NOW, &file_attr(&attr)),
        Err(error) => reply.error(errno(error)),
    }
}

/// Answers a `mkdir`.
pub(super) fn mkdir(space: &PutSpace, parent: u64, name: &OsStr, reply: ReplyEntry) {
    match text(name).and_then(|name| space.mkdir(parent, name).map_err(errno)) {
        Ok(attr) => reply.entry(&NOW, &file_attr(&attr), Generation(0)),
        Err(error) => reply.error(error),
    }
}

/// Answers a create: the new file, open for writes that go straight to the store.
pub(super) fn create(space: &PutSpace, parent: u64, name: &OsStr, reply: ReplyCreate) {
    match text(name).and_then(|name| space.create(parent, name).map_err(errno)) {
        Ok((attr, fh)) => reply.created(
            &NOW,
            &file_attr(&attr),
            Generation(0),
            FileHandle(fh),
            FopenFlags::FOPEN_DIRECT_IO,
        ),
        Err(error) => reply.error(error),
    }
}

/// Answers an open of a file or a directory that is not a create.
pub(super) fn open(result: Result<(), rustix::io::Errno>, reply: ReplyOpen) {
    match result {
        Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
        Err(error) => reply.error(errno(error)),
    }
}

/// Answers a write.
pub(super) fn write(
    space: &PutSpace,
    (ino, fh): (u64, u64),
    offset: u64,
    data: &[u8],
    reply: ReplyWrite,
) {
    match space.write((ino, fh), offset, data) {
        // A write is at most `max_write` bytes, far below `u32::MAX`.
        Ok(written) => reply.written(u32::try_from(written).unwrap_or(u32::MAX)),
        Err(error) => reply.error(errno(error)),
    }
}

/// Answers a rename: `RENAME_NOREPLACE` is the only flag taken.
pub(super) fn rename(
    space: &PutSpace,
    from: (u64, &OsStr),
    to: (u64, &OsStr),
    flags: RenameFlags,
    reply: ReplyEmpty,
) {
    if !(flags - RenameFlags::RENAME_NOREPLACE).is_empty() {
        return reply.error(Errno::EPERM);
    }
    let names = text(from.1).and_then(|old| Ok((old, text(to.1)?)));
    let renamed = names.and_then(|(old, new)| {
        let noreplace = flags.contains(RenameFlags::RENAME_NOREPLACE);
        space
            .rename((from.0, old), (to.0, new), noreplace)
            .map_err(errno)
    });
    match renamed {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(error),
    }
}

/// Answers an unlink.
pub(super) fn unlink(space: &PutSpace, parent: u64, name: &OsStr, reply: ReplyEmpty) {
    match text(name).and_then(|name| space.unlink(parent, name).map_err(errno)) {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(error),
    }
}

/// Answers a listing.
pub(super) fn readdir(space: &PutSpace, ino: u64, offset: u64, mut reply: ReplyDirectory) {
    let listed = space.readdir(ino, offset, &mut |next, item| {
        let kind = file_attr(&item.attr).kind;
        !reply.add(INodeNo(item.attr.ino), next, kind, &item.name)
    });
    match listed {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(error)),
    }
}

/// Answers a listing with the attributes of each entry. The kernel counts a lookup of each entry
/// it is sent but `.` and `..`; only the files are listed, and their numbers need no count.
pub(super) fn readdirplus(space: &PutSpace, ino: u64, offset: u64, mut reply: ReplyDirectoryPlus) {
    let listed = space.readdir(ino, offset, &mut |next, item| {
        let attr = file_attr(&item.attr);
        !reply.add(attr.ino, next, &item.name, &NOW, &attr, Generation(0))
    });
    match listed {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(error)),
    }
}

/// Answers a `statfs`: the stage's volume, in blocks of [`BLOCK`].
pub(super) fn statfs(space: &PutSpace, reply: ReplyStatfs) {
    match space.space() {
        Ok(volume) => {
            let (blocks, free) = (
                volume.total / u64::from(BLOCK),
                volume.free / u64::from(BLOCK),
            );
            reply.statfs(blocks, free, free, 0, 0, BLOCK, NAME_MAX, BLOCK);
        }
        Err(error) => reply.error(Errno::from(error)),
    }
}

#[cfg(test)]
#[path = "fuse_put_tests.rs"]
mod tests;
