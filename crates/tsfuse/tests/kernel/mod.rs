//! The raw harness of the kernel checks of `docs/design/tsfuse-kernel-findings.md` (row T-F0 of
//! `docs/design/fetch-cache-serve.md`): a mount made with `tsfuse::mount`, INIT answered by hand
//! with the flags a check asks for, and requests read and answered one at a time on a chosen
//! descriptor, so that a check decides which clone reads each request and which clone each reply
//! is written to. The served tree is tiny and read-only: the root directory and two files, `f`
//! and `g`, of [`FILE_SIZE`] bytes whose content is [`byte`].
//!
//! A check runs where a FUSE mount can be made: as root through `mount(2)` (the farm), or through
//! `fusermount3` (CI installs `fuse3`). Elsewhere [`Conn::mount`] says why on stderr and the
//! check does not run. Every check blocks only on kernel events (a request read, a reply
//! written, a caller's system call returning), never on a timer; [`Conn`]'s drop aborts the
//! connection, so a failed check ends every blocked caller with `ECONNABORTED` instead of hanging.
#![allow(dead_code, reason = "each test binary uses a part of the helpers")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tsfuse::abi::{
    Attr, AttrOut, EntryOut, InHeader, InitIn, InitOut, InterruptIn, OpenOut, OutHeader, ReadIn,
    StatfsOut,
};
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{DevFd, ReplyDevice};

/// The opcodes the checks see (`enum fuse_opcode` of the pinned `abi/fuse.h`).
pub mod op {
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const ACCESS: u32 = 34;
    pub const INTERRUPT: u32 = 36;
    pub const BATCH_FORGET: u32 = 42;
}

/// INIT flags (`FUSE_*` of the pinned `abi/fuse.h`), as one 64-bit set (`flags2` above bit 31).
pub mod flag {
    pub const ASYNC_READ: u64 = 1 << 0;
    pub const SPLICE_WRITE: u64 = 1 << 7;
    pub const SPLICE_MOVE: u64 = 1 << 8;
    pub const NO_OPEN_SUPPORT: u64 = 1 << 17;
    pub const PARALLEL_DIROPS: u64 = 1 << 18;
    pub const MAX_PAGES: u64 = 1 << 22;
    pub const INIT_EXT: u64 = 1 << 30;
    pub const HAS_RESEND: u64 = 1 << 39;
    pub const OVER_IO_URING: u64 = 1 << 41;
}

/// `FOPEN_DIRECT_IO`.
pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
/// `FOPEN_KEEP_CACHE`.
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
/// `FUSE_NOTIFY_RESEND`, the notify code of a resend (`enum fuse_notify_code`).
pub const NOTIFY_RESEND: i32 = 7;
/// `FUSE_UNIQUE_RESEND`: the bit the kernel sets in the unique of a resent request.
pub const UNIQUE_RESEND: u64 = 1 << 63;
/// `FUSE_INT_REQ_BIT` (`fs/fuse/fuse_dev_i.h`): the bit of an INTERRUPT's own unique.
pub const INT_REQ_BIT: u64 = 1;

/// errno values the checks compare with.
pub mod errno {
    pub const ENOENT: i32 = 2;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const EAGAIN: i32 = 11;
    pub const EPERM: i32 = 1;
    pub const EINVAL: i32 = 22;
    pub const ENOSYS: i32 = 38;
    pub const ECONNABORTED: i32 = 103;
}

/// The root's inode.
pub const ROOT: u64 = 1;
/// The size of each file: four 1 MiB cells.
pub const FILE_SIZE: u64 = 4 << 20;
/// The files of the tree: name and inode.
pub const FILES: [(&str, u64); 2] = [("f", 2), ("g", 3)];
/// How long names and attributes stay valid: long, so that a check sees no LOOKUP or GETATTR
/// it did not cause.
pub const VALID: Duration = Duration::from_secs(3600);
/// The read buffer: a request is at most `max_write` (here [`MAX_WRITE`]) plus its header.
const READ_BUFFER: usize = (MAX_WRITE as usize) + 4096;
/// The `max_write` every check answers INIT with.
pub const MAX_WRITE: u32 = 128 * 1024;

/// The byte at `offset` of the file with inode `ino`.
#[must_use]
pub fn byte(ino: u64, offset: u64) -> u8 {
    ((offset % 251) as u8) ^ ((ino as u8).wrapping_mul(37))
}

/// `len` bytes of the file `ino` from `offset`, cut at its end.
#[must_use]
pub fn content(ino: u64, offset: u64, len: u64) -> Vec<u8> {
    let end = (offset + len).min(FILE_SIZE);
    (offset.min(end)..end).map(|o| byte(ino, o)).collect()
}

/// The inode of the file named `name`.
#[must_use]
pub fn ino_of(name: &str) -> Option<u64> {
    FILES.iter().find(|(n, _)| *n == name).map(|(_, i)| *i)
}

/// What a check asks for at INIT.
#[derive(Debug, Clone, Copy)]
pub struct Setup {
    /// The flags asked for; the connection gets those the kernel offered.
    pub want: u64,
    /// `max_pages` (sent with `MAX_PAGES`).
    pub max_pages: u16,
    /// `max_readahead`, bytes.
    pub max_readahead: u32,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            want: flag::INIT_EXT | flag::MAX_PAGES,
            max_pages: 256,
            max_readahead: 128 * 1024,
        }
    }
}

/// One request as read: its header and the bytes after it.
#[derive(Debug, Clone)]
pub struct Req {
    pub h: InHeader,
    pub body: Vec<u8>,
}

impl Req {
    /// The arguments of a READ.
    #[must_use]
    pub fn read_in(&self) -> ReadIn {
        ReadIn::read_prefix(&self.body).unwrap().0
    }

    /// The unique an INTERRUPT names.
    #[must_use]
    pub fn interrupted(&self) -> u64 {
        InterruptIn::read_prefix(&self.body).unwrap().0.unique
    }

    /// The name a LOOKUP asks for.
    #[must_use]
    pub fn name(&self) -> String {
        let end = self
            .body
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(self.body.len());
        String::from_utf8_lossy(&self.body[..end]).into_owned()
    }
}

/// Reads one request from `dev` (one `read(2)`, one request).
///
/// # Errors
///
/// The `read(2)` error: `ENODEV` once the file system is unmounted, `ECONNABORTED` once the
/// connection is aborted, `EAGAIN` on an empty non-blocking descriptor.
pub fn read_req(dev: impl AsFd) -> io::Result<Req> {
    let mut buf = vec![0u8; READ_BUFFER];
    let n = rustix::io::read(dev, &mut buf)?;
    let (h, rest) = InHeader::read_prefix(&buf[..n]).expect("a request starts with its header");
    Ok(Req {
        h,
        body: rest.to_vec(),
    })
}

/// Writes the reply `error` (0 or a negated errno) and `parts` to request `unique` on `dev`, as
/// one `writev(2)` with a correct length.
///
/// # Errors
///
/// The `writev(2)` error, which is the kernel's verdict on the reply.
pub fn reply(dev: &DevFd, unique: u64, error: i32, parts: &[&[u8]]) -> io::Result<()> {
    let len = OutHeader::SIZE + parts.iter().map(|p| p.len()).sum::<usize>();
    let oh = OutHeader {
        len: u32::try_from(len).unwrap(),
        error,
        unique,
    };
    let mut all: Vec<&[u8]> = vec![oh.as_bytes()];
    all.extend_from_slice(parts);
    dev.send(&all)
}

/// The errno of a failed reply, write or system call.
#[must_use]
pub fn errno_of(e: &io::Error) -> Option<i32> {
    e.raw_os_error()
}

/// The attributes of node `ino`: the root directory or a file.
#[must_use]
pub fn attr(ino: u64) -> Attr {
    let (mode, size, nlink) = match ino {
        ROOT => (0o40_555, 0, 2),
        _ => (0o100_444, FILE_SIZE, 1),
    };
    Attr {
        ino,
        size,
        blocks: size.div_ceil(512),
        mtime: 1_000_000,
        ctime: 1_000_000,
        atime: 1_000_000,
        mode,
        nlink,
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        blksize: 4096,
        ..Attr::default()
    }
}

/// How the tree answers OPEN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Open {
    /// Success with these `FOPEN_*` flags.
    Flags(u32),
    /// `ENOSYS`, which under `NO_OPEN_SUPPORT` stops the kernel sending OPEN.
    Enosys,
}

/// Answers `req` as the tiny tree does, on `dev` (the descriptor that read it). FORGETs and
/// INTERRUPTs get no reply; an unknown opcode gets `ENOSYS`.
///
/// # Errors
///
/// The reply's error.
pub fn answer(dev: &DevFd, req: &Req, open: Open) -> io::Result<()> {
    let u = req.h.unique;
    match req.h.opcode {
        op::FORGET | op::BATCH_FORGET | op::INTERRUPT => Ok(()),
        op::LOOKUP => match (req.h.nodeid, ino_of(&req.name())) {
            (ROOT, Some(ino)) => {
                let entry = EntryOut::new(ino, 1, VALID, VALID, attr(ino));
                reply(dev, u, 0, &[entry.as_bytes()])
            }
            _ => reply(dev, u, -errno::ENOENT, &[]),
        },
        op::GETATTR => {
            let out = AttrOut::new(VALID, attr(req.h.nodeid));
            reply(dev, u, 0, &[out.as_bytes()])
        }
        op::OPEN => match open {
            Open::Flags(open_flags) => {
                let out = OpenOut {
                    fh: req.h.nodeid,
                    open_flags,
                    backing_id: 0,
                };
                reply(dev, u, 0, &[out.as_bytes()])
            }
            Open::Enosys => reply(dev, u, -errno::ENOSYS, &[]),
        },
        op::OPENDIR => reply(dev, u, 0, &[OpenOut::default().as_bytes()]),
        op::READ => {
            let r = req.read_in();
            let data = content(req.h.nodeid, r.offset, u64::from(r.size));
            reply(dev, u, 0, &[&data])
        }
        op::STATFS => reply(dev, u, 0, &[StatfsOut::default().as_bytes()]),
        op::READDIR | op::RELEASE | op::RELEASEDIR | op::FLUSH | op::ACCESS => {
            reply(dev, u, 0, &[])
        }
        _ => reply(dev, u, -errno::ENOSYS, &[]),
    }
}

/// Reads requests on `dev` and answers them until one satisfies `stop`, which it returns
/// unanswered (parked).
pub fn serve_until(dev: &DevFd, open: Open, stop: impl Fn(&Req) -> bool) -> Req {
    loop {
        let req = read_req(dev).unwrap();
        if stop(&req) {
            return req;
        }
        answer(dev, &req, open).unwrap();
    }
}

/// Whether `req` is a READ of the file `name`.
#[must_use]
pub fn is_read_of(req: &Req, name: &str) -> bool {
    req.h.opcode == op::READ && Some(req.h.nodeid) == ino_of(name)
}

/// What a [`Server`] saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// Requests per opcode.
    pub ops: BTreeMap<u32, usize>,
    /// Every READ: inode, offset, size.
    pub reads: Vec<(u64, u64, u32)>,
    /// Replies the kernel refused, with the opcode and errno.
    pub refused: Vec<(u32, Option<i32>)>,
}

impl Seen {
    /// Requests of `opcode`.
    #[must_use]
    pub fn count(&self, opcode: u32) -> usize {
        self.ops.get(&opcode).copied().unwrap_or(0)
    }
}

/// A thread answering every request of one descriptor as the tree does, until the connection
/// ends (unmount or abort), recording what it saw.
pub struct Server {
    seen: Arc<Mutex<Seen>>,
    handle: JoinHandle<()>,
}

impl Server {
    /// Starts serving on `dev`.
    #[must_use]
    pub fn spawn(dev: DevFd, open: Open) -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let record = seen.clone();
        let handle = std::thread::spawn(move || {
            while let Ok(req) = read_req(&dev) {
                {
                    let mut s = record.lock().unwrap();
                    *s.ops.entry(req.h.opcode).or_default() += 1;
                    if req.h.opcode == op::READ {
                        let r = req.read_in();
                        s.reads.push((req.h.nodeid, r.offset, r.size));
                    }
                }
                if let Err(e) = answer(&dev, &req, open) {
                    record
                        .lock()
                        .unwrap()
                        .refused
                        .push((req.h.opcode, errno_of(&e)));
                }
            }
        });
        Self { seen, handle }
    }

    /// What it has seen so far.
    #[must_use]
    pub fn seen(&self) -> Seen {
        self.seen.lock().unwrap().clone()
    }

    /// Whether its thread has ended.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.handle.is_finished()
    }
}

/// A mount of the tiny tree and its first descriptor, after INIT.
pub struct Conn {
    /// The descriptor the mount made; INIT was answered on it.
    pub dev: DevFd,
    /// The mount point.
    pub mp: PathBuf,
    /// The INIT request's flags.
    pub offered: u64,
    /// The flags answered: asked for and offered.
    pub granted: u64,
    /// The `max_readahead` the kernel offered.
    pub offered_readahead: u32,
    mount: Option<Mount>,
    _dir: TestDir,
}

/// Why this machine cannot make a FUSE mount as the user of the tests, if it cannot.
fn unavailable() -> Option<&'static str> {
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(tsfuse::DEV_FUSE)
        .is_ok();
    let root = rustix::process::geteuid().is_root();
    let helper = std::process::Command::new(tsfuse::mount::FUSERMOUNT)
        .arg("--version")
        .output()
        .is_ok();
    match (device, root || helper) {
        (false, _) => Some("/dev/fuse cannot be opened for reading and writing"),
        (true, false) => Some("not root and fusermount3 is not installed"),
        (true, true) => None,
    }
}

/// Says on stderr why a check does not run.
pub fn skip(why: &str) {
    let _ = writeln!(io::stderr(), "skipped: {why}");
}

impl Conn {
    /// Mounts the tree in a scratch directory named after `name` and answers INIT with
    /// `setup`. `None` (having said why on stderr) where no FUSE mount can be made.
    #[must_use]
    pub fn mount(name: &str, setup: &Setup) -> Option<Self> {
        if let Some(why) = unavailable() {
            skip(why);
            return None;
        }
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let mp = dir.path().join("mnt");
        std::fs::create_dir(&mp).unwrap();
        let options = MountOptions {
            allow_other: false,
            ..MountOptions::tree("tsfuse-kernel")
        };
        let (dev, mount) = Mounter::default().mount(&mp, &options).unwrap();
        let req = read_req(&dev).unwrap();
        assert_eq!(req.h.opcode, op::INIT, "the first request is INIT");
        let (init, _) = InitIn::read_prefix(&req.body).expect("INIT carries fuse_init_in");
        let high = match u64::from(init.flags) & flag::INIT_EXT {
            0 => 0,
            _ => u64::from(init.flags2) << 32,
        };
        let offered = u64::from(init.flags) | high;
        let granted = setup.want & offered;
        let out = InitOut {
            major: 7,
            minor: init.minor.min(tsfuse::abi::KERNEL_MINOR_VERSION),
            max_readahead: setup.max_readahead.min(init.max_readahead),
            flags: (granted & u64::from(u32::MAX)) as u32,
            max_background: 64,
            congestion_threshold: 48,
            max_write: MAX_WRITE,
            time_gran: 1,
            max_pages: setup.max_pages,
            flags2: (granted >> 32) as u32,
            ..InitOut::default()
        };
        reply(&dev, req.h.unique, 0, &[out.as_bytes()]).unwrap();
        Some(Self {
            dev,
            mp,
            offered,
            granted,
            offered_readahead: init.max_readahead,
            mount: Some(mount),
            _dir: dir,
        })
    }

    /// A new clone of the connection (`FUSE_DEV_IOC_CLONE`), with its own processing queue.
    #[must_use]
    pub fn clone_reader(&self) -> DevFd {
        self.dev.clone_reader().unwrap()
    }

    /// The path of `name` in the mount.
    #[must_use]
    pub fn path(&self, name: &str) -> PathBuf {
        self.mp.join(name)
    }

    /// The mount, for its connection id and abort.
    #[must_use]
    pub fn the_mount(&self) -> &Mount {
        self.mount.as_ref().unwrap()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(mount) = self.mount.take() {
            // Abort first: a caller still blocked on a request ends with ECONNABORTED, so the
            // unmount (and a failed check) never waits on it.
            let _ = mount.abort();
            let _ = mount.unmount(true);
        }
    }
}

/// Another descriptor of the same open device (`dup(2)`): the same processing queue, as a
/// supervisor holds it.
#[must_use]
pub fn dup(dev: &DevFd) -> DevFd {
    DevFd::from_fd(dev.try_clone_fd().unwrap())
}

/// Puts the open device in non-blocking mode (shared with its dups, not with clones).
pub fn set_nonblocking(dev: &DevFd) {
    let flags = rustix::fs::fcntl_getfl(dev).unwrap();
    rustix::fs::fcntl_setfl(dev, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
}

/// A page-aligned buffer of `len` bytes (a direct READ is split by pages of the caller's
/// buffer, so an unaligned one would shift every split).
pub struct Aligned {
    buf: Vec<u8>,
    start: usize,
    len: usize,
}

impl Aligned {
    #[must_use]
    pub fn new(len: usize) -> Self {
        let buf = vec![0u8; len + 4096];
        let start = buf.as_ptr().align_offset(4096);
        Self { buf, start, len }
    }

    pub fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.start..self.start + self.len]
    }
}

/// Runs `f` on a new thread: a caller of the mount, whose system calls block until the
/// requests they make are answered.
pub fn caller<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> JoinHandle<T> {
    std::thread::spawn(f)
}

/// Reads `len` bytes of `path` at `offset` with one `pread(2)`.
pub fn pread(path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt as _;
    let file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, offset)?;
    buf.truncate(n);
    Ok(buf)
}
