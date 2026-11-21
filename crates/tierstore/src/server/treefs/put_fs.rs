//! `PutFs`: the put directory on `tsfuse` (`docs/design/fetch-cache-serve.md`, 5.2, section 18
//! row T-F9), the adapter of [`PutSpace`] onto [`tsfuse::Fs`]. It answers what the handler on
//! fuser that served the put directory before it answered, byte for byte, so that the switch of
//! `tierstore fs` from fuser to `tsfuse` (row T-R) changed no answer a job sees; the tests check
//! its replies against the golden files of that handler's (`put_fs_golden/`).
//!
//! It serves the inodes of `put/` in the numbering of the mount (`ino.rs`: `put/` is index 4 of
//! the namespace, every inode below it is in the put range) and the lookup of `put` in the
//! mount's root; [`PutFs::owns`] says which requests those are, for the frontend that serves the
//! trees beside it (`node_fs.rs`). A request it does not own is answered `ENOENT`.
//!
//! The opcodes (5.2, "Put directory mount", fixed here from the handler on fuser):
//! - LOOKUP, GETATTR, OPENDIR, READDIR, READDIRPLUS, RELEASEDIR, STATFS, as on fuser: every
//!   answer has a validity of zero (the store changes under the kernel), `statfs` is the stage's
//!   volume in blocks of 4 KiB;
//! - CREATE (answered with `FOPEN_DIRECT_IO`, so that each write reaches the store with its
//!   offset, no page of a staged file is cached and a file is never mapped), WRITE, FLUSH (tells
//!   the put directory that a descriptor was closed), RELEASE, FSYNC (does nothing: the commit is
//!   the rename), SETATTR (taken only when it changes nothing: the mode or the size the file has),
//!   MKDIR, UNLINK (the abort of a `.part` file);
//! - RENAME and RENAME2: `RENAME_NOREPLACE` is the only flag taken, any other is `EPERM`; a
//!   commit that waits or uploads is answered from the thread that runs it
//!   ([`PutSpace::rename_with`]) and the request is [`Handled::Parked`] meanwhile, so that no
//!   reader of the session waits for a commit;
//! - OPEN of a file that is not a create is refused by the put directory (`EPERM` for a staged
//!   file, `EACCES` for a committed name), READ is `EACCES` (a put file is never read back);
//! - SYMLINK, MKNOD, RMDIR, LINK, SETXATTR and REMOVEXATTR are `EPERM` (what the put directory does
//!   not do); GETXATTR and LISTXATTR are not in `tsfuse`'s list, so the session answers `ENOSYS`
//!   and the kernel answers `EOPNOTSUPP` without asking again, as on the trees;
//! - READLINK is `EINVAL`: nothing below `put/` is a link (the kernel asks only of a link);
//! - FORGET and BATCH_FORGET reach [`Fs::forget`], which lets a directory go once the kernel holds
//!   no lookup of it.
//!
//! A name that is not UTF-8 is not a name of the put directory: `EINVAL`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tsfuse::abi::{
    Attr, AttrOut, DirentBuf, DirentPlusBuf, EntryKind, EntryOut, Kstatfs, OpenOut, Operation,
    StatfsOut, WriteOut, fattr, fopen,
};
use tsfuse::{Errno, Fs, Handled, Replier, Request};

use super::ino;
use super::put::space::{PutAttr, PutItem, PutSpace};

/// The block size `statfs` and the attributes report.
const BLOCK: u32 = 4096;

/// The longest name `statfs` reports.
const NAME_MAX: u32 = 255;

/// Every answer below `put/` may be kept for no time.
const NOW: Duration = Duration::ZERO;

/// `RENAME_NOREPLACE` of `renameat2(2)`.
const RENAME_NOREPLACE: u32 = 1;

/// `S_IFDIR`, the type bits of a directory in `st_mode`.
const S_IFDIR: u32 = 0o040_000;

/// `S_IFREG`, the type bits of a regular file in `st_mode`.
const S_IFREG: u32 = 0o100_000;

/// The fields of a SETATTR that the put directory never changes: the owner, the group and the
/// times. (The mode and the size are taken when they change nothing; the handle, the lock owner
/// and `FATTR_*_NOW` without its time name no change of their own.)
const OTHERS: u32 = fattr::UID | fattr::GID | fattr::ATIME | fattr::MTIME | fattr::CTIME;

/// The wire errno of a refusal of the put directory.
fn sys(error: rustix::io::Errno) -> Errno {
    Errno(error.raw_os_error())
}

/// `name` as text; a name that is not UTF-8 is not a name of the put directory.
fn text(name: &[u8]) -> Result<&str, Errno> {
    std::str::from_utf8(name).map_err(|_| sys(rustix::io::Errno::INVAL))
}

/// The attributes the kernel is sent.
pub(super) fn attr(attr: &PutAttr) -> Attr {
    let (secs, nsecs) = (attr.ms / 1000, (attr.ms % 1000) as u32 * 1_000_000);
    let kind = if attr.dir { S_IFDIR } else { S_IFREG };
    Attr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: secs,
        mtime: secs,
        ctime: secs,
        atimensec: nsecs,
        mtimensec: nsecs,
        ctimensec: nsecs,
        mode: kind | u32::from(attr.perm),
        nlink: if attr.dir { 2 } else { 1 },
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: BLOCK,
        flags: 0,
    }
}

/// The lookup reply of `put_attr`.
pub(super) fn entry(put_attr: &PutAttr) -> EntryOut {
    EntryOut::new(put_attr.ino, 0, NOW, NOW, attr(put_attr))
}

/// The lookup reply of a name that is not there: an inode number of 0, which the kernel takes as
/// `ENOENT` and keeps for no time.
pub(super) fn absent() -> EntryOut {
    let mut out = entry(&PutAttr {
        ino: 0,
        dir: false,
        size: 0,
        perm: 0,
        uid: 0,
        gid: 0,
        ms: 0,
    });
    out.attr.nlink = 0;
    out
}

/// The type of a listed entry.
fn kind(item: &PutItem) -> EntryKind {
    if item.attr.dir {
        EntryKind::Directory
    } else {
        EntryKind::File
    }
}

/// Answers with `result`'s payload, or its errno.
fn answer(replier: Replier, result: Result<&[u8], Errno>) {
    // A device error is a connection that is gone; there is no one to tell.
    let _ = match result {
        Ok(payload) => replier.ok(&[payload]),
        Err(errno) => replier.error(errno),
    };
}

/// Answers with the bytes of `result`'s value, or its errno.
fn reply<T>(replier: Replier, result: Result<T, Errno>, bytes: fn(&T) -> &[u8]) {
    answer(replier, result.as_ref().map(bytes).map_err(|errno| *errno));
}

/// The put directory, served on `tsfuse`.
#[derive(Debug)]
pub(in crate::server) struct PutFs {
    space: Arc<PutSpace>,
}

impl PutFs {
    /// The put directory `space`, served.
    pub(in crate::server) fn new(space: Arc<PutSpace>) -> Self {
        Self { space }
    }

    /// Whether `req` is the put directory's: a request on an inode of `put/` (for LINK, the node
    /// linked, as the handler on fuser routes it), or the lookup of `put` in the mount's root.
    pub(in crate::server) fn owns(req: &Request<'_>) -> bool {
        let node = req.header.nodeid;
        match Operation::parse(req) {
            Ok(Operation::Link { arg, .. }) => ino::put_of(arg.oldnodeid).is_some(),
            Ok(Operation::Lookup { name }) if node == ino::ROOT => name == b"put",
            _ => ino::put_of(node).is_some(),
        }
    }

    /// Answers the lookup of `name` in `parent`, or of `put` in the root.
    fn lookup(&self, parent: u64, name: &[u8]) -> Result<EntryOut, Errno> {
        if parent == ino::ROOT {
            return self.space.getattr(ino::PUT).map(|a| entry(&a)).map_err(sys);
        }
        let looked = self.space.lookup(parent, text(name)?).map_err(sys)?;
        Ok(looked.as_ref().map_or_else(absent, entry))
    }

    /// Answers a SETATTR of `node`.
    fn setattr(&self, node: u64, arg: &tsfuse::abi::SetattrIn) -> Result<AttrOut, Errno> {
        let mode = (arg.valid & fattr::MODE != 0).then_some(arg.mode);
        let size = (arg.valid & fattr::SIZE != 0).then_some(arg.size);
        let others = arg.valid & OTHERS != 0;
        let changed = self
            .space
            .setattr(node, (mode, size), others)
            .map_err(sys)?;
        Ok(AttrOut::new(NOW, attr(&changed)))
    }

    /// Answers a CREATE of `name` in `parent`: the new file's entry and its open.
    fn create(&self, parent: u64, name: &[u8]) -> Result<Vec<u8>, Errno> {
        let (made, fh) = self.space.create(parent, text(name)?).map_err(sys)?;
        let open = OpenOut {
            fh,
            open_flags: fopen::DIRECT_IO,
            backing_id: 0,
        };
        Ok([entry(&made).as_bytes(), open.as_bytes()].concat())
    }

    /// Answers a STATFS: the stage's volume, in blocks of [`BLOCK`].
    fn statfs(&self) -> Result<StatfsOut, Errno> {
        let volume = self
            .space
            .space()
            .map_err(|error| Errno(error.raw_os_error().unwrap_or(Errno::EIO.0)))?;
        let (blocks, free) = (
            volume.total / u64::from(BLOCK),
            volume.free / u64::from(BLOCK),
        );
        Ok(StatfsOut {
            st: Kstatfs {
                blocks,
                bfree: free,
                bavail: free,
                files: 0,
                ffree: 0,
                bsize: BLOCK,
                namelen: NAME_MAX,
                frsize: BLOCK,
                padding: 0,
                spare: [0; 6],
            },
        })
    }

    /// The READDIR reply of `node` from `offset`, at most `size` bytes. A name the kernel would
    /// refuse ends the list, as a full one does (the store holds no such name).
    fn readdir(&self, node: u64, offset: u64, size: u32) -> Result<DirentBuf, Errno> {
        let mut list = DirentBuf::new(size as usize);
        self.space
            .readdir(node, offset, &mut |next, item| {
                list.push(item.attr.ino, next, kind(&item), item.name.as_bytes())
                    .is_ok()
            })
            .map_err(sys)?;
        Ok(list)
    }

    /// The READDIRPLUS reply of `node` from `offset`, at most `size` bytes: each entry with its
    /// attributes. The kernel counts a lookup of each entry it is sent but `.` and `..`; only the
    /// files are listed, and their numbers need no count.
    fn readdirplus(&self, node: u64, offset: u64, size: u32) -> Result<DirentPlusBuf, Errno> {
        let mut list = DirentPlusBuf::new(size as usize);
        self.space
            .readdir(node, offset, &mut |next, item| {
                let name = item.name.as_bytes();
                list.push(item.attr.ino, next, kind(&item), name, entry(&item.attr))
                    .is_ok()
            })
            .map_err(sys)?;
        Ok(list)
    }

    /// Answers a rename of `name` in `parent` to `new` in `new_parent` with the `RENAME_*`
    /// `flags`: inline, or from the thread of a commit that waits ([`Handled::Parked`]).
    fn rename(
        &self,
        (parent, name): (u64, &[u8]),
        (new_parent, new): (u64, &[u8]),
        flags: u32,
        replier: Replier,
    ) -> Handled {
        if flags & !RENAME_NOREPLACE != 0 {
            answer(replier, Err(sys(rustix::io::Errno::PERM)));
            return Handled::Done;
        }
        let names = text(name).and_then(|old| Ok((old, text(new)?)));
        let (old, new) = match names {
            Ok(names) => names,
            Err(errno) => {
                answer(replier, Err(errno));
                return Handled::Done;
            }
        };
        // Whether the answer came on this thread, before `rename_with` returned.
        let inline = Arc::new(AtomicBool::new(false));
        let here = (std::thread::current().id(), Arc::clone(&inline));
        let noreplace = flags & RENAME_NOREPLACE != 0;
        self.space
            .rename_with((parent, old), (new_parent, new), noreplace, move |done| {
                if std::thread::current().id() == here.0 {
                    here.1.store(true, Ordering::Release);
                }
                answer(replier, done.map(|()| &[][..]).map_err(sys));
            });
        if inline.load(Ordering::Acquire) {
            Handled::Done
        } else {
            Handled::Parked
        }
    }
}

impl Fs for PutFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let node = req.header.nodeid;
        let op = match Operation::parse(&req) {
            Ok(op) => op,
            Err(error) => {
                answer(replier, Err(error.errno()));
                return Handled::Done;
            }
        };
        if !Self::owns(&req) {
            answer(replier, Err(sys(rustix::io::Errno::NOENT)));
            return Handled::Done;
        }
        let space = &self.space;
        let empty: Result<&[u8], Errno> = Ok(&[]);
        match op {
            Operation::Lookup { name } => {
                reply(replier, self.lookup(node, name), EntryOut::as_bytes)
            }
            Operation::Getattr(_) => {
                let got = space.getattr(node).map_err(sys);
                let out = got.map(|a| AttrOut::new(NOW, attr(&a)));
                reply(replier, out, AttrOut::as_bytes);
            }
            Operation::Setattr(arg) => reply(replier, self.setattr(node, &arg), AttrOut::as_bytes),
            Operation::Mkdir { name, .. } => {
                let made = text(name).and_then(|name| space.mkdir(node, name).map_err(sys));
                reply(replier, made.map(|a| entry(&a)), EntryOut::as_bytes);
            }
            Operation::Unlink { name } => {
                let removed = text(name).and_then(|name| space.unlink(node, name).map_err(sys));
                answer(replier, removed.and(empty));
            }
            Operation::Rename { arg, name, newname } => {
                return self.rename((node, name), (arg.newdir, newname), 0, replier);
            }
            Operation::Rename2 { arg, name, newname } => {
                return self.rename((node, name), (arg.newdir, newname), arg.flags, replier);
            }
            Operation::Open(_) => {
                let opened = space.open(node).map_err(sys).map(|()| OpenOut::default());
                reply(replier, opened, OpenOut::as_bytes);
            }
            Operation::Opendir(_) => {
                let opened = space
                    .opendir(node)
                    .map_err(sys)
                    .map(|()| OpenOut::default());
                reply(replier, opened, OpenOut::as_bytes);
            }
            Operation::Read(_) => answer(replier, Err(sys(rustix::io::Errno::ACCESS))),
            Operation::Write { arg, data } => {
                let written = space.write((node, arg.fh), arg.offset, data).map_err(sys);
                // A write is at most `max_write` bytes, far below `u32::MAX`.
                let out = written.map(|n| WriteOut {
                    size: u32::try_from(n).unwrap_or(u32::MAX),
                    padding: 0,
                });
                reply(replier, out, WriteOut::as_bytes);
            }
            Operation::Statfs => reply(replier, self.statfs(), StatfsOut::as_bytes),
            Operation::Release(arg) => {
                space.release(arg.fh);
                answer(replier, empty);
            }
            Operation::Flush(arg) => {
                space.flush(arg.fh);
                answer(replier, empty);
            }
            Operation::Fsync(_) | Operation::Releasedir(_) => answer(replier, empty),
            Operation::Readdir(arg) => {
                let listed = self.readdir(node, arg.offset, arg.size);
                reply(replier, listed, DirentBuf::as_bytes);
            }
            Operation::Readdirplus(arg) => {
                let listed = self.readdirplus(node, arg.offset, arg.size);
                reply(replier, listed, DirentPlusBuf::as_bytes);
            }
            Operation::Create { name, .. } => {
                reply(replier, self.create(node, name), Vec::as_slice)
            }
            Operation::Readlink => answer(replier, Err(sys(rustix::io::Errno::INVAL))),
            Operation::Symlink { .. }
            | Operation::Mknod { .. }
            | Operation::Rmdir { .. }
            | Operation::Link { .. }
            | Operation::Setxattr
            | Operation::Removexattr { .. } => answer(replier, Err(sys(rustix::io::Errno::PERM))),
            // The session answers these itself and never hands them to a file system.
            Operation::Init(_)
            | Operation::Destroy
            | Operation::Interrupt(_)
            | Operation::Forget(_)
            | Operation::BatchForget { .. } => answer(replier, Err(Errno::ENOSYS)),
        }
        Handled::Done
    }

    fn forget(&self, nodeid: u64, nlookup: u64) {
        if ino::put_of(nodeid).is_some() {
            self.space.forget(nodeid, nlookup);
        }
    }
}

#[cfg(test)]
#[path = "put_fs_tests.rs"]
mod tests;
