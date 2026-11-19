//! The null file system on `tsfuse` (`fsbench tsfuse-null-fs`): the files of a list, served
//! read-only by the FUSE library of the redesign (`docs/design/fetch-cache-serve.md`, 5.2), with
//! nothing of tierstore behind it. It is the variants V3, V4 and V5 of the benchmark (17.1):
//! `tsfuse` on `/dev/fuse` with one reader or N cloned readers (one descriptor and one thread
//! each), READ replies by `pread` and `writev` (V3) or by splice above a threshold (V4, with or
//! without `SPLICE_F_MOVE`), and `tsfuse` over io_uring ([`Transport::Uring`], V5: a ring and a
//! thread per possible CPU, `depth` entries each, beside the `/dev/fuse` readers that carry
//! INTERRUPT and FORGET; where the session cannot use io_uring it serves on `/dev/fuse` and
//! [`Served`] says why), in the regimes of [`crate::nullfs`]: the timeouts the kernel keeps
//! entries and attributes for, and the open mode (`explicit` answers every `OPEN` and `OPENDIR`,
//! with `FOPEN_KEEP_CACHE` for a file; `none` answers them `ENOSYS`, after which the kernel,
//! which was granted `FUSE_NO_OPEN_SUPPORT` and `FUSE_NO_OPENDIR_SUPPORT`, opens without asking).
//!
//! Its settings of the session are those of a tree mount of the redesign
//! ([`InitConfig::tree`]: requests of up to 1 MiB, readahead 128 KiB, 1024 background requests,
//! the flags of 5.2; over io_uring, requests of up to [`URING_MAX_PAGES`] pages, as an io_uring
//! tree mount asks for, [`tsfuse::UringConfig`]) and its mount is a tree mount (`ro`, `nosuid`, `nodev`,
//! `default_permissions`), named `fsbench-tsfuse`.
//!
//! **Where the bytes come from.** A READ is answered through [`ReplyPath`] from a data file (as
//! a tree mount answers from the pack files of the node cache): the files of the list one after
//! another, each on a 4 KiB boundary ([`Tree`]). The data file is made, or extended, to its
//! length with a hole, so its bytes are zeros unless the operator wrote the files' bytes into
//! it; either way they come from the data file's page cache once read.
//!
//! **Requests.** `LOOKUP`, `GETATTR`, `OPEN`, `READ`, `OPENDIR`, `READDIR`, `READDIRPLUS`,
//! `RELEASE` and `RELEASEDIR` are answered inline on the reader that read them; every other
//! request `tsfuse` implements (`FLUSH`, `STATFS`, writes) is answered `ENOSYS`, as `fuser`'s
//! defaults answer `FLUSH` for [`crate::nullfs`], and a body too short for its opcode `EIO`.
//! `tsfuse` answers `INIT`, `INTERRUPT`, `FORGET`, `DESTROY` and the opcodes it does not
//! implement itself.

use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::os::fd::AsFd as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tsfuse::abi::{
    Attr, AttrOut, DirentBuf, DirentPlusBuf, EntryKind, EntryOut, InitConfig, Negotiated, OpenOut,
    Operation, ReadIn, fopen,
};
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{
    Completion, DevFd, Errno, Fs, Handled, Replier, ReplyConfig, ReplyCounts, ReplyPath, Request,
    Serving, Session, TransportMode, UringConfig,
};

use crate::Error;
use crate::list::FileList;
use crate::nullfs::OpenMode;
use crate::tree::{Miss, Node, Span, Tree};

/// The name of the process in its errors.
const PROCESS: &str = "tsfuse-null-fs";

/// The source and type the mount table shows.
const FSNAME: &str = "fsbench-tsfuse";

/// How READ replies are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Reply {
    /// `pread` into a pooled buffer and one `writev` (V3).
    Writev,
    /// Splice through a pooled pipe at and above the threshold, `pread` and `writev` below it
    /// (V4).
    Splice,
}

/// The largest request of a session over io_uring, pages: 32 (128 KiB payloads, the largest
/// buffered READ, K21), so that the default depth registers at most 64 MiB on 88 possible CPUs
/// ([`UringConfig`], 5.2 item 4). A session on `/dev/fuse` asks for [`InitConfig::tree`]'s 256.
pub const URING_MAX_PAGES: u16 = 32;

/// The transport a session asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `/dev/fuse` readers alone ([`Session::init`]).
    Dev,
    /// io_uring with `config` ([`Session::init_uring`]); `/dev/fuse` where the session cannot use
    /// it.
    Uring(UringConfig),
}

impl Transport {
    /// The INIT terms of a tree session on this transport.
    #[must_use]
    pub fn init_config(self) -> InitConfig {
        match self {
            Self::Dev => InitConfig::tree(),
            Self::Uring(_) => InitConfig {
                max_pages: URING_MAX_PAGES,
                ..InitConfig::tree()
            },
        }
    }
}

/// What a session serves on, as [`Served`] and the storm's records say it: `io_uring`, or
/// `dev-fuse` and why not io_uring (`dev-fuse (Disabled)`); and the queues, depth and bytes the
/// ring entries register (`uring_registered_bytes`), on io_uring alone.
#[must_use]
pub fn transport_of(mode: TransportMode) -> (String, Option<tsfuse::UringMemory>) {
    match mode {
        TransportMode::Uring(memory) => ("io_uring".to_owned(), Some(memory)),
        TransportMode::DevFuse(why) => (format!("dev-fuse ({why:?})"), None),
    }
}

/// What the null file system on `tsfuse` serves with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// How long the kernel may keep an entry.
    pub entry_timeout: Duration,
    /// How long the kernel may keep attributes.
    pub attr_timeout: Duration,
    /// How `OPEN` and `OPENDIR` are answered.
    pub open: OpenMode,
    /// The readers: the mount's descriptor and the clones bound beside it, a thread each.
    pub readers: NonZeroUsize,
    /// How READ replies are written.
    pub reply: Reply,
    /// The smallest reply that is spliced, in data bytes ([`Reply::Splice`] only).
    pub splice_min: usize,
    /// `SPLICE_F_MOVE` on the splice into the device ([`Reply::Splice`] only).
    pub move_pages: bool,
    /// Whether users other than the one that mounted it may use it (`allow_other`).
    pub allow_other: bool,
    /// The transport the session asks for.
    pub transport: Transport,
}

/// The configuration of the reply path for `options`: pipes sized for the largest READ a tree
/// session asks for (the kernel may grant fewer pages, never more), and one idle pipe and buffer
/// kept per reader.
#[must_use]
pub fn reply_config(options: &Options) -> ReplyConfig {
    ReplyConfig {
        splice_min: match options.reply {
            Reply::Writev => None,
            Reply::Splice => Some(options.splice_min),
        },
        max_data: usize::from(options.transport.init_config().max_pages)
            * rustix::param::page_size(),
        pool: options.readers.get(),
        move_pages: options.move_pages,
    }
}

/// The mount: a tree mount named [`FSNAME`], open to its owner alone unless asked.
#[must_use]
pub fn mount_options(options: &Options) -> MountOptions {
    MountOptions {
        allow_other: options.allow_other,
        subtype: Some(FSNAME.to_owned()),
        ..MountOptions::tree(FSNAME)
    }
}

/// The errno of a miss.
fn errno(miss: Miss) -> Errno {
    Errno(miss.errno())
}

/// The null file system of a tree on `tsfuse`.
#[derive(Debug)]
pub struct TsNullFs {
    tree: Tree,
    options: Options,
    owner: (u32, u32),
    data: File,
    path: ReplyPath,
}

impl TsNullFs {
    /// Serves `tree` with `options` from `data` (read at the places of [`Tree`]); entries report
    /// `owner` (user, group).
    ///
    /// # Errors
    /// The reply path's first pipe cannot be made.
    pub fn new(tree: Tree, options: Options, owner: (u32, u32), data: File) -> Result<Self, Error> {
        let path = ReplyPath::new(reply_config(&options)).map_err(Error::of(PROCESS, "pipe"))?;
        Ok(Self {
            tree,
            options,
            owner,
            data,
            path,
        })
    }

    /// The bytes of the READ replies written so far, by the way they went.
    #[must_use]
    pub fn counts(&self) -> ReplyCounts {
        self.path.counts()
    }

    /// The bytes the reply path's pipes hold.
    #[must_use]
    pub fn pipe_size(&self) -> usize {
        self.path.pipe_size()
    }

    fn attr(&self, ino: u64, node: &Node) -> Attr {
        let (mode, size, nlink) = match node {
            Node::Dir { .. } => (0o40_555, 0, 2),
            Node::File { size, .. } => (0o100_444, *size, 1),
        };
        Attr {
            ino,
            size,
            blocks: size.div_ceil(512),
            mode,
            nlink,
            uid: self.owner.0,
            gid: self.owner.1,
            blksize: 4096,
            ..Attr::default()
        }
    }

    fn entry(&self, ino: u64, node: &Node) -> EntryOut {
        let attr = self.attr(ino, node);
        EntryOut::new(
            ino,
            0,
            self.options.entry_timeout,
            self.options.attr_timeout,
            attr,
        )
    }

    /// The reply to an `OPEN` (`open_flags` on success) or an `OPENDIR`.
    fn open(&self, replier: Replier, open_flags: u32) -> Completion {
        match self.options.open {
            OpenMode::Explicit => {
                let out = OpenOut {
                    fh: 0,
                    open_flags,
                    backing_id: 0,
                };
                replier.ok(&[out.as_bytes()])
            }
            OpenMode::NoOpen => replier.error(Errno::ENOSYS),
        }
    }

    /// The reply to a READ of `span`: its bytes of the data file, through the reply path.
    fn read(&self, replier: Replier, span: Span) -> Completion {
        // A span is at most a READ's `size`, a `u32`.
        let len = usize::try_from(span.len).unwrap_or(usize::MAX);
        match self.path.fill(replier, self.data.as_fd(), span.at, len) {
            Ok(filled) => filled.commit(),
            Err(failed) => failed.replier.error(Errno::EIO),
        }
    }

    /// The reply to a READDIR (or, `plus`, a READDIRPLUS) of directory `ino` from `arg.offset`:
    /// as many entries as fit `arg.size` bytes.
    fn list(&self, replier: Replier, ino: u64, arg: ReadIn, plus: bool) -> Completion {
        let entries = match self.tree.entries(ino) {
            Ok(entries) => entries,
            Err(miss) => return replier.error(errno(miss)),
        };
        let max = arg.size as usize;
        let (mut short, mut long) = (DirentBuf::new(max), DirentPlusBuf::new(max));
        let skip = usize::try_from(arg.offset).unwrap_or(usize::MAX);
        for (i, (child, name, node)) in entries.into_iter().enumerate().skip(skip) {
            let kind = match node {
                Node::Dir { .. } => EntryKind::Directory,
                Node::File { .. } => EntryKind::File,
            };
            let off = i as u64 + 1;
            let pushed = match plus {
                true => long.push(child, off, kind, name.as_bytes(), self.entry(child, node)),
                false => short.push(child, off, kind, name.as_bytes()),
            };
            // Full: the kernel asks again from the last offset taken.
            if pushed.is_err() {
                break;
            }
        }
        match plus {
            true => replier.ok(&[long.as_bytes()]),
            false => replier.ok(&[short.as_bytes()]),
        }
    }
}

impl Fs for TsNullFs {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let ino = req.header.nodeid;
        // A device error has nobody to tell: the connection is gone or the kernel refused the
        // reply, and the request is over either way.
        let _ = match Operation::parse(&req) {
            Ok(Operation::Lookup { name }) => match self.tree.lookup(ino, name) {
                Ok((child, node)) => replier.ok(&[self.entry(child, node).as_bytes()]),
                Err(miss) => replier.error(errno(miss)),
            },
            Ok(Operation::Getattr(_)) => match self.tree.node(ino) {
                Ok(node) => {
                    let out = AttrOut::new(self.options.attr_timeout, self.attr(ino, node));
                    replier.ok(&[out.as_bytes()])
                }
                Err(miss) => replier.error(errno(miss)),
            },
            Ok(Operation::Open(_)) => self.open(replier, fopen::KEEP_CACHE),
            Ok(Operation::Opendir(_)) => self.open(replier, 0),
            Ok(Operation::Read(arg)) => match self.tree.read(ino, arg.offset, arg.size) {
                Ok(span) => self.read(replier, span),
                Err(miss) => replier.error(errno(miss)),
            },
            Ok(Operation::Readdir(arg)) => self.list(replier, ino, arg, false),
            Ok(Operation::Readdirplus(arg)) => self.list(replier, ino, arg, true),
            Ok(Operation::Release(_) | Operation::Releasedir(_)) => replier.ok(&[]),
            Ok(_) => replier.error(Errno::ENOSYS),
            Err(e) => replier.error(e.errno()),
        };
        Handled::Done
    }
}

/// Opens the data file at `path`, made if it is not there and extended (with a hole) to `len`
/// bytes if it is shorter.
///
/// # Errors
/// The file cannot be opened, its length read, or extended.
pub fn open_data(path: &Path, len: u64) -> Result<File, Error> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(Error::at("open", path))?;
    let have = file.metadata().map_err(Error::at("stat", path))?.len();
    if have < len {
        file.set_len(len).map_err(Error::at("extend", path))?;
    }
    Ok(file)
}

/// INIT on `device` with the terms of a tree mount on `transport`, `readers` descriptors, and a
/// reader on each serving `fs` (and, on io_uring, a thread per queue): the session serving and
/// the terms INIT agreed.
///
/// # Errors
/// INIT fails, a clone cannot be bound, or a reader cannot be started.
pub fn start(
    device: DevFd,
    readers: NonZeroUsize,
    transport: Transport,
    fs: Arc<dyn Fs>,
) -> Result<(Serving, Negotiated), Error> {
    let config = transport.init_config();
    let mut session = match transport {
        Transport::Dev => Session::init(device, &config),
        Transport::Uring(uring) => Session::init_uring(device, &config, &uring),
    }
    .map_err(Error::of(PROCESS, "init"))?;
    session
        .clone_readers(readers)
        .map_err(Error::of(PROCESS, "clone the readers"))?;
    let negotiated = *session.negotiated();
    let serving = session
        .serve(fs)
        .map_err(Error::of(PROCESS, "start the readers"))?;
    Ok((serving, negotiated))
}

/// Detaches the mount when dropped armed: a failure between the mount and its unmount leaves no
/// mount behind. With the mount detached, the kernel ends the connection, and the readers end
/// on their own.
pub(crate) struct Detach {
    mount: Mount,
    armed: bool,
}

impl Detach {
    /// Detaches `mount` when dropped, unless disarmed.
    pub(crate) fn new(mount: Mount) -> Self {
        Self { mount, armed: true }
    }

    /// The mount was unmounted: nothing to do when dropped.
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Detach {
    fn drop(&mut self) {
        if self.armed {
            // An error is already on its way to the caller; this one would only hide it.
            let _ = self.mount.clone().unmount(true);
        }
    }
}

/// What the null file system says when it has unmounted: one line of JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Served {
    /// The readers it served with.
    pub readers: usize,
    /// The flags INIT granted.
    pub granted: String,
    /// The flags of 5.2 the kernel did not offer.
    pub missing: String,
    /// The `max_pages` the session asked for.
    pub max_pages: u16,
    /// The bytes each pipe of the reply path holds.
    pub pipe_size: usize,
    /// Data bytes of READ replies spliced (`splice_bytes_total`).
    pub splice_bytes: u64,
    /// Data bytes of READ replies written with `writev` (`pread_bytes_total`).
    pub pread_bytes: u64,
    /// What the session served on ([`transport_of`]).
    pub transport: String,
    /// The bytes the ring entries registered (`uring_registered_bytes`); `None` on `/dev/fuse`.
    pub uring_registered_bytes: Option<u64>,
    /// The entries of each queue; `None` on `/dev/fuse`.
    pub uring_depth: Option<u16>,
    /// The queues, one per possible CPU; `None` on `/dev/fuse`.
    pub uring_queues: Option<u32>,
}

/// Mounts the null file system of `list` at `mountpoint` on `tsfuse`, reading from the data file
/// at `data`, writes `mounted` and a newline to `output`, serves until `input` ends, then
/// unmounts, waits for the readers, and writes a [`Served`] line.
///
/// # Errors
/// The list names a file as a directory, the data file cannot be opened, the mount fails, the
/// session or the output fails (the mount is then detached), the unmount does, or a reader
/// failed.
pub fn serve(
    list: &FileList,
    mountpoint: &Path,
    data: &Path,
    options: Options,
    mut input: impl Read,
    mut output: impl Write,
) -> Result<(), Error> {
    let tree = Tree::of(list)?;
    let data = open_data(data, tree.data_len())?;
    let owner = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    let fs = Arc::new(TsNullFs::new(tree, options, owner, data)?);
    let (device, mount) = Mounter::default()
        .mount(mountpoint, &mount_options(&options))
        .map_err(Error::of(PROCESS, "mount"))?;
    let detach = Detach::new(mount.clone());
    let (serving, negotiated) = start(device, options.readers, options.transport, fs.clone())?;
    let (transport, memory) = transport_of(serving.transport());
    writeln!(output, "mounted")
        .and_then(|()| output.flush())
        .map_err(Error::of(PROCESS, "write"))?;
    // The end of the input is the signal to stop; what it holds is not kept.
    let _ = std::io::copy(&mut input, &mut std::io::sink());
    mount
        .unmount(false)
        .map_err(Error::of(PROCESS, "unmount"))?;
    detach.disarm();
    serving.join().map_err(Error::of(PROCESS, "serve"))?;
    let counts = fs.counts();
    let served = Served {
        readers: options.readers.get(),
        granted: format!("{:?}", negotiated.granted),
        missing: format!("{:?}", negotiated.missing),
        max_pages: negotiated.reply.max_pages,
        pipe_size: fs.pipe_size(),
        splice_bytes: counts.splice_bytes,
        pread_bytes: counts.pread_bytes,
        transport,
        uring_registered_bytes: memory.map(|m| m.bytes),
        uring_depth: memory.map(|m| m.depth),
        uring_queues: memory.map(|m| m.queues),
    };
    // A `Served` always encodes: it holds numbers and strings alone.
    let line = serde_json::to_string(&served).unwrap_or_default();
    writeln!(output, "{line}")
        .and_then(|()| output.flush())
        .map_err(Error::of(PROCESS, "write"))
}

#[cfg(test)]
#[path = "tsnull_tests.rs"]
mod tests;
