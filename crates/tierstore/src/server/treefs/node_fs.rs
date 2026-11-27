//! `NodeFs`: the FUSE frontend of the node's mount on `tsfuse` (`docs/design/fetch-cache-serve.md`
//! 5.2). It decodes the kernel's requests into the calls of the core (`core.rs`) and encodes the
//! answers. It decides nothing: what a lookup, an open or a listing answers is the core's; every
//! request that would change a tree is answered `EROFS`. Where `fs.put` is enabled, `put/` and
//! every inode below it are the put directory's, whose requests go to [`PutFs`] (which also
//! answers the lookup of `put` in the root).
//!
//! The mount (`docs/design/virtual-build-fs.md` section 6.3; `serving.rs`): `default_permissions`
//! (the kernel checks the modes, which the core reports), `allow_other` where the settings say
//! (jobs run as other users), and the INIT terms of `tsfuse`'s put mount, among them
//! `FUSE_CACHE_SYMLINKS`, `FUSE_PARALLEL_DIROPS`, `FUSE_DO_READDIRPLUS` and `FUSE_HAS_RESEND`
//! (so that a process that takes the connection over can have the requests the one before read
//! and did not answer sent again, `launch.rs`), and `max_background` 1024.
//!
//! The opcodes:
//! - LOOKUP, GETATTR, READLINK, OPENDIR, READDIR, READDIRPLUS: the core's answer, with its
//!   validity. A name that is not there is an entry of inode 0 valid for the core's time, which
//!   the kernel keeps as a negative entry; a name that is not UTF-8 is not there (`ENOENT`).
//!   READDIRPLUS counts a lookup of every entry it sends but `.` and `..`, as the kernel takes one.
//! - OPEN: every open is a cached open (`FOPEN_KEEP_CACHE`): every read the page cache misses is
//!   answered here, from the cache file of the blob or the span of the pack. On a connection an
//!   earlier release answered with passthrough, the open of a blob that release registered a
//!   backing id for is answered `FOPEN_PASSTHROUGH` with the id (`backing.rs`); this release never
//!   asks for passthrough. OPEN and OPENDIR are never answered `ENOSYS`, so the `NO_OPEN` flags the
//!   handshake grants change nothing. OPENDIR is answered `FOPEN_CACHE_DIR`.
//! - READ, RELEASE, RELEASEDIR, STATFS (no blocks, blocks of 512, names of 255): as the core
//!   says. A read or a release that names a handle an earlier process gave out is the core's to
//!   make sense of (`core/adopt.rs`).
//! - FLUSH and FSYNC: `ENOSYS` (a tree is never written; the kernel stops sending them after the
//!   first).
//! - SETATTR, MKNOD, MKDIR, UNLINK, RMDIR, SYMLINK, RENAME, RENAME2, LINK, WRITE, CREATE, SETXATTR,
//!   REMOVEXATTR: `EROFS`. GETXATTR and LISTXATTR are not in `tsfuse`'s opcodes: the session
//!   answers them `ENOSYS`, and the kernel stops asking after the first.
//! - FORGET and BATCH_FORGET reach the core (or the put directory) through [`Fs::forget`].
//!
//! Every request of the core is answered on the reader that read it, inline, but those that may
//! wait on the fill socket: an open whose bytes the core cannot have without asking the node
//! (`core/now.rs`), a read of a handle an earlier process gave out, and a lookup in the namespace
//! are parked and answered by the fetches' threads (`fetches.rs`), so that a node that takes fills
//! and does not answer holds none of the readers. (A rename of the put
//! directory whose commit waits or uploads is parked and answered from the commit's thread,
//! `put_fs.rs`.) The session leaves INTERRUPTs unanswered (`serving.rs`), so every request gets
//! its own answer; where the kernel does not take one (the connection is gone, or a session that
//! answered an INTERRUPT `EINTR` first), what the core counted for it (the lookup of a LOOKUP or of
//! a READDIRPLUS's entries, the open of an OPEN) is given back, so that no tree is held in memory
//! by a lookup the kernel never forgets.
//!
//! Every OPEN is timed from its arrival to its answer, on the reader or on a thread of the
//! fetches, and counted as waiting meanwhile (`core/opens.rs`); so is every request the session
//! hands the frontend, by the kind of its opcode ([`request_kind`], `core/requests.rs`): LOOKUP,
//! GETATTR, OPEN, READ, READDIR and READDIRPLUS, READLINK, and any other as `other`, a body too
//! short for its opcode among them. The requests of the put directory are not, nor FORGET and
//! BATCH_FORGET, which have no answer, nor what the session answers itself (INIT, INTERRUPT, an
//! opcode it does not know). The stop of the readers tells the node of the opens and the requests
//! once they are all answered ([`NodeFs::drain`]).

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;

use tsfuse::abi::{
    Attr, AttrOut, DirPushError, DirentBuf, DirentPlusBuf, EntryKind, EntryOut, Kstatfs, OpenOut,
    Operation, ReadIn, StatfsOut, fopen,
};
use tsfuse::{Completion, Errno, Fs, Handled, Replier, Request};

use super::backing::BackingId;
use super::core::{self, Core, FIXED_MTIME, Failing, Kind, Looked, Opened};
use super::fetches::Fetches;
use super::ino;
use super::put_fs::PutFs;
use crate::server::local_cache::requests::RequestKind;

/// `ENOENT`.
const ENOENT: Errno = Errno(2);
/// `EACCES`.
const EACCES: Errno = Errno(13);
/// `ENOTDIR`.
const ENOTDIR: Errno = Errno(20);
/// `EISDIR`.
const EISDIR: Errno = Errno(21);
/// `EINVAL`.
const EINVAL: Errno = Errno(22);
/// `EROFS`.
const EROFS: Errno = Errno(30);

/// `S_IFDIR`, `S_IFREG` and `S_IFLNK`: the type bits of `st_mode`.
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// `O_ACCMODE`: the bits of an open's flags that say whether it reads, writes or both.
const O_ACCMODE: u32 = 0o3;

/// The block size the attributes report.
const BLKSIZE: u32 = 4096;

/// What a lookup of a name that is not there answers: an inode number of 0.
const ABSENT: core::Attr = core::Attr {
    ino: 0,
    kind: Kind::File,
    size: 0,
    perm: 0,
    nlink: 0,
    uid: 0,
    gid: 0,
};

/// The kernel's errno of the core's.
fn errno(error: core::Errno) -> Errno {
    match error {
        core::Errno::NoEnt => ENOENT,
        core::Errno::Acces => EACCES,
        core::Errno::NotDir => ENOTDIR,
        core::Errno::IsDir => EISDIR,
        core::Errno::Inval => EINVAL,
        core::Errno::Io => Errno::EIO,
        core::Errno::RoFs => EROFS,
    }
}

/// The attributes the kernel is sent: every time is the core's fixed one.
fn attr(attr: &core::Attr) -> Attr {
    let kind = match attr.kind {
        Kind::Dir => S_IFDIR,
        Kind::File => S_IFREG,
        Kind::Symlink => S_IFLNK,
    };
    Attr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: FIXED_MTIME,
        mtime: FIXED_MTIME,
        ctime: FIXED_MTIME,
        mode: kind | u32::from(attr.perm),
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        blksize: BLKSIZE,
        ..Attr::default()
    }
}

/// The entry of `entry`, its name and its attributes valid for the core's time.
fn entry(entry: &core::Entry) -> EntryOut {
    EntryOut::new(entry.attr.ino, 0, entry.ttl, entry.ttl, attr(&entry.attr))
}

/// The type of a listed entry.
fn kind(kind: Kind) -> EntryKind {
    match kind {
        Kind::Dir => EntryKind::Directory,
        Kind::File => EntryKind::File,
        Kind::Symlink => EntryKind::Symlink,
    }
}

/// Answers with `result`'s payload, or its errno; whether the kernel took the answer (not when an
/// INTERRUPT answered the request first, nor when the connection is gone).
fn send(replier: Replier, result: Result<&[u8], Errno>) -> bool {
    let completion = match result {
        Ok(payload) => replier.ok(&[payload]),
        Err(error) => replier.error(error),
    };
    matches!(completion, Completion::Written)
}

/// The filesystem the kernel sees.
pub(super) struct NodeFs {
    core: Arc<Core>,
    /// The put directory, where it is served (the core's).
    put: Option<PutFs>,
    /// The threads that answer the requests that may wait on the fill socket (`fetches.rs`).
    fetches: Fetches,
}

/// The kind of the request `op`, for `tierstore_fs_request_seconds` (`core/requests.rs`).
fn request_kind(op: &Operation<'_>) -> RequestKind {
    match op {
        Operation::Lookup { .. } => RequestKind::Lookup,
        Operation::Getattr(_) => RequestKind::Getattr,
        Operation::Open(_) => RequestKind::Open,
        Operation::Read(_) => RequestKind::Read,
        Operation::Readdir(_) | Operation::Readdirplus(_) => RequestKind::Readdir,
        Operation::Readlink => RequestKind::Readlink,
        _ => RequestKind::Other,
    }
}

/// Whether a LOOKUP of `name` in `parent` may ask the fill socket: one in the namespace (a tree
/// of `trees/` resolved, a placement looked up), but that of `trees` in the root.
fn lookup_asks(parent: u64, name: &[u8]) -> bool {
    let namespace = ino::split(parent).0 == 0 || ino::is_placed_dir(parent);
    namespace && !(parent == ino::ROOT && name == b"trees")
}

/// LOOKUP of `name` in `parent`, answered from `core`.
fn lookup(core: &Core, parent: u64, name: &[u8], replier: Replier) {
    let Ok(name) = std::str::from_utf8(name) else {
        send(replier, Err(ENOENT));
        return;
    };
    match core.lookup(parent, name) {
        Ok(Looked::Found(found)) => {
            if !send(replier, Ok(entry(&found).as_bytes())) {
                core.forget(found.attr.ino, 1);
            }
        }
        Ok(Looked::Absent(ttl)) => {
            let absent = EntryOut::new(0, 0, ttl, ttl, attr(&ABSENT));
            send(replier, Ok(absent.as_bytes()));
        }
        Err(error) => {
            send(replier, Err(errno(error)));
        }
    }
}

/// Answers the OPEN of `node` with what `core` opened; whether that is a handle, whether or not the
/// kernel takes it (an open whose answer is not taken was made all the same).
fn answer_open(
    core: &Core,
    node: u64,
    opened: Result<Opened, core::Errno>,
    replier: Replier,
) -> bool {
    let (fh, out) = match opened {
        Err(error) => {
            send(replier, Err(errno(error)));
            return false;
        }
        Ok(Opened::Cached(fh)) => (
            fh,
            OpenOut {
                fh,
                open_flags: fopen::KEEP_CACHE,
                backing_id: 0,
            },
        ),
        Ok(Opened::Adopted { fh, id }) => {
            let backing = id
                .downcast_ref::<BackingId>()
                .and_then(|id| i32::try_from(id.0).ok());
            let Some(backing_id) = backing else {
                // Only `serving.rs` makes the handles of ids, of this type: not seen.
                core.release(fh);
                let number = ino::split(node).0;
                core.say_failed((number, Failing::Other), None, &|| {
                    format!(
                        "tierstore fs: cannot open the inode {node}: its backing id of an \
                         earlier release is not one this process can answer with"
                    )
                });
                send(replier, Err(Errno::EIO));
                return false;
            };
            (
                fh,
                OpenOut {
                    fh,
                    open_flags: fopen::PASSTHROUGH,
                    backing_id,
                },
            )
        }
    };
    if !send(replier, Ok(out.as_bytes())) {
        core.release(fh);
    }
    true
}

impl NodeFs {
    /// A frontend over `core`, whose requests that may wait on the fill socket are answered by
    /// `fetchers` threads of their own (`fetches.rs`).
    ///
    /// # Errors
    /// What the system said when a thread cannot be started.
    pub(super) fn new(core: Arc<Core>, fetchers: NonZeroUsize) -> io::Result<Self> {
        Ok(Self {
            put: core.put().cloned().map(PutFs::new),
            fetches: Fetches::start(fetchers)?,
            core,
        })
    }

    /// Waits until every request parked on the fetches is answered (`Fetches::drain`), then tells
    /// the node of the opens (`core/opens.rs`), of the requests (`core/requests.rs`) and of the
    /// tree lists (`core/lists.rs`): the stop of the readers for a hand-over or a restart, after
    /// which this process answers no more opens.
    pub(super) fn drain(&self) {
        self.fetches.drain();
        self.core.report_opens();
        self.core.report_requests();
        self.core.report_tree_lists();
    }

    /// The requests parked on the fetches and not answered yet.
    #[cfg(test)]
    pub(super) fn parked(&self) -> usize {
        self.fetches.busy()
    }

    /// Answers `job` on a thread of the fetches (`fetches.rs`).
    fn park(&self, job: impl FnOnce(&Core) + Send + 'static) -> Handled {
        let core = Arc::clone(&self.core);
        self.fetches.park(Box::new(move || job(&core)));
        Handled::Parked
    }

    /// READDIR, or READDIRPLUS with `plus`, of `node` from the offset of `arg`.
    fn readdir(&self, node: u64, arg: &ReadIn, plus: bool, replier: Replier) {
        let size = arg.size as usize;
        let mut plain = DirentBuf::new(size);
        let mut full = DirentPlusBuf::new(size);
        let mut counted = Vec::new();
        let mut unnamed = false;
        let listed = self.core.readdir(node, arg.offset, &mut |next, item| {
            let listed = &item.entry.attr;
            let name = item.name.as_bytes();
            let pushed = if plus {
                full.push(
                    listed.ino,
                    next,
                    kind(listed.kind),
                    name,
                    entry(&item.entry),
                )
            } else {
                plain.push(listed.ino, next, kind(listed.kind), name)
            };
            match pushed {
                Ok(()) => {
                    // The kernel takes a lookup of each entry it was sent, but of `.` and `..`.
                    if plus && !matches!(item.name.as_str(), "." | "..") {
                        counted.push(listed.ino);
                    }
                    true
                }
                Err(DirPushError::Full) => false,
                Err(DirPushError::InvalidName) => {
                    unnamed = true;
                    false
                }
            }
        });
        if let Err(error) = listed {
            send(replier, Err(errno(error)));
            return;
        }
        if unnamed {
            // No entry the core lists has such a name; the kernel would refuse the listing.
            let number = ino::split(node).0;
            self.core.say_failed((number, Failing::Other), None, &|| {
                format!(
                    "tierstore fs: cannot list the directory of the inode {node}: an entry has a \
                     name the kernel refuses"
                )
            });
            send(replier, Err(Errno::EIO));
            return;
        }
        // Counted before the answer, so that a FORGET of an entry, which can follow the answer at
        // once, finds its count.
        for &ino in &counted {
            self.core.count(ino);
        }
        let bytes = if plus {
            full.as_bytes()
        } else {
            plain.as_bytes()
        };
        if !send(replier, Ok(bytes)) {
            for ino in counted {
                self.core.forget(ino, 1);
            }
        }
    }
}

impl Fs for NodeFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        if let Some(put) = self.put.as_ref().filter(|_| PutFs::owns(&req)) {
            return put.handle(req, replier);
        }
        // Timed from now to the answer, under the kind of its opcode (`core/requests.rs`).
        let arrival = self.core.request_begins();
        let node = req.header.nodeid;
        let op = match Operation::parse(&req) {
            Ok(op) => op,
            Err(error) => {
                send(replier, Err(error.errno()));
                self.core.request_ends(arrival, RequestKind::Other);
                return Handled::Done;
            }
        };
        let kind = request_kind(&op);
        match op {
            Operation::Lookup { name } if lookup_asks(node, name) => {
                let name = name.to_vec();
                return self.park(move |core| {
                    lookup(core, node, &name, replier);
                    core.request_ends(arrival, kind);
                });
            }
            Operation::Lookup { name } => lookup(&self.core, node, name, replier),
            Operation::Getattr(_) => {
                let got = self.core.getattr(node).map_err(errno);
                let out = got.map(|found| AttrOut::new(found.ttl, attr(&found.attr)));
                send(replier, out.as_ref().map(AttrOut::as_bytes).map_err(|&e| e));
            }
            Operation::Readlink => {
                let target = self.core.readlink(node).map_err(errno);
                send(
                    replier,
                    target.as_deref().map(str::as_bytes).map_err(|&e| e),
                );
            }
            Operation::Open(arg) => {
                let writing = arg.flags & O_ACCMODE != 0;
                // Timed and waiting from now to the answer (`core/opens.rs`).
                let timing = self.core.open_begins();
                let Some(opened) = self.core.open_now(node, writing) else {
                    // Parked, and answered by `fs.fetch_timeout` from now.
                    return self.park(move |core| {
                        let mut timing = timing;
                        let opened = core.open_from(node, writing, &mut timing);
                        let ok = answer_open(core, node, opened, replier);
                        core.open_ends(timing, ok);
                        core.request_ends(arrival, kind);
                    });
                };
                let ok = answer_open(&self.core, node, opened, replier);
                self.core.open_ends(timing, ok);
            }
            Operation::Read(arg) if self.core.read_reopens(arg.fh) => {
                return self.park(move |core| {
                    let read = core.read_open(node, arg.fh, arg.offset, arg.size);
                    send(replier, read.as_deref().map_err(|&error| errno(error)));
                    core.request_ends(arrival, kind);
                });
            }
            Operation::Read(arg) => {
                let read = self.core.read_open(node, arg.fh, arg.offset, arg.size);
                send(replier, read.as_deref().map_err(|&error| errno(error)));
            }
            Operation::Release(arg) => {
                self.core.release(arg.fh);
                send(replier, Ok(&[]));
            }
            Operation::Releasedir(_) => {
                send(replier, Ok(&[]));
            }
            Operation::Opendir(_) => {
                let out = self.core.opendir(node).map_err(errno).map(|()| OpenOut {
                    fh: 0,
                    open_flags: fopen::CACHE_DIR,
                    backing_id: 0,
                });
                send(replier, out.as_ref().map(OpenOut::as_bytes).map_err(|&e| e));
            }
            Operation::Readdir(arg) => self.readdir(node, &arg, false, replier),
            Operation::Readdirplus(arg) => self.readdir(node, &arg, true, replier),
            Operation::Statfs => {
                let st = Kstatfs {
                    bsize: 512,
                    namelen: 255,
                    ..Kstatfs::default()
                };
                send(replier, Ok(StatfsOut { st }.as_bytes()));
            }
            Operation::Setattr(_)
            | Operation::Mknod { .. }
            | Operation::Mkdir { .. }
            | Operation::Unlink { .. }
            | Operation::Rmdir { .. }
            | Operation::Symlink { .. }
            | Operation::Rename { .. }
            | Operation::Rename2 { .. }
            | Operation::Link { .. }
            | Operation::Write { .. }
            | Operation::Create { .. }
            | Operation::Setxattr
            | Operation::Removexattr { .. } => {
                send(replier, Err(EROFS));
            }
            // FLUSH and FSYNC: nothing of a tree is written. The others the session answers
            // itself, and never hands to a file system.
            _ => {
                send(replier, Err(Errno::ENOSYS));
            }
        }
        self.core.request_ends(arrival, kind);
        Handled::Done
    }

    fn forget(&self, nodeid: u64, nlookup: u64) {
        match &self.put {
            Some(put) if ino::put_of(nodeid).is_some() => put.forget(nodeid, nlookup),
            _ => self.core.forget(nodeid, nlookup),
        }
    }
}

#[cfg(test)]
#[path = "node_fs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "node_fs_put_tests.rs"]
mod put_tests;
