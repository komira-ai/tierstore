//! `TreeFs`: a tree mount on `tsfuse` (`docs/design/fetch-cache-serve.md`, 5.2 and 5.3, section
//! 18 row T-F8), the adapter of a [`Tree`] of `tierstore-tree` onto [`tsfuse::Fs`]. One mount
//! serves one tree: the mount's root is the tree's root, and an inode number is the entry's index
//! in the image plus one (the root is index 0, FUSE's root inode 1), so FORGET and BATCH_FORGET
//! change nothing and need no answer (the session does not pass them on).
//!
//! The opcodes (5.2, "Tree mount"):
//! - LOOKUP, GETATTR, READLINK: from the mapped image, inline. A name that is not there is a
//!   lookup answered with inode 0, which the kernel keeps as a negative entry; every entry and
//!   attribute is valid for [`TREE_TTL`] (the tree is immutable), with one size and one mtime
//!   ([`FIXED_MTIME`]) per node, so the kernel never drops the page cache of a file (K19).
//! - READDIR, READDIRPLUS: `.`, `..`, then the directory's children in name order, encoded
//!   straight from their records; the offset of an entry is its position plus one.
//! - OPEN and OPENDIR: `ENOSYS`, every one. The session negotiated `NO_OPEN_SUPPORT` and
//!   `NO_OPENDIR_SUPPORT`, so the first `ENOSYS` stops the kernel sending them; every one that
//!   arrives before it lands is answered the same (K18). The kernel then keeps a file's page cache
//!   across opens and sends no RELEASE; RELEASE and RELEASEDIR are answered if they come. FLUSH
//!   and FSYNC are `ENOSYS` (the kernel stops sending them too).
//! - READ: the reader protocol of 5.1 through [`Tree::read`], the bytes staged by the session's
//!   [`ReplyPath`] (splice above its threshold, `pread` below), committed only after the
//!   post-read recheck. A read whose cells are not all `present` (or changed while read) is
//!   [`Handled::Parked`]: a task on the parker runtime waits ([`Tree::wait`]) and reads again
//!   from the start, and its reply goes to the request's own device. An INTERRUPT ends the wait
//!   (the session answered `EINTR`).
//! - The lost path (4.6): a wait answered `lost` marks the tree lost; every parked read of the
//!   tree, whatever pack it waits on, is answered `EIO`, and from then on every READ, STATFS,
//!   READDIR and READDIRPLUS is answered `EIO` until the mount is released (the tree dropped).
//!   A job's after-exit check reads that `EIO` as an unreadable input, and the job is retried.
//!   LOOKUP, GETATTR and READLINK keep answering from the image, which is intact.
//! - STATFS: what the tree mount on fuser answers (no blocks, blocks of 512, names of 255).
//! - SETATTR, MKDIR, UNLINK, RENAME, WRITE, CREATE: `EROFS`. Anything else: `ENOSYS`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tierstore_fetch_api::list::NodeIndex;
use tierstore_tree::{
    NAME_MAX, Node, NodeKind, ReadFailure, ReadStep, Stage, Staged, Tree, Wait, Waited,
};
use tsfuse::abi::{
    Attr, AttrOut, DirentBuf, DirentPlusBuf, EntryKind, EntryOut, Kstatfs, Operation, ReadIn,
    StatfsOut,
};
use tsfuse::{
    CancelToken, Completion, Errno, FillFailed, Filled, Fs, Handled, Replier, ReplyPath, Request,
};

use super::core::{FIXED_MTIME, TREE_TTL};

/// `ENOENT`.
const ENOENT: Errno = Errno(2);
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

/// The counts of a tree mount's answers that the tree's own counts do not have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct TreeFsStats {
    /// OPEN and OPENDIR answered `ENOSYS`.
    pub opens_refused: u64,
    /// READs answered from the parker runtime, after a wait.
    pub parked_answered: u64,
}

/// A tree mount: the adapter of a [`Tree`] onto [`Fs`].
#[derive(Debug)]
pub(in crate::server) struct TreeFs {
    tree: Arc<Tree>,
    reply: Arc<ReplyPath>,
    parker: tokio::runtime::Handle,
    owner: (u32, u32),
    opens_refused: AtomicU64,
    parked_answered: Arc<AtomicU64>,
}

/// A READ's reply being built: the session's reply path and the request's replier.
struct Reply<'p> {
    path: &'p ReplyPath,
    replier: Replier,
}

/// The bytes of a READ staged in the reply path, not yet written to the device.
struct Filling<'p> {
    path: &'p ReplyPath,
    filled: Filled<'p>,
}

impl<'p> Stage for Reply<'p> {
    type Staged = Filling<'p>;
    type Error = FillFailed;

    fn fill(
        self,
        data: std::os::fd::BorrowedFd<'_>,
        offset: u64,
        len: usize,
    ) -> Result<Filling<'p>, FillFailed> {
        let path = self.path;
        path.fill(self.replier, data, offset, len)
            .map(|filled| Filling { path, filled })
    }
}

impl<'p> Staged for Filling<'p> {
    type Back = Reply<'p>;
    type Committed = Completion;

    fn commit(self) -> Completion {
        self.filled.commit()
    }

    fn discard(self) -> Reply<'p> {
        Reply {
            path: self.path,
            replier: self.filled.discard(),
        }
    }
}

/// The errno of a failed read.
fn read_errno(failure: &ReadFailure) -> Errno {
    match failure {
        ReadFailure::NotFound => ENOENT,
        ReadFailure::NotAFile => EISDIR,
        ReadFailure::Bad { .. } | ReadFailure::Lost | ReadFailure::Broken(_) => Errno::EIO,
    }
}

/// Answers with `result`'s payload, or its errno. A device error is a connection that is gone:
/// there is no one to tell.
fn answer(replier: Replier, result: Result<&[u8], Errno>) {
    let _ = match result {
        Ok(payload) => replier.ok(&[payload]),
        Err(errno) => replier.error(errno),
    };
}

/// What one step of a READ leaves to do.
enum Next {
    /// Answered.
    Done,
    /// Wait, then read again.
    Park(Replier, Wait),
}

/// Runs [`Tree::read`] once and answers whatever it ends in but a park.
fn read_once(
    tree: &Tree,
    path: &ReplyPath,
    node: NodeIndex,
    arg: &ReadIn,
    replier: Replier,
) -> Next {
    settle(tree.read(node, arg.offset, arg.size, Reply { path, replier }))
}

/// Answers what a [`Tree::read`] ended in but a park, and hands a park on.
fn settle(step: ReadStep<Reply<'_>>) -> Next {
    match step {
        ReadStep::Served(_) => Next::Done,
        ReadStep::Empty(reply) => {
            answer(reply.replier, Ok(&[]));
            Next::Done
        }
        ReadStep::Failed(reply, failure) => {
            answer(reply.replier, Err(read_errno(&failure)));
            Next::Done
        }
        ReadStep::FillFailed(failed) => {
            answer(failed.replier, Err(Errno::EIO));
            Next::Done
        }
        ReadStep::Park(reply, wait) => Next::Park(reply.replier, wait),
    }
}

/// What a parked READ keeps while it waits: no buffer and no counter (4.8).
struct ParkedRead {
    tree: Arc<Tree>,
    path: Arc<ReplyPath>,
    node: NodeIndex,
    arg: ReadIn,
    cancel: CancelToken,
    answered: Arc<AtomicU64>,
}

/// A parked READ: waits, reads again, until it is answered, interrupted or the tree is lost.
async fn parked(read: ParkedRead, mut replier: Replier, mut wait: Wait) {
    let ParkedRead {
        tree,
        path,
        node,
        arg,
        cancel,
        answered,
    } = read;
    loop {
        let waited = tokio::select! {
            waited = tree.wait(&wait) => waited,
            // The session answered the INTERRUPT with `EINTR`; the replier writes nothing more.
            () = cancel.cancelled() => return,
        };
        if waited != Waited::Retry {
            answer(replier, Err(Errno::EIO));
            answered.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match read_once(&tree, &path, node, &arg, replier) {
            Next::Done => {
                answered.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Next::Park(back, again) => {
                replier = back;
                wait = again;
            }
        }
    }
}

impl TreeFs {
    /// The mount of `tree`: READ replies go through `reply` (the session's reply path), parked
    /// READs wait on `parker` (a runtime of the mount process, not a reader thread), and every
    /// node is owned by `owner` (uid, gid).
    pub(in crate::server) fn new(
        tree: Arc<Tree>,
        reply: Arc<ReplyPath>,
        parker: tokio::runtime::Handle,
        owner: (u32, u32),
    ) -> Self {
        Self {
            tree,
            reply,
            parker,
            owner,
            opens_refused: AtomicU64::new(0),
            parked_answered: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The counts of the mount's answers.
    pub(in crate::server) fn stats(&self) -> TreeFsStats {
        TreeFsStats {
            opens_refused: self.opens_refused.load(Ordering::Relaxed),
            parked_answered: self.parked_answered.load(Ordering::Relaxed),
        }
    }

    /// The entry of inode `nodeid`: index `nodeid - 1` of the image.
    fn node(&self, nodeid: u64) -> Result<Node, Errno> {
        nodeid
            .checked_sub(1)
            .and_then(|index| self.tree.image().node(NodeIndex(index)))
            .ok_or(ENOENT)
    }

    /// The number of `node`'s subdirectories.
    fn subdirs(&self, first: u64, count: u64) -> u32 {
        let image = self.tree.image();
        let dirs = (first..first.saturating_add(count))
            .filter_map(|child| image.node(NodeIndex(child)))
            .filter(|child| matches!(child.kind, NodeKind::Dir { .. }))
            .count();
        u32::try_from(dirs).unwrap_or(u32::MAX)
    }

    /// The attributes of `node`.
    fn attr(&self, node: &Node) -> Attr {
        let (kind, perm, size, nlink) = match node.kind {
            NodeKind::Dir { first, count } => (
                S_IFDIR,
                u32::from(node.mode),
                0,
                self.subdirs(first, count).saturating_add(2),
            ),
            NodeKind::File { size, .. } => (S_IFREG, u32::from(node.mode), size, 1),
            NodeKind::Symlink { len } => (S_IFLNK, 0o777, len, 1),
        };
        Attr {
            ino: node.index.0 + 1,
            size,
            blocks: size.div_ceil(512),
            atime: FIXED_MTIME,
            mtime: FIXED_MTIME,
            ctime: FIXED_MTIME,
            atimensec: 0,
            mtimensec: 0,
            ctimensec: 0,
            mode: kind | perm,
            nlink,
            uid: self.owner.0,
            gid: self.owner.1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The lookup reply of `node`.
    fn entry(&self, node: &Node) -> EntryOut {
        EntryOut::new(node.index.0 + 1, 0, TREE_TTL, TREE_TTL, self.attr(node))
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<EntryOut, Errno> {
        let parent = self.node(parent)?;
        let image = self.tree.image();
        Ok(
            match image
                .lookup(parent.index, name)
                .and_then(|child| image.node(child))
            {
                Some(child) => self.entry(&child),
                // Inode 0: `ENOENT`, kept by the kernel for the validity, as the tree never changes.
                None => {
                    let mut absent = EntryOut::new(0, 0, TREE_TTL, TREE_TTL, Attr::default());
                    absent.attr.nlink = 0;
                    absent
                }
            },
        )
    }

    fn readlink(&self, nodeid: u64) -> Result<Vec<u8>, Errno> {
        let node = self.node(nodeid)?;
        self.tree.image().read_link(&node).ok_or(EINVAL)
    }

    /// The entries of directory `nodeid` from position `arg.offset`, as many as fit `arg.size`.
    fn readdir(&self, nodeid: u64, arg: &ReadIn, plus: bool) -> Result<Vec<u8>, Errno> {
        if self.tree.is_lost() {
            return Err(Errno::EIO);
        }
        let dir = self.node(nodeid)?;
        let NodeKind::Dir { first, count } = dir.kind else {
            return Err(ENOTDIR);
        };
        let image = self.tree.image();
        let size = arg.size as usize;
        let (mut plain, mut with) = (DirentBuf::new(size), DirentPlusBuf::new(size));
        let end = count.saturating_add(2);
        let mut name = [0u8; NAME_MAX];
        for position in arg.offset..end {
            let (ino, kind, entry, label): (u64, EntryKind, EntryOut, &[u8]) = match position {
                0 => (nodeid, EntryKind::Directory, EntryOut::default(), b"."),
                1 => (
                    dir.parent.0 + 1,
                    EntryKind::Directory,
                    EntryOut::default(),
                    b"..",
                ),
                _ => {
                    let child = first
                        .checked_add(position - 2)
                        .and_then(|child| image.node(NodeIndex(child)))
                        .ok_or(Errno::EIO)?;
                    let kind = match child.kind {
                        NodeKind::Dir { .. } => EntryKind::Directory,
                        NodeKind::File { .. } => EntryKind::File,
                        NodeKind::Symlink { .. } => EntryKind::Symlink,
                    };
                    let label = image.name(&child, &mut name).ok_or(Errno::EIO)?;
                    (child.index.0 + 1, kind, self.entry(&child), label)
                }
            };
            let pushed = if plus {
                with.push(ino, position + 1, kind, label, entry)
            } else {
                plain.push(ino, position + 1, kind, label)
            };
            if pushed.is_err() {
                break;
            }
        }
        Ok(if plus {
            with.as_bytes()
        } else {
            plain.as_bytes()
        }
        .to_vec())
    }

    fn statfs(&self) -> Result<StatfsOut, Errno> {
        if self.tree.is_lost() {
            return Err(Errno::EIO);
        }
        Ok(StatfsOut {
            st: Kstatfs {
                bsize: 512,
                namelen: 255,
                ..Kstatfs::default()
            },
        })
    }

    fn read(&self, nodeid: u64, arg: ReadIn, replier: Replier) -> Handled {
        let Some(node) = nodeid.checked_sub(1).map(NodeIndex) else {
            answer(replier, Err(ENOENT));
            return Handled::Done;
        };
        let cancel = replier.cancel_token();
        match read_once(&self.tree, &self.reply, node, &arg, replier) {
            Next::Done => Handled::Done,
            Next::Park(replier, wait) => {
                let read = ParkedRead {
                    tree: Arc::clone(&self.tree),
                    path: Arc::clone(&self.reply),
                    node,
                    arg,
                    cancel,
                    answered: Arc::clone(&self.parked_answered),
                };
                self.parker.spawn(parked(read, replier, wait));
                Handled::Parked
            }
        }
    }
}

impl Fs for TreeFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let nodeid = req.header.nodeid;
        let op = match Operation::parse(&req) {
            Ok(op) => op,
            Err(error) => {
                answer(replier, Err(error.errno()));
                return Handled::Done;
            }
        };
        match op {
            Operation::Read(arg) => return self.read(nodeid, arg, replier),
            Operation::Lookup { name } => match self.lookup(nodeid, name) {
                Ok(entry) => answer(replier, Ok(entry.as_bytes())),
                Err(errno) => answer(replier, Err(errno)),
            },
            Operation::Getattr(_) => match self.node(nodeid) {
                Ok(node) => answer(
                    replier,
                    Ok(AttrOut::new(TREE_TTL, self.attr(&node)).as_bytes()),
                ),
                Err(errno) => answer(replier, Err(errno)),
            },
            Operation::Readlink => {
                let target = self.readlink(nodeid);
                answer(replier, target.as_deref().map_err(|e| *e));
            }
            Operation::Readdir(arg) | Operation::Readdirplus(arg) => {
                let plus = matches!(op, Operation::Readdirplus(_));
                let listed = self.readdir(nodeid, &arg, plus);
                answer(replier, listed.as_deref().map_err(|e| *e));
            }
            Operation::Statfs => match self.statfs() {
                Ok(out) => answer(replier, Ok(out.as_bytes())),
                Err(errno) => answer(replier, Err(errno)),
            },
            Operation::Open(_) | Operation::Opendir(_) => {
                self.opens_refused.fetch_add(1, Ordering::Relaxed);
                answer(replier, Err(Errno::ENOSYS));
            }
            Operation::Release(_) | Operation::Releasedir(_) => answer(replier, Ok(&[])),
            Operation::Setattr(_)
            | Operation::Mkdir { .. }
            | Operation::Unlink { .. }
            | Operation::Rename { .. }
            | Operation::Write { .. }
            | Operation::Create { .. } => answer(replier, Err(EROFS)),
            _ => answer(replier, Err(Errno::ENOSYS)),
        }
        Handled::Done
    }
}

#[cfg(test)]
#[path = "tree_fs_harness.rs"]
mod tree_fs_harness;

#[cfg(test)]
#[path = "tree_fs_adapter_tests.rs"]
mod tree_fs_adapter_tests;

#[cfg(test)]
#[path = "tree_fs_tests.rs"]
mod tree_fs_tests;

#[cfg(test)]
#[path = "tree_fs_read_tests.rs"]
mod tree_fs_read_tests;

#[cfg(test)]
#[path = "tree_fs_kernel_tests.rs"]
mod kernel_tests;
