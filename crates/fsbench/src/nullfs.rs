//! The null file system (`fsbench null-fs`): the files of a list, served read-only with no storage
//! behind them (every byte is zero), on the FUSE library `tierstore fs` uses and with its
//! settings of the transport: the same `fuser` commit, worker threads reading one descriptor,
//! `max_background` 1024, `FUSE_PARALLEL_DIROPS`, `default_permissions`. What it costs per
//! request is the transport's cost alone, the floor a variant of the server is held against.
//!
//! What it does not do that `tierstore fs` does: passthrough (every read reaches this process),
//! `READDIRPLUS`, symbolic links, resend and hand-over, and negative entries (a name that is not
//! there is `ENOENT`, which the kernel does not keep; `tierstore fs` answers inode 0, which it
//! keeps for the timeout). The benchmark looks up only names that are there.
//!
//! Two settings make the regimes of the benchmark: the timeouts the kernel may keep an entry
//! and its attributes for (0 sends a `LOOKUP` and a `GETATTR` for every `stat`), and the open
//! mode: `explicit` answers every `OPEN` with `FOPEN_KEEP_CACHE`, as `tierstore fs` does, and
//! `none` answers the first with `ENOSYS`, after which the kernel sends no `OPEN` or `RELEASE`
//! for a file of this mount (no-open, `FUSE_NO_OPEN_SUPPORT`).
//!
//! The inodes are those of [`Tree`]. Entries report the user and group that mounted it, mode
//! 0444 for files and 0555 for directories, and a fixed modification time of 0.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyOpen, Request, Session, SessionACL,
};

use crate::Error;
use crate::list::FileList;
pub use crate::tree::Tree;
use crate::tree::{Miss, Node};

/// The background requests the kernel may have outstanding at once: `tierstore fs`'s value.
const MAX_BACKGROUND: u16 = 1024;

/// The bytes a read is answered from (a read asks for at most `max_pages` pages, 1 MiB).
static ZEROS: [u8; 1 << 20] = [0; 1 << 20];

/// How `OPEN` is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OpenMode {
    /// Every `OPEN` is answered, with `FOPEN_KEEP_CACHE`.
    Explicit,
    /// `ENOSYS`: the kernel opens files of the mount without asking from then on.
    #[value(name = "none")]
    NoOpen,
}

/// What the null file system serves with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// How long the kernel may keep an entry.
    pub entry_timeout: Duration,
    /// How long the kernel may keep attributes.
    pub attr_timeout: Duration,
    /// How `OPEN` is answered.
    pub open: OpenMode,
    /// The threads that read requests.
    pub threads: usize,
    /// Whether users other than the one that mounted it may use it (`allow_other`).
    pub allow_other: bool,
}

/// The errno of a miss.
fn errno(miss: Miss) -> Errno {
    match miss {
        Miss::NotFound => Errno::ENOENT,
        Miss::NotDir => Errno::ENOTDIR,
        Miss::IsDir => Errno::EISDIR,
    }
}

/// Answers `reply` with `answer` when `result` is a value, and with `fail` (its error reply)
/// when it is not.
fn finish<R, T>(reply: R, result: Result<T, Miss>, fail: fn(R, Errno), answer: impl FnOnce(R, T)) {
    match result {
        Ok(value) => answer(reply, value),
        Err(miss) => fail(reply, errno(miss)),
    }
}

/// The null file system of a tree.
#[derive(Debug)]
pub struct NullFs {
    tree: Tree,
    options: Options,
    owner: (u32, u32),
}

impl NullFs {
    /// Serves `tree` with `options`; entries report `owner` (user, group).
    #[must_use]
    pub fn new(tree: Tree, options: Options, owner: (u32, u32)) -> Self {
        Self {
            tree,
            options,
            owner,
        }
    }

    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let (kind, perm, size, nlink) = match node {
            Node::Dir { .. } => (FileType::Directory, 0o555, 0, 2),
            Node::File { size, .. } => (FileType::RegularFile, 0o444, *size, 1),
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: self.owner.0,
            gid: self.owner.1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

/// The mount's settings: those of `tierstore fs`, read-only, with `threads` workers.
#[must_use]
pub fn config(options: &Options) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("fsbench-null".to_owned()),
        MountOption::Subtype("fsbench-null".to_owned()),
        MountOption::DefaultPermissions,
        MountOption::RO,
    ];
    config.acl = if options.allow_other {
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    config.n_threads = Some(options.threads);
    config.clone_fd = false;
    config
}

impl Filesystem for NullFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // A kernel that does not offer it runs without it, as `tierstore fs` does.
        let _ = config.add_capabilities(InitFlags::FUSE_PARALLEL_DIROPS);
        let _ = config.set_max_background(MAX_BACKGROUND);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        finish(
            reply,
            self.tree.lookup(parent.0, name.as_bytes()),
            ReplyEntry::error,
            |reply, (ino, node)| {
                reply.entry_with_ttls(
                    &self.options.attr_timeout,
                    &self.options.entry_timeout,
                    &self.attr(ino, node),
                    Generation(0),
                );
            },
        );
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        finish(
            reply,
            self.tree.node(ino.0),
            ReplyAttr::error,
            |reply, node| {
                reply.attr(&self.options.attr_timeout, &self.attr(ino.0, node));
            },
        );
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.options.open {
            OpenMode::Explicit => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            OpenMode::NoOpen => reply.error(Errno::ENOSYS),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        finish(
            reply,
            self.tree.read(ino.0, offset, size),
            ReplyData::error,
            |reply, span| {
                // A READ asks for at most `max_pages` pages, which `ZEROS` holds.
                let n = usize::try_from(span.len).map_or(ZEROS.len(), |n| n.min(ZEROS.len()));
                reply.data(&ZEROS[..n]);
            },
        );
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        finish(
            reply,
            self.tree.entries(ino.0),
            ReplyDirectory::error,
            |mut reply, entries| {
                for (i, (child, name, node)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    let kind = match node {
                        Node::Dir { .. } => FileType::Directory,
                        Node::File { .. } => FileType::RegularFile,
                    };
                    if reply.add(INodeNo(child), i as u64 + 1, kind, name) {
                        break;
                    }
                }
                reply.ok();
            },
        );
    }
}

/// Mounts the null file system of `list` at `mountpoint`, writes `mounted` and a newline to
/// `output`, serves until `input` ends, then unmounts.
///
/// # Errors
/// The list names a file as a directory, the mount fails, or the unmount does.
pub fn serve(
    list: &FileList,
    mountpoint: &Path,
    options: Options,
    mut input: impl Read,
    mut output: impl Write,
) -> Result<(), Error> {
    let tree = Tree::of(list)?;
    let owner = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let fs = NullFs::new(tree, options, owner);
    let session = Session::new(fs, mountpoint, &config(&options))
        .and_then(Session::spawn)
        .map_err(Error::at("mount", mountpoint))?;
    writeln!(output, "mounted")
        .and_then(|()| output.flush())
        .map_err(Error::of("null-fs", "write"))?;
    // The end of the input is the signal to stop; what it holds is not kept.
    let _ = std::io::copy(&mut input, &mut std::io::sink());
    session
        .umount_and_join()
        .map_err(Error::at("unmount", mountpoint))
}

#[cfg(test)]
#[path = "nullfs_tests.rs"]
mod tests;
