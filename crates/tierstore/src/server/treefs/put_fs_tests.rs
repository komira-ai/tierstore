//! The put directory on `tsfuse` against the handler on fuser (section 18, row T-F9: "put-directory
//! opcodes against the current handler"). Two put directories are made alike (the same settings,
//! a test clock at the same time, a fake disk and a fake upload socket each); one is served by the
//! fuser frontend of `tierstore fs` (`fuse.rs` with `fuse_put.rs`), the other by a `tsfuse`
//! `Session` serving [`PutFs`], each over a socket pair that stands in for `/dev/fuse`. Every
//! request of the script is the same bytes to both, and each reply (its error, its unique and
//! every byte of its body) must be the same, and the same as the script's golden file in
//! `put_fs_golden/` (one line per exchange: the request's opcode and node id, the reply's error,
//! unique and body in hex). The script runs every opcode of the put directory, and each of its
//! refusals.
//!
//! The direct tests below call [`PutFs::handle`] with a replier of a recording device, for what
//! no reply shows: whether a request was answered inline or parked, which requests the put
//! directory owns, and the answers to what the session never hands a file system.
//!
//! Catches:
//! - a reply of `PutFs` that differs from fuser's in any byte: attributes (mode bits, link count,
//!   times, blocks, block size), validity, an absent name's entry, the open of a create
//!   (`FOPEN_DIRECT_IO` and its handle), a listing's records and where a short buffer cuts it,
//!   `statfs`, a write's count, or any errno;
//! - an opcode the handler on fuser answers that `tsfuse` answers `ENOSYS` (RENAME2, SYMLINK,
//!   MKNOD, RMDIR, LINK, SETXATTR, REMOVEXATTR), or an extended attribute read that is not;
//! - a FORGET that does not reach the put directory, so that a directory `mkdir` made is never
//!   let go;
//! - a rename flag other than `RENAME_NOREPLACE` taken, or a name that is not UTF-8 taken;
//! - a commit that waits answered as inline (the reader would have waited for the upload), or a
//!   rename answered inline reported parked;
//! - a request of the trees taken by the put directory, or one of the put directory left out.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tsfuse::abi::{
    BatchForgetIn, CreateIn, FlushIn, ForgetIn, ForgetOne, FsyncIn, GetattrIn, InHeader,
    InitConfig, InitFlags, InitIn, LinkIn, MkdirIn, MknodIn, OpenIn, ReadIn, ReleaseIn, Rename2In,
    RenameIn, SetattrIn, WriteIn, fattr,
};
use tsfuse::{DevFd, Fs, Handled, ReplyDevice, Request, Session, Tokens};

use super::PutFs;
use crate::server::local_cache::testing::Quirk;
use crate::server::treefs::fuse::{Frontend, config};
use crate::server::treefs::ino;
use crate::server::treefs::put::testing::SpaceRig;
use crate::server::treefs::testing::{Rig, sample};

/// Opcodes of the uapi `fuse.h` the script sends.
mod op {
    pub(super) const LOOKUP: u32 = 1;
    pub(super) const FORGET: u32 = 2;
    pub(super) const GETATTR: u32 = 3;
    pub(super) const SETATTR: u32 = 4;
    pub(super) const READLINK: u32 = 5;
    pub(super) const SYMLINK: u32 = 6;
    pub(super) const MKNOD: u32 = 8;
    pub(super) const MKDIR: u32 = 9;
    pub(super) const UNLINK: u32 = 10;
    pub(super) const RMDIR: u32 = 11;
    pub(super) const RENAME: u32 = 12;
    pub(super) const LINK: u32 = 13;
    pub(super) const OPEN: u32 = 14;
    pub(super) const READ: u32 = 15;
    pub(super) const WRITE: u32 = 16;
    pub(super) const STATFS: u32 = 17;
    pub(super) const RELEASE: u32 = 18;
    pub(super) const FSYNC: u32 = 20;
    pub(super) const SETXATTR: u32 = 21;
    pub(super) const GETXATTR: u32 = 22;
    pub(super) const LISTXATTR: u32 = 23;
    pub(super) const REMOVEXATTR: u32 = 24;
    pub(super) const FLUSH: u32 = 25;
    pub(super) const INIT: u32 = 26;
    pub(super) const OPENDIR: u32 = 27;
    pub(super) const READDIR: u32 = 28;
    pub(super) const RELEASEDIR: u32 = 29;
    pub(super) const CREATE: u32 = 35;
    pub(super) const DESTROY: u32 = 38;
    pub(super) const BATCH_FORGET: u32 = 42;
    pub(super) const READDIRPLUS: u32 = 44;
    pub(super) const RENAME2: u32 = 45;
    pub(super) const LSEEK: u32 = 46;
}

/// Errnos the script expects, as the kernel's end reads them (negated).
mod errno {
    pub(super) const EPERM: i32 = -1;
    pub(super) const ENOENT: i32 = -2;
    pub(super) const EIO: i32 = -5;
    pub(super) const EBADF: i32 = -9;
    pub(super) const EACCES: i32 = -13;
    pub(super) const EEXIST: i32 = -17;
    pub(super) const EISDIR: i32 = -21;
    pub(super) const EINVAL: i32 = -22;
    pub(super) const ENOSYS: i32 = -38;
    pub(super) const ESTALE: i32 = -116;
}

/// `RENAME_NOREPLACE` and `RENAME_EXCHANGE`.
const NOREPLACE: u32 = 1;
const EXCHANGE: u32 = 2;

/// The bytes of a request of `opcode` on `nodeid`, from the test's user.
fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let header = InHeader {
        len: u32::try_from(InHeader::SIZE + body.len()).unwrap(),
        opcode,
        unique,
        nodeid,
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        pid: 1,
        total_extlen: 0,
        padding: 0,
    };
    [header.as_bytes(), body].concat()
}

/// A name and its NUL.
fn name(name: &[u8]) -> Vec<u8> {
    [name, b"\0"].concat()
}

/// A reply as the kernel's end reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    error: i32,
    unique: u64,
    body: Vec<u8>,
}

impl Reply {
    /// The node id of an entry reply (LOOKUP, MKDIR, CREATE).
    fn nodeid(&self) -> u64 {
        u64::from_ne_bytes(self.body[..8].try_into().unwrap())
    }

    /// The file handle of a CREATE reply: the open after the entry.
    fn fh(&self) -> u64 {
        u64::from_ne_bytes(self.body[128..136].try_into().unwrap())
    }
}

/// The kernel's end of a socket pair that stands in for `/dev/fuse`: one request per write, one
/// reply per read.
struct Kernel(UnixDatagram);

impl Kernel {
    /// The device end for a server, and the kernel's end for the test.
    fn pair() -> (OwnedFd, Self) {
        let (device, kernel) = UnixDatagram::pair().unwrap();
        // A reply that never comes fails the test instead of hanging it (no time is asserted).
        kernel
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        (OwnedFd::from(device), Self(kernel))
    }

    fn send(&self, bytes: &[u8]) {
        assert_eq!(self.0.send(bytes).unwrap(), bytes.len());
    }

    fn reply(&self) -> Reply {
        let mut bytes = vec![0u8; 1 << 20];
        let got = self.0.recv(&mut bytes).unwrap();
        let len = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, got, "the header's length is the message's");
        Reply {
            error: i32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            unique: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            body: bytes[16..got].to_vec(),
        }
    }
}

/// An INIT of a 7.40 kernel that offers `INIT_EXT` alone.
fn init() -> Vec<u8> {
    let init = InitIn {
        major: 7,
        minor: 40,
        max_readahead: 131_072,
        flags: u32::try_from(InitFlags::INIT_EXT.0).unwrap(),
        ..InitIn::default()
    };
    request(op::INIT, 1, 0, init.as_bytes())
}

/// The two put directories, served, and the kernel's end of each.
struct Pair<'a> {
    fuser: &'a Kernel,
    tsfuse: &'a Kernel,
    /// The disks of the two stages, fuser's first.
    disks: [&'a crate::server::local_cache::testing::FakeDisk; 2],
    unique: std::cell::Cell<u64>,
    /// Every exchange answered so far, a line each (see [`said`]).
    said: std::cell::RefCell<Vec<String>>,
}

/// One exchange as a line of a golden file: the request's opcode and node id, then the reply's
/// error, unique and every byte of its body, in hex.
fn said(opcode: u32, nodeid: u64, reply: &Reply) -> String {
    let body: String = reply.body.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{opcode} {nodeid:#x} {} {} {body}",
        reply.error, reply.unique
    )
}

impl Pair<'_> {
    /// Sends `opcode` on `nodeid` with `body` to both and returns the reply, after asserting that
    /// both replies are the same, byte for byte.
    fn both(&self, opcode: u32, nodeid: u64, body: &[u8]) -> Reply {
        let unique = self.unique.get() + 1;
        self.unique.set(unique);
        let bytes = request(opcode, unique, nodeid, body);
        self.fuser.send(&bytes);
        self.tsfuse.send(&bytes);
        let (fuser, tsfuse) = (self.fuser.reply(), self.tsfuse.reply());
        assert_eq!(
            tsfuse, fuser,
            "opcode {opcode} on node {nodeid:#x}: tsfuse's reply, then fuser's"
        );
        assert_eq!(fuser.unique, unique);
        self.said.borrow_mut().push(said(opcode, nodeid, &fuser));
        fuser
    }

    /// Sends a request that takes no reply to both.
    fn unanswered(&self, opcode: u32, nodeid: u64, body: &[u8]) {
        let unique = self.unique.get() + 1;
        self.unique.set(unique);
        let bytes = request(opcode, unique, nodeid, body);
        self.fuser.send(&bytes);
        self.tsfuse.send(&bytes);
    }

    /// The error of the reply to `opcode`, the same on both.
    fn error(&self, opcode: u32, nodeid: u64, body: &[u8]) -> i32 {
        self.both(opcode, nodeid, body).error
    }

    /// LOOKUP of `name` in `parent`.
    fn lookup(&self, parent: u64, child: &[u8]) -> Reply {
        self.both(op::LOOKUP, parent, &name(child))
    }

    /// The node id of `path` below `put/`, looked up name by name.
    fn walk(&self, path: &str) -> u64 {
        path.split('/').fold(ino::PUT, |parent, child| {
            let looked = self.lookup(parent, child.as_bytes());
            assert_eq!(looked.error, 0, "{child} of {path}");
            looked.nodeid()
        })
    }
}

/// Serves two put directories alike, one on fuser and one on `tsfuse`, and runs `script` over
/// them; then ends both sessions with a DESTROY, whose replies are compared too.
fn compare(name: &str, script: impl FnOnce(&Pair<'_>)) {
    let tree = Rig::new(&format!("{name}-tree"), sample());
    let fuser_rig = SpaceRig::new(&format!("{name}-fuser"));
    let tsfuse_rig = SpaceRig::new(&format!("{name}-tsfuse"));
    tree.core.serve_put(Arc::new(fuser_rig.space));

    let (device, fuser) = Kernel::pair();
    fuser.send(&init());
    let settings = config(1, false);
    let session = fuser::Session::from_fd(
        Frontend::new(tree.core.clone()),
        device,
        settings.acl,
        settings,
    )
    .unwrap();
    assert_eq!(fuser.reply().error, 0);
    let fuser_session = session.spawn().unwrap();

    let (device, tsfuse) = Kernel::pair();
    tsfuse.send(&init());
    let session = Session::init(DevFd::from_fd(device), &InitConfig::put()).unwrap();
    assert_eq!(tsfuse.reply().error, 0);
    let serving = session
        .serve(Arc::new(PutFs::new(Arc::new(tsfuse_rig.space))))
        .unwrap();

    let pair = Pair {
        fuser: &fuser,
        tsfuse: &tsfuse,
        disks: [&fuser_rig.rig.disk, &tsfuse_rig.rig.disk],
        unique: std::cell::Cell::new(1),
        said: std::cell::RefCell::new(Vec::new()),
    };
    for disk in pair.disks {
        disk.set_space(1 << 30, 3 << 27);
    }
    script(&pair);

    let ended = pair.both(op::DESTROY, 0, &[]);
    assert_eq!((ended.error, ended.body.len()), (0, 0));
    fuser_session.join().unwrap();
    let _ = serving.join().unwrap();
    let said = pair.said.into_inner();
    let golden: Vec<&str> = golden(name).lines().collect();
    for (at, (said, golden)) in said.iter().zip(&golden).enumerate() {
        assert_eq!(
            said,
            golden,
            "{name}: exchange {} of the golden file",
            at + 1
        );
    }
    assert_eq!(
        said.len(),
        golden.len(),
        "{name}: the exchanges of the golden file"
    );
}

/// The golden file of a script: what the handler on fuser answered to each of its exchanges,
/// recorded while both handlers answered alike (`put_fs_golden/`).
fn golden(name: &str) -> &'static str {
    match name {
        "putfs-names" => include_str!("put_fs_golden/putfs-names.txt"),
        "putfs-commit" => include_str!("put_fs_golden/putfs-commit.txt"),
        "putfs-refused" => include_str!("put_fs_golden/putfs-refused.txt"),
        "putfs-mkdir" => include_str!("put_fs_golden/putfs-mkdir.txt"),
        other => panic!("no golden file for {other}"),
    }
}

fn create_in() -> Vec<u8> {
    CreateIn {
        flags: 0o1, // O_WRONLY
        mode: 0o100_644,
        umask: 0o022,
        open_flags: 0,
    }
    .as_bytes()
    .to_vec()
}

fn create(child: &[u8]) -> Vec<u8> {
    [create_in(), name(child)].concat()
}

fn write(fh: u64, offset: u64, data: &[u8]) -> Vec<u8> {
    let arg = WriteIn {
        fh,
        offset,
        size: u32::try_from(data.len()).unwrap(),
        write_flags: 0,
        lock_owner: 0,
        flags: 0o1,
        padding: 0,
    };
    [arg.as_bytes(), data].concat()
}

fn release(fh: u64) -> Vec<u8> {
    ReleaseIn {
        fh,
        flags: 0o1,
        release_flags: 0,
        lock_owner: 0,
    }
    .as_bytes()
    .to_vec()
}

fn read_in(fh: u64, offset: u64, size: u32) -> Vec<u8> {
    ReadIn {
        fh,
        offset,
        size,
        read_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    }
    .as_bytes()
    .to_vec()
}

fn setattr(valid: u32, change: impl FnOnce(&mut SetattrIn)) -> Vec<u8> {
    let mut arg = SetattrIn {
        valid,
        ..SetattrIn::default()
    };
    change(&mut arg);
    arg.as_bytes().to_vec()
}

fn rename(newdir: u64, old: &[u8], new: &[u8]) -> Vec<u8> {
    [RenameIn { newdir }.as_bytes(), &name(old), &name(new)].concat()
}

fn rename2(newdir: u64, flags: u32, old: &[u8], new: &[u8]) -> Vec<u8> {
    let arg = Rename2In {
        newdir,
        flags,
        padding: 0,
    };
    [arg.as_bytes(), &name(old), &name(new)].concat()
}

fn mkdir(child: &[u8]) -> Vec<u8> {
    let arg = MkdirIn {
        mode: 0o755,
        umask: 0o022,
    };
    [arg.as_bytes(), &name(child)].concat()
}

const GETATTR: [u8; GetattrIn::SIZE] = [0; GetattrIn::SIZE];
const OPEN_RDONLY: [u8; OpenIn::SIZE] = [0; OpenIn::SIZE];

/// The put directory's namespace and listings: `put/` from the root, the directories that lead to
/// the prefix (shown, never listed), the prefix's directories, a name that is not there and one
/// that is not UTF-8, a listing whole, from a position, past its end and cut by a short buffer,
/// with and without attributes, and `statfs` (and its failure).
#[test]
fn the_namespace_and_listings_are_answered_as_on_fuser() {
    compare("putfs-names", |p| {
        let put = p.lookup(ino::ROOT, b"put");
        assert_eq!((put.error, put.nodeid()), (0, ino::PUT));
        assert_eq!(p.error(op::GETATTR, ino::PUT, &GETATTR), 0);
        let cas = p.walk("cas");
        let nonce = p.walk("cas/b/build-1/nonce-1");
        // A name that leads nowhere: the absent entry (node 0).
        let other = p.lookup(ino::PUT, b"other");
        assert_eq!((other.error, other.nodeid()), (0, 0));
        assert_eq!(p.lookup(ino::PUT, b"\xff").error, errno::EINVAL);
        assert_eq!(p.lookup(nonce, b"a/b").error, errno::EINVAL);

        // `put/` and the leading directories are never listed; the prefix's are.
        assert_eq!(p.error(op::OPENDIR, ino::PUT, &OPEN_RDONLY), errno::EACCES);
        assert_eq!(p.error(op::OPENDIR, cas, &OPEN_RDONLY), errno::EACCES);
        assert_eq!(
            p.error(op::READDIR, cas, &read_in(0, 0, 4096)),
            errno::EACCES
        );
        assert_eq!(p.error(op::OPENDIR, nonce, &OPEN_RDONLY), 0);

        // A file in the directory, so that a listing has more than `.` and `..`.
        let made = p.both(op::CREATE, nonce, &create(b"a.part"));
        assert_eq!(made.error, 0);
        assert_eq!(p.error(op::RELEASE, made.nodeid(), &release(made.fh())), 0);
        for opcode in [op::READDIR, op::READDIRPLUS] {
            let whole = p.both(opcode, nonce, &read_in(0, 0, 4096));
            assert_eq!(whole.error, 0);
            assert!(!whole.body.is_empty());
            // From the second entry, past the end, a buffer that holds one record (a dirent of
            // `.` is 32 bytes, with its entry 160) and a buffer one record cannot fill.
            assert!(p.both(opcode, nonce, &read_in(0, 1, 4096)).body.len() < whole.body.len());
            assert!(p.both(opcode, nonce, &read_in(0, 9, 4096)).body.is_empty());
            let (size, record) = if opcode == op::READDIRPLUS {
                (200, 160)
            } else {
                (40, 32)
            };
            assert_eq!(
                p.both(opcode, nonce, &read_in(0, 0, size)).body.len(),
                record
            );
            assert!(p.both(opcode, nonce, &read_in(0, 0, 8)).body.is_empty());
        }
        assert_eq!(p.error(op::RELEASEDIR, nonce, &release(0)), 0);

        let statfs = p.both(op::STATFS, nonce, &[]);
        assert_eq!((statfs.error, statfs.body.len()), (0, 80));
        for disk in p.disks {
            disk.set(Quirk::SpaceFails);
        }
        assert_eq!(p.error(op::STATFS, nonce, &[]), errno::EIO);
        for disk in p.disks {
            disk.set(Quirk::None);
        }

        // A file that is gone, and a directory this process was never told of.
        assert_eq!(
            p.error(op::UNLINK, nonce, &name(b"a.part")),
            0,
            "the abort of a staged file"
        );
        assert_eq!(p.error(op::GETATTR, made.nodeid(), &GETATTR), errno::ENOENT);
        assert_eq!(
            p.error(op::GETATTR, ino::put_dir(0x1234), &GETATTR),
            errno::ESTALE
        );
    });
}

/// A staged file's life: the create (and the names it refuses), writes (and a handle that is not
/// one), its attributes, the changes of attributes taken (none) and refused, the opens and reads
/// refused, flush and fsync, renames of the `.part` name with and without `RENAME_NOREPLACE`, the
/// flags refused, the commit (answered from the commit's thread on `tsfuse`), what is refused of a
/// committed name, an unlink that aborts, and a rename across directories.
#[test]
fn a_staged_file_is_created_written_and_committed_as_on_fuser() {
    compare("putfs-commit", |p| {
        let cas = p.walk("cas");
        let nonce = p.walk("cas/b/build-1/nonce-1");
        let other = p.walk("cas/b/build-1/nonce-2");
        // Names the create refuses: a final name, white space, not UTF-8, outside a prefix.
        assert_eq!(p.error(op::CREATE, nonce, &create(b"pack")), errno::EPERM);
        assert_eq!(
            p.error(op::CREATE, nonce, &create(b"a b.part")),
            errno::EINVAL
        );
        assert_eq!(
            p.error(op::CREATE, nonce, &create(b"\xff.part")),
            errno::EINVAL
        );
        assert_eq!(p.error(op::CREATE, cas, &create(b"x.part")), errno::EPERM);

        let made = p.both(op::CREATE, nonce, &create(b"a.part"));
        assert_eq!(made.error, 0);
        let (file, fh) = (made.nodeid(), made.fh());
        let written = p.both(op::WRITE, file, &write(fh, 0, b"hello"));
        assert_eq!((written.error, written.body.len()), (0, 8));
        assert_eq!(p.error(op::WRITE, file, &write(fh, 5, b" world")), 0);
        assert_eq!(
            p.error(op::WRITE, file, &write(fh ^ 0xff, 0, b"x")),
            errno::EBADF
        );
        assert_eq!(p.error(op::GETATTR, file, &GETATTR), 0);

        // Changes of attributes: taken only when they change nothing.
        let same_size = setattr(fattr::SIZE | fattr::FH, |a| {
            a.size = 11;
            a.fh = fh;
        });
        assert_eq!(p.error(op::SETATTR, file, &same_size), 0);
        let truncate = setattr(fattr::SIZE, |a| a.size = 9);
        assert_eq!(p.error(op::SETATTR, file, &truncate), errno::EPERM);
        let same_mode = setattr(fattr::MODE, |a| a.mode = 0o100_600);
        assert_eq!(p.error(op::SETATTR, file, &same_mode), 0);
        let set_id = setattr(fattr::MODE, |a| a.mode = 0o104_600);
        assert_eq!(p.error(op::SETATTR, file, &set_id), errno::EPERM);
        for valid in [
            fattr::UID,
            fattr::GID,
            fattr::ATIME,
            fattr::MTIME,
            fattr::CTIME,
        ] {
            assert_eq!(
                p.error(op::SETATTR, file, &setattr(valid, |_| ())),
                errno::EPERM
            );
        }
        // `FATTR_ATIME_NOW` and `FATTR_MTIME_NOW` name no time without `FATTR_ATIME`/`MTIME`.
        let now_alone = setattr(fattr::ATIME_NOW | fattr::MTIME_NOW, |_| ());
        assert_eq!(p.error(op::SETATTR, file, &now_alone), 0);
        assert_eq!(
            p.error(op::SETATTR, nonce, &setattr(fattr::SIZE, |_| ())),
            errno::EPERM
        );

        assert_eq!(p.error(op::OPEN, file, &OPEN_RDONLY), errno::EPERM);
        assert_eq!(p.error(op::OPEN, nonce, &OPEN_RDONLY), errno::EISDIR);
        assert_eq!(
            p.error(op::READ, file, &read_in(fh, 0, 4096)),
            errno::EACCES
        );
        let flush = FlushIn {
            fh,
            ..FlushIn::default()
        };
        assert_eq!(p.error(op::FLUSH, file, flush.as_bytes()), 0);
        let fsync = FsyncIn {
            fh,
            ..FsyncIn::default()
        };
        assert_eq!(p.error(op::FSYNC, file, fsync.as_bytes()), 0);

        // Renames of the `.part` name, and the flags refused.
        assert_eq!(
            p.error(
                op::RENAME2,
                nonce,
                &rename2(nonce, EXCHANGE, b"a.part", b"z.part")
            ),
            errno::EPERM
        );
        assert_eq!(
            p.error(
                op::RENAME2,
                nonce,
                &rename2(nonce, NOREPLACE, b"a.part", b"b.part")
            ),
            0
        );
        assert_eq!(
            p.error(op::RENAME, nonce, &rename(nonce, b"b.part", b"\xff")),
            errno::EINVAL
        );
        assert_eq!(
            p.error(op::RENAME, nonce, &rename(nonce, b"\xff", b"b")),
            errno::EINVAL
        );
        assert_eq!(
            p.error(op::RENAME, nonce, &rename(other, b"b.part", b"b.part")),
            errno::EPERM
        );

        // The commit, once the file is closed.
        assert_eq!(p.error(op::RELEASE, file, &release(fh)), 0);
        assert_eq!(
            p.error(op::RENAME, nonce, &rename(nonce, b"b.part", b"b")),
            0
        );
        let committed = p.lookup(nonce, b"b");
        assert_eq!((committed.error, committed.nodeid()), (0, file));
        assert_eq!(
            p.both(op::READDIRPLUS, nonce, &read_in(0, 0, 4096)).error,
            0
        );
        assert_eq!(p.error(op::OPEN, file, &OPEN_RDONLY), errno::EACCES);
        assert_eq!(
            p.error(op::RENAME, nonce, &rename(nonce, b"b", b"c")),
            errno::EPERM
        );
        assert_eq!(p.error(op::UNLINK, nonce, &name(b"b")), errno::EPERM);

        // A second staged file: `RENAME_NOREPLACE` onto the committed name, then the abort.
        let second = p.both(op::CREATE, nonce, &create(b"d.part"));
        assert_eq!(second.error, 0);
        assert_eq!(
            p.error(op::RELEASE, second.nodeid(), &release(second.fh())),
            0
        );
        assert_eq!(
            p.error(
                op::RENAME2,
                nonce,
                &rename2(nonce, NOREPLACE, b"d.part", b"b")
            ),
            errno::EEXIST
        );
        assert_eq!(p.error(op::UNLINK, nonce, &name(b"\xff")), errno::EINVAL);
        assert_eq!(p.error(op::UNLINK, nonce, &name(b"d.part")), 0);
        let gone = p.lookup(nonce, b"d.part");
        assert_eq!((gone.error, gone.nodeid()), (0, 0));
    });
}

/// What the put directory does not do, refused as on fuser: a symbolic link, a node, an `rmdir`,
/// a hard link, setting or removing an extended attribute (`EPERM`); reading the extended
/// attributes (`ENOSYS`, so that the kernel stops asking); a link's target (`EINVAL` of `put/`);
/// and an opcode neither implements.
#[test]
fn what_the_put_directory_does_not_do_is_refused_as_on_fuser() {
    compare("putfs-refused", |p| {
        let nonce = p.walk("cas/b/build-1/nonce-1");
        let made = p.both(op::CREATE, nonce, &create(b"a.part"));
        let file = made.nodeid();
        let symlink = [name(b"c.part"), name(b"a.part")].concat();
        assert_eq!(p.error(op::SYMLINK, nonce, &symlink), errno::EPERM);
        let mknod = MknodIn {
            mode: 0o010_600,
            rdev: 0,
            umask: 0,
            padding: 0,
        };
        let mknod = [mknod.as_bytes(), &name(b"d.part")].concat();
        assert_eq!(p.error(op::MKNOD, nonce, &mknod), errno::EPERM);
        assert_eq!(p.error(op::RMDIR, nonce, &name(b"sub")), errno::EPERM);
        let link = [LinkIn { oldnodeid: file }.as_bytes(), &name(b"b.part")].concat();
        assert_eq!(p.error(op::LINK, nonce, &link), errno::EPERM);
        // The compat argument of a SETXATTR without `FUSE_SETXATTR_EXT`: size and flags.
        let setxattr = [&[1, 0, 0, 0, 0, 0, 0, 0][..], &name(b"user.x"), b"v"].concat();
        assert_eq!(p.error(op::SETXATTR, file, &setxattr), errno::EPERM);
        assert_eq!(
            p.error(op::REMOVEXATTR, file, &name(b"user.x")),
            errno::EPERM
        );
        let getxattr = [&[0u8; 8][..], &name(b"user.x")].concat();
        assert_eq!(p.error(op::GETXATTR, file, &getxattr), errno::ENOSYS);
        assert_eq!(p.error(op::LISTXATTR, file, &[0; 8]), errno::ENOSYS);
        assert_eq!(p.error(op::READLINK, ino::PUT, &[]), errno::EINVAL);
        assert_eq!(p.error(op::LSEEK, file, &[0; 24]), errno::ENOSYS);
        assert_eq!(p.error(op::RELEASE, file, &release(made.fh())), 0);
    });
}

/// `mkdir` and FORGET: a directory made where free names are not there, the names it refuses, and
/// a made directory let go once the kernel forgets every lookup of it, by FORGET or by
/// BATCH_FORGET, and kept while it holds one.
#[test]
fn made_directories_are_forgotten_as_on_fuser() {
    compare("putfs-mkdir", |p| {
        let cas = p.walk("cas");
        let nonce = p.walk("cas/b/build-1/nonce-1");
        let staged = p.both(op::CREATE, nonce, &create(b"a.part"));
        assert_eq!(
            p.error(op::RELEASE, staged.nodeid(), &release(staged.fh())),
            0
        );
        // With a file in it, a free name in the directory is not there: `mkdir` makes it.
        let sub = p.both(op::MKDIR, nonce, &mkdir(b"sub"));
        assert_eq!(sub.error, 0);
        let kept = p.both(op::MKDIR, nonce, &mkdir(b"kept"));
        assert_eq!(kept.error, 0);
        let third = p.both(op::MKDIR, nonce, &mkdir(b"third"));
        assert_eq!(third.error, 0);
        assert_eq!(p.error(op::MKDIR, nonce, &mkdir(b"sub")), errno::EEXIST);
        assert_eq!(p.error(op::MKDIR, nonce, &mkdir(b"x.part")), errno::EPERM);
        assert_eq!(p.error(op::MKDIR, nonce, &mkdir(b"a.part")), errno::EPERM);
        assert_eq!(p.error(op::MKDIR, cas, &mkdir(b"zzz")), errno::EPERM);
        assert_eq!(p.error(op::MKDIR, nonce, &mkdir(b"\xff")), errno::EINVAL);
        // A second lookup of `kept`: two lookups, of which one is forgotten.
        assert_eq!(p.lookup(nonce, b"kept").nodeid(), kept.nodeid());

        p.unanswered(op::FORGET, sub.nodeid(), ForgetIn { nlookup: 1 }.as_bytes());
        let batch = BatchForgetIn { count: 3, dummy: 0 };
        let nodes = [
            ForgetOne {
                nodeid: kept.nodeid(),
                nlookup: 1,
            },
            ForgetOne {
                nodeid: third.nodeid(),
                nlookup: 1,
            },
            // A node of the trees, which the put directory does not count.
            ForgetOne {
                nodeid: ino::TREES,
                nlookup: 1,
            },
        ];
        let body = [
            batch.as_bytes(),
            nodes[0].as_bytes(),
            nodes[1].as_bytes(),
            nodes[2].as_bytes(),
        ]
        .concat();
        p.unanswered(op::BATCH_FORGET, 0, &body);

        // Forgotten: not there (the directory holds a file, so a free name is not one).
        for gone in [b"sub".as_slice(), b"third"] {
            let looked = p.lookup(nonce, gone);
            assert_eq!((looked.error, looked.nodeid()), (0, 0));
        }
        let still = p.lookup(nonce, b"kept");
        assert_eq!((still.error, still.nodeid()), (0, kept.nodeid()));
    });
}

/// A device that records each reply written to it: `(error, unique, body)`.
#[derive(Default)]
struct Recorder(Mutex<Vec<(i32, u64, Vec<u8>)>>);

impl ReplyDevice for Recorder {
    fn send(&self, parts: &[&[u8]]) -> std::io::Result<()> {
        let bytes = parts.concat();
        let error = i32::from_ne_bytes(bytes[4..8].try_into().unwrap());
        let unique = u64::from_ne_bytes(bytes[8..16].try_into().unwrap());
        self.0
            .lock()
            .unwrap()
            .push((error, unique, bytes[16..].to_vec()));
        Ok(())
    }
}

/// Calls `fs.handle` with `bytes`, on a replier of `device`, and gives what it says.
fn handle(fs: &PutFs, device: &Arc<Recorder>, tokens: &Tokens, bytes: &[u8]) -> Handled {
    let req = Request::parse(bytes).unwrap();
    let replier = tokens.register(
        req.header.unique,
        Arc::clone(device) as Arc<dyn ReplyDevice>,
    );
    fs.handle(req, replier)
}

/// The replies `device` has had, taken.
fn taken(device: &Recorder) -> Vec<(i32, u64, Vec<u8>)> {
    std::mem::take(&mut *device.0.lock().unwrap())
}

/// Asserts which requests the put directory owns: those on its inodes, the lookup of `put` in
/// the root (and no other lookup there), and a LINK by the node linked; and that a request it does
/// not own is answered `ENOENT`.
#[test]
fn the_put_directory_owns_its_inodes_and_the_lookup_of_put() {
    let rig = SpaceRig::new("putfs-owns");
    let fs = PutFs::new(Arc::new(rig.space));
    let (device, tokens) = (Arc::new(Recorder::default()), Tokens::new());
    let owns = |opcode: u32, nodeid: u64, body: &[u8]| {
        PutFs::owns(&Request::parse(&request(opcode, 7, nodeid, body)).unwrap())
    };
    let file = ino::put_file(3);
    assert!(owns(op::GETATTR, ino::PUT, &GETATTR));
    assert!(owns(op::GETATTR, ino::put_dir(9), &GETATTR));
    assert!(owns(op::GETATTR, file, &GETATTR));
    assert!(owns(op::LOOKUP, ino::ROOT, &name(b"put")));
    assert!(!owns(op::LOOKUP, ino::ROOT, &name(b"trees")));
    assert!(!owns(op::GETATTR, ino::ROOT, &GETATTR));
    assert!(!owns(op::GETATTR, ino::TREES, &GETATTR));
    assert!(!owns(op::GETATTR, ino::of(1, 2), &GETATTR));
    let link = |old: u64| [LinkIn { oldnodeid: old }.as_bytes(), &name(b"x")].concat();
    // A LINK is the linked node's: into `put/` from a tree is the tree's, and the reverse is ours.
    assert!(owns(op::LINK, ino::PUT, &link(file)));
    assert!(!owns(op::LINK, ino::PUT, &link(ino::of(1, 2))));
    assert!(owns(op::LINK, ino::of(1, 0), &link(file)));

    let lookup = request(op::LOOKUP, 8, ino::ROOT, &name(b"trees"));
    assert_eq!(handle(&fs, &device, &tokens, &lookup), Handled::Done);
    let getattr = request(op::GETATTR, 9, ino::of(1, 2), &GETATTR);
    assert_eq!(handle(&fs, &device, &tokens, &getattr), Handled::Done);
    assert_eq!(
        taken(&device),
        [
            (errno::ENOENT, 8, Vec::new()),
            (errno::ENOENT, 9, Vec::new())
        ]
    );
}

/// Asserts that a body too short for its opcode is answered `EIO`, that what the session answers
/// itself (INIT, DESTROY, INTERRUPT, FORGET, BATCH_FORGET) is answered `ENOSYS` if a caller hands
/// it over anyway, that READLINK of a file is `EINVAL`, and that a FORGET of a node outside the
/// put directory changes nothing in it.
#[test]
fn a_malformed_or_session_request_is_answered_and_a_foreign_forget_ignored() {
    let rig = SpaceRig::new("putfs-direct");
    let fs = PutFs::new(Arc::new(rig.space));
    let (device, tokens) = (Arc::new(Recorder::default()), Tokens::new());
    let short = request(op::GETATTR, 2, ino::PUT, &[0; 3]);
    assert_eq!(handle(&fs, &device, &tokens, &short), Handled::Done);
    let session_ops: [(u32, &[u8]); 5] = [
        (op::INIT, &[0; 64]),
        (op::DESTROY, &[]),
        (36, &[0; 8]),
        (op::FORGET, &[0; 8]),
        (op::BATCH_FORGET, &[0; 8]),
    ];
    for (unique, (opcode, body)) in (3..).zip(session_ops) {
        let bytes = request(opcode, unique, ino::PUT, body);
        assert_eq!(handle(&fs, &device, &tokens, &bytes), Handled::Done);
    }
    let readlink = request(op::READLINK, 8, ino::put_file(1), &[]);
    assert_eq!(handle(&fs, &device, &tokens, &readlink), Handled::Done);
    let errors: Vec<(i32, u64)> = taken(&device)
        .into_iter()
        .map(|(error, unique, _)| (error, unique))
        .collect();
    assert_eq!(
        errors,
        [
            (errno::EIO, 2),
            (errno::ENOSYS, 3),
            (errno::ENOSYS, 4),
            (errno::ENOSYS, 5),
            (errno::ENOSYS, 6),
            (errno::ENOSYS, 7),
            (errno::EINVAL, 8),
        ]
    );

    // A made directory, kept while the kernel holds its lookup, whatever it forgets of the trees.
    let mut nonce = ino::PUT;
    for (unique, child) in (10..).zip(["cas", "b", "build-1", "nonce-1"]) {
        let bytes = request(op::LOOKUP, unique, nonce, &name(child.as_bytes()));
        assert_eq!(handle(&fs, &device, &tokens, &bytes), Handled::Done);
        let replies = taken(&device);
        nonce = u64::from_ne_bytes(replies[0].2[..8].try_into().unwrap());
    }
    let bytes = request(op::CREATE, 20, nonce, &create(b"a.part"));
    let _ = handle(&fs, &device, &tokens, &bytes);
    let bytes = request(op::MKDIR, 21, nonce, &mkdir(b"sub"));
    let _ = handle(&fs, &device, &tokens, &bytes);
    let sub = u64::from_ne_bytes(taken(&device)[1].2[..8].try_into().unwrap());
    fs.forget(ino::TREES, 1);
    fs.forget(ino::ROOT, 1);
    let bytes = request(op::LOOKUP, 22, nonce, &name(b"sub"));
    let _ = handle(&fs, &device, &tokens, &bytes);
    let looked = taken(&device);
    assert_eq!(
        u64::from_ne_bytes(looked[0].2[..8].try_into().unwrap()),
        sub
    );
    // The `mkdir` and the lookup above: two lookups the kernel holds.
    fs.forget(sub, 2);
    let _ = handle(&fs, &device, &tokens, &bytes);
    let looked = taken(&device);
    assert_eq!(u64::from_ne_bytes(looked[0].2[..8].try_into().unwrap()), 0);
}

/// Asserts that a rename answered on the reader's thread is [`Handled::Done`] (a refused flag, a
/// name that is not UTF-8, a rename of the `.part` name, a commit refused at once) and that a
/// commit that uploads, answered from the commit's own thread, is [`Handled::Parked`] and
/// answered exactly once.
#[test]
fn a_commit_that_uploads_is_parked_and_every_other_rename_is_inline() {
    let rig = SpaceRig::new("putfs-parked");
    let fs = PutFs::new(Arc::new(rig.space));
    let (device, tokens) = (Arc::new(Recorder::default()), Tokens::new());
    let mut nonce = ino::PUT;
    for (unique, child) in (10..).zip(["cas", "b", "build-1", "nonce-1"]) {
        let bytes = request(op::LOOKUP, unique, nonce, &name(child.as_bytes()));
        let _ = handle(&fs, &device, &tokens, &bytes);
        nonce = u64::from_ne_bytes(taken(&device)[0].2[..8].try_into().unwrap());
    }
    let bytes = request(op::CREATE, 20, nonce, &create(b"a.part"));
    let _ = handle(&fs, &device, &tokens, &bytes);
    let made = taken(&device).remove(0).2;
    let (file, fh) = (
        u64::from_ne_bytes(made[..8].try_into().unwrap()),
        u64::from_ne_bytes(made[128..136].try_into().unwrap()),
    );
    let bytes = request(op::WRITE, 21, file, &write(fh, 0, b"bytes"));
    let _ = handle(&fs, &device, &tokens, &bytes);

    let inline: [(u64, Vec<u8>, i32); 4] = [
        (
            30,
            rename2(nonce, EXCHANGE, b"a.part", b"b.part"),
            errno::EPERM,
        ),
        (31, rename2(nonce, 0, b"a.part", b"\xff"), errno::EINVAL),
        (32, rename2(nonce, NOREPLACE, b"a.part", b"b.part"), 0),
        // Still open: the commit is refused at once, with nothing to wait for.
        (33, rename2(nonce, 0, b"b.part", b"b"), -16),
    ];
    let _ = taken(&device);
    for (unique, body, _) in &inline {
        let bytes = request(op::RENAME2, *unique, nonce, body);
        assert_eq!(
            handle(&fs, &device, &tokens, &bytes),
            Handled::Done,
            "{unique}"
        );
    }
    let answered: Vec<(i32, u64)> = taken(&device)
        .into_iter()
        .map(|(error, unique, _)| (error, unique))
        .collect();
    let want: Vec<(i32, u64)> = inline.iter().map(|(u, _, e)| (*e, *u)).collect();
    assert_eq!(answered, want);

    let bytes = request(op::RELEASE, 40, file, &release(fh));
    let _ = handle(&fs, &device, &tokens, &bytes);
    let _ = taken(&device);
    let commit = request(op::RENAME, 41, nonce, &rename(nonce, b"b.part", b"b"));
    assert_eq!(handle(&fs, &device, &tokens, &commit), Handled::Parked);
    crate::server::treefs::testing::until(|| !device.0.lock().unwrap().is_empty());
    assert_eq!(taken(&device), [(0, 41, Vec::new())]);
    assert_eq!(tokens.registered(), 0);
    assert_eq!(
        rig.puts.object("cas/b/build-1/nonce-1/b"),
        Some(b"bytes".to_vec())
    );
}
