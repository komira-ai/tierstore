//! `NodeFs`, the FUSE frontend of the node's mount on `tsfuse`: its answers to each request of the
//! kernel, driven with no kernel. The tests here call [`NodeFs::handle`] with a replier of a
//! device that records the reply (a stand-in for `/dev/fuse`, as `put_fs_tests.rs` does) and check
//! each opcode's answer and each of its refusals. The handshake, the routing of the put
//! directory, and what the frontend gives back to the core when the kernel did not take an answer
//! are in `node_fs_session_tests.rs`; the tests over a real mount are in
//! `node_fs_kernel_tests.rs`.

use std::any::Any;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

use tsfuse::abi::{
    AttrOut, CreateIn, Dirent, DirentPlus, EntryOut, FlushIn, FsyncIn, InHeader, InitConfig,
    Kstatfs, LinkIn, MkdirIn, MknodIn, NAME_MAX, OpenIn, OpenOut, ReadIn, ReleaseIn, Rename2In,
    RenameIn, SetattrIn, StatfsOut, WriteIn, fopen,
};
use tsfuse::{Errno, Fs, Handled, Interrupted, ReplyDevice, Request, Tokens};

use super::{ABSENT, NodeFs, attr as attr_of, errno as kernel_errno};
use crate::server::treefs::backing::{BackingId, FILE, Guard, Saved};
use crate::server::treefs::core::{self, FIXED_MTIME, Kind, TREE_TTL};
use crate::server::treefs::put::testing::SpaceRig;
use crate::server::treefs::serving::{init_config, mount_options};
use crate::server::treefs::testing::{Built, FakeKernel, OWNER, Rig, file, sample, sha, until};
use crate::server::treefs::{handle, ino};
use crate::tree_manifest::TreeDigest;

/// The run the sample tree is attached for.
const RUN: &str = "run-1";

/// The threads of the fetches of the frontend, as many as the readers of the default settings.
const FETCHERS: std::num::NonZeroUsize = std::num::NonZeroUsize::new(16).unwrap();

/// The entries of the sample tree (`testing::sample`) in its tree, numbered 1: the index of each.
const BIN: u32 = 2;
const CC: u32 = 3;
const TOOL: u32 = 4;
const EMPTY: u32 = 5;
const A_H: u32 = 7;
const LIB: u32 = 8;

/// The inode number of entry `index` of the sample tree.
fn node(index: u32) -> u64 {
    ino::of(1, index)
}

/// Opcodes of the uapi `fuse.h` the tests send.
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
    pub(super) const REMOVEXATTR: u32 = 24;
    pub(super) const FLUSH: u32 = 25;
    pub(super) const INIT: u32 = 26;
    pub(super) const OPENDIR: u32 = 27;
    pub(super) const READDIR: u32 = 28;
    pub(super) const RELEASEDIR: u32 = 29;
    pub(super) const CREATE: u32 = 35;
    pub(super) const INTERRUPT: u32 = 36;
    pub(super) const DESTROY: u32 = 38;
    pub(super) const BATCH_FORGET: u32 = 42;
    pub(super) const READDIRPLUS: u32 = 44;
    pub(super) const RENAME2: u32 = 45;
}

/// An errno as the kernel's end reads it in a reply: negated.
fn reads(errno: rustix::io::Errno) -> i32 {
    -errno.raw_os_error()
}

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
fn cstr(name: &[u8]) -> Vec<u8> {
    [name, b"\0"].concat()
}

/// The body of an OPEN for `flags`.
fn open_in(flags: u32) -> Vec<u8> {
    OpenIn {
        flags,
        open_flags: 0,
    }
    .as_bytes()
    .to_vec()
}

/// The body of a READ or a READDIR of `size` bytes at `offset` of the open `fh`.
fn read_in(fh: u64, offset: u64, size: u32) -> Vec<u8> {
    ReadIn {
        fh,
        offset,
        size,
        ..ReadIn::default()
    }
    .as_bytes()
    .to_vec()
}

/// The body of a RELEASE of the open `fh`.
fn release_in(fh: u64) -> Vec<u8> {
    ReleaseIn {
        fh,
        ..ReleaseIn::default()
    }
    .as_bytes()
    .to_vec()
}

/// A reply as the kernel's end reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    error: i32,
    unique: u64,
    body: Vec<u8>,
}

/// The reply in `bytes`, one message of the device.
fn parse_reply(bytes: &[u8]) -> Reply {
    let len = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len(), "the header's length is the message's");
    Reply {
        error: i32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
        unique: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
        body: bytes[16..].to_vec(),
    }
}

fn entry_out(reply: &Reply) -> EntryOut {
    EntryOut::read_prefix(&reply.body).unwrap().0
}

fn attr_out(reply: &Reply) -> AttrOut {
    AttrOut::read_prefix(&reply.body).unwrap().0
}

fn open_out(reply: &Reply) -> OpenOut {
    OpenOut::read_prefix(&reply.body).unwrap().0
}

/// One record of a READDIR or READDIRPLUS reply.
#[derive(Debug)]
struct Listed {
    ino: u64,
    off: u64,
    typ: u32,
    name: String,
    /// The entry that came with the name (READDIRPLUS).
    entry: Option<EntryOut>,
}

/// The records of the body of a READDIR, or of a READDIRPLUS with `plus`.
fn listing(body: &[u8], plus: bool) -> Vec<Listed> {
    let (mut rest, mut records) = (body, Vec::new());
    while !rest.is_empty() {
        let entry = plus.then(|| {
            let (entry, after) = EntryOut::read_prefix(rest).unwrap();
            rest = after;
            entry
        });
        let (dirent, after) = Dirent::read_prefix(rest).unwrap();
        let len = dirent.namelen as usize;
        let name = String::from_utf8(after[..len].to_vec()).unwrap();
        rest = &after[(Dirent::SIZE + len).next_multiple_of(8) - Dirent::SIZE..];
        records.push(Listed {
            ino: dirent.ino,
            off: dirent.off,
            typ: dirent.typ,
            name,
            entry,
        });
    }
    records
}

/// The names of `records`.
fn names(records: &[Listed]) -> Vec<&str> {
    records.iter().map(|listed| listed.name.as_str()).collect()
}

/// A device that records each reply written to it.
#[derive(Default)]
struct Recorder(Mutex<Vec<Reply>>);

impl ReplyDevice for Recorder {
    fn send(&self, parts: &[&[u8]]) -> std::io::Result<()> {
        self.0.lock().unwrap().push(parse_reply(&parts.concat()));
        Ok(())
    }
}

/// A device that has gone: it takes no reply.
struct Gone;

impl ReplyDevice for Gone {
    fn send(&self, _parts: &[&[u8]]) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }
}

/// What becomes of a reply.
#[derive(Clone, Copy, Debug)]
enum Delivery {
    /// The kernel takes it.
    Taken,
    /// An INTERRUPT answered the request `EINTR` first, so the reply is not written.
    Interrupted,
    /// The device is gone, so the write fails.
    Gone,
}

/// Asserts that of `replies` only what the way the reply was lost leaves: the `EINTR` the
/// INTERRUPT wrote, or nothing for a device that is gone.
fn assert_not_taken(how: Delivery, replies: &[Reply]) {
    match how {
        Delivery::Interrupted => {
            let errors: Vec<i32> = replies.iter().map(|reply| reply.error).collect();
            assert_eq!(errors, [reads(rustix::io::Errno::INTR)]);
        }
        Delivery::Gone => assert!(replies.is_empty(), "{replies:?}"),
        Delivery::Taken => panic!("a reply that was taken"),
    }
}

/// A rig with the sample tree attached for [`RUN`], a `NodeFs` over its core, and the recording
/// device its requests are answered on.
struct Wire {
    fs: NodeFs,
    rig: Rig,
    device: Arc<Recorder>,
    tokens: Tokens,
    next: Cell<u64>,
    /// The scratch directory of the put directory, where it is served.
    _put: Option<Box<dyn Any>>,
}

impl Wire {
    /// A frontend over the sample tree, with no put directory.
    fn new(name: &str) -> Self {
        Self::over(name, sample(), false)
    }

    /// A frontend over the sample tree, with the put directory served.
    fn with_put(name: &str) -> Self {
        Self::over(name, sample(), true)
    }

    /// A frontend over `built`, with the put directory served or not.
    fn over(name: &str, built: Built, put: bool) -> Self {
        Self::on(Rig::new(name, built), name, put)
    }

    /// A frontend over the core of `rig`, with the put directory served or not.
    fn on(rig: Rig, name: &str, put: bool) -> Self {
        let root = rig.upstream.built.lock().unwrap().root.clone();
        let _ = rig.core.attach(&root, RUN).unwrap();
        let scratch = put.then(|| {
            let space = SpaceRig::new(&format!("{name}-put"));
            rig.core.serve_put(Arc::new(space.space));
            Box::new(space.rig) as Box<dyn Any>
        });
        Self {
            fs: NodeFs::new(Arc::clone(&rig.core), FETCHERS).unwrap(),
            rig,
            device: Arc::new(Recorder::default()),
            tokens: Tokens::new(),
            next: Cell::new(0),
            _put: scratch,
        }
    }

    /// Hands a request to the frontend and gives the replies the device got, whatever way `how`
    /// has the reply lost or taken.
    fn deliver(&self, how: Delivery, opcode: u32, nodeid: u64, body: &[u8]) -> Vec<Reply> {
        let unique = self.next.get() + 1;
        self.next.set(unique);
        let bytes = request(opcode, unique, nodeid, body);
        let req = Request::parse(&bytes).unwrap();
        let device: Arc<dyn ReplyDevice> = match how {
            Delivery::Gone => Arc::new(Gone),
            _ => self.device.clone(),
        };
        let replier = self.tokens.register(unique, device);
        if let Delivery::Interrupted = how {
            let interrupted = self.tokens.interrupt(unique, 0, &*self.device);
            assert!(matches!(interrupted, Interrupted::Cancelled));
        }
        if self.fs.handle(req, replier) == Handled::Parked {
            // Answered by a thread of the fetches: once it is, the reply is on the device.
            until(|| self.fs.parked() == 0);
        }
        std::mem::take(&mut *self.device.0.lock().unwrap())
    }

    /// The one reply to a request the kernel takes.
    fn ask(&self, opcode: u32, nodeid: u64, body: &[u8]) -> Reply {
        let mut replies = self.deliver(Delivery::Taken, opcode, nodeid, body);
        assert_eq!(replies.len(), 1, "opcode {opcode} on node {nodeid:#x}");
        replies.remove(0)
    }

    /// The error of the reply to a request the kernel takes.
    fn error(&self, opcode: u32, nodeid: u64, body: &[u8]) -> i32 {
        self.ask(opcode, nodeid, body).error
    }

    /// What the core has served, by kind.
    fn served(&self, what: &str) -> usize {
        self.rig
            .core
            .served
            .lock()
            .unwrap()
            .iter()
            .filter(|got| **got == what)
            .count()
    }

    /// The value of the sample of the node's cache's `/metricz` text that begins with `series`.
    fn metric(&self, series: &str) -> u64 {
        let prefix = format!("{series} ");
        let text = self.rig.node.metrics();
        let found = text
            .lines()
            .find_map(|line| line.strip_prefix(&prefix).map(|n| n.parse().unwrap()));
        found.unwrap_or_else(|| panic!("no sample {series} in the cache's metrics"))
    }

    /// The reads of the mount that joined a fill the cache already had running.
    fn joins(&self) -> u64 {
        self.metric("tierstore_cache_fill_joins_total{path=\"mount\"}")
    }

    /// The trees the core keeps in memory once the run is detached: those of which the kernel
    /// holds a lookup, as far as the core counted.
    fn kept_after_detach(&self) -> usize {
        let _ = self.rig.core.detach(RUN);
        self.rig.core.trees_in_memory()
    }

    /// Has the core answer the open of `bin/cc` with backing id `id`, as the guard `wrap` makes.
    fn adopt(&self, id: u32, wrap: &dyn Fn(u32) -> Guard) {
        let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
        let saved = Saved {
            blob: TreeDigest::from_hex(&sha(&cc)).unwrap(),
            id,
            opens: 1,
            exact: false,
        };
        let path = self.rig.runtime.join(FILE);
        self.rig.core.adopt_backing(&path, &[saved], wrap);
    }
}

/// Asserts that each refusal of the core is the errno the kernel is told.
#[test]
fn each_refusal_of_the_core_is_an_errno_of_the_kernel() {
    use crate::server::treefs::core::Errno as Core;
    let cases = [
        (Core::NoEnt, rustix::io::Errno::NOENT),
        (Core::Acces, rustix::io::Errno::ACCESS),
        (Core::NotDir, rustix::io::Errno::NOTDIR),
        (Core::IsDir, rustix::io::Errno::ISDIR),
        (Core::Inval, rustix::io::Errno::INVAL),
        (Core::Io, rustix::io::Errno::IO),
        (Core::RoFs, rustix::io::Errno::ROFS),
    ];
    for (core, kernel) in cases {
        assert_eq!(kernel_errno(core), Errno(kernel.raw_os_error()), "{core:?}");
    }
}

/// Asserts the attributes the kernel is sent: the type bits of each kind with the permission
/// bits, the size in blocks of 512 rounded up, the core's fixed time for all three times, a block
/// size of 4096, and the owner and link count as the core says.
#[test]
fn attributes_carry_the_kind_the_blocks_and_the_fixed_times() {
    let of = |kind, size| core::Attr {
        ino: 77,
        kind,
        size,
        perm: 0o640,
        nlink: 3,
        uid: 11,
        gid: 12,
    };
    let modes = [
        (Kind::Dir, 0o040_640),
        (Kind::File, 0o100_640),
        (Kind::Symlink, 0o120_640),
    ];
    for (kind, mode) in modes {
        let sent = attr_of(&of(kind, 1025));
        assert_eq!(
            (sent.ino, sent.size, sent.blocks),
            (77, 1025, 3),
            "{kind:?}"
        );
        assert_eq!((sent.mode, sent.nlink), (mode, 3), "{kind:?}");
        assert_eq!((sent.uid, sent.gid, sent.blksize), (11, 12, 4096));
        let times = (sent.atime, sent.mtime, sent.ctime);
        assert_eq!(times, (FIXED_MTIME, FIXED_MTIME, FIXED_MTIME));
    }
    assert_eq!(attr_of(&of(Kind::File, 512)).blocks, 1);
    assert_eq!(attr_of(&of(Kind::File, 0)).blocks, 0);
}

/// Asserts the options of the mount: the kernel checks the modes, the mount is read-write (the
/// put directory is below its root) and `nosuid` and `nodev`, it is named for tierstore as source
/// and as subtype, other users are let in only where the settings say so, and the `INIT` terms
/// are the put mount's of `tsfuse` with 1024 background requests.
#[test]
fn the_mount_options_let_others_in_only_where_the_settings_say_so() {
    let shared = mount_options(true);
    assert!(shared.allow_other);
    assert!(!mount_options(false).allow_other);
    assert_eq!(
        mount_options(false),
        tsfuse::mount::MountOptions {
            allow_other: false,
            ..shared.clone()
        }
    );
    assert!(shared.default_permissions);
    assert!(shared.nosuid && shared.nodev);
    assert!(!shared.read_only);
    assert_eq!(shared.fsname, "tierstore-fs");
    assert_eq!(shared.subtype.as_deref(), Some("tierstore-fs"));
    assert_eq!(init_config(), InitConfig::put());
    assert_eq!(init_config().max_background, 1024);
}

/// Asserts a LOOKUP: a name that is there is its entry (inode, generation 0, the core's validity
/// for the name and the attributes, the attributes), a name that is not is an entry of inode 0 valid
/// for the core's time, which the kernel keeps as a negative entry, and the namespace is valid for
/// no time.
#[test]
fn a_lookup_answers_the_entry_of_a_name_and_inode_0_for_a_name_that_is_not_there() {
    let wire = Wire::new("nodefs-lookup");
    let found = wire.ask(op::LOOKUP, node(BIN), &cstr(b"cc"));
    assert_eq!((found.error, found.unique), (0, 1));
    let entry = entry_out(&found);
    let valid = TREE_TTL.as_secs();
    assert_eq!((entry.nodeid, entry.generation), (node(CC), 0));
    assert_eq!((entry.entry_valid, entry.attr_valid), (valid, valid));
    let (attr, owner) = (entry.attr, (entry.attr.uid, entry.attr.gid));
    assert_eq!(
        (attr.ino, attr.size, attr.blocks, attr.nlink),
        (node(CC), 3000, 6, 1)
    );
    assert_eq!(
        (attr.mode, owner, attr.mtime),
        (0o100_755, OWNER, FIXED_MTIME)
    );

    let absent = wire.ask(op::LOOKUP, node(BIN), &cstr(b"nothing.h"));
    let entry = entry_out(&absent);
    assert_eq!((absent.error, entry.nodeid), (0, 0));
    assert_eq!((entry.entry_valid, entry.attr_valid), (valid, valid));
    assert_eq!(entry.attr, attr_of(&ABSENT));

    let namespace = entry_out(&wire.ask(op::LOOKUP, ino::ROOT, &cstr(b"trees")));
    assert_eq!((namespace.nodeid, namespace.entry_valid), (ino::TREES, 0));
    assert_eq!(namespace.attr.mode, 0o040_711);
}

/// Asserts the refusals of a LOOKUP: a name that is not text is not there (and is not asked of the
/// core), a parent that is a file is `ENOTDIR`, a name the namespace does not have is `ENOENT`.
#[test]
fn a_lookup_that_cannot_be_answered_is_an_errno() {
    let wire = Wire::new("nodefs-lookup-refused");
    let odd = wire.error(op::LOOKUP, node(BIN), &cstr(b"caf\xe9"));
    assert_eq!(odd, reads(rustix::io::Errno::NOENT));
    assert_eq!(wire.served("lookup"), 0);
    let below_file = wire.error(op::LOOKUP, node(CC), &cstr(b"x"));
    assert_eq!(below_file, reads(rustix::io::Errno::NOTDIR));
    let views = wire.error(op::LOOKUP, ino::ROOT, &cstr(b"views"));
    assert_eq!(views, reads(rustix::io::Errno::NOENT));
}

/// Asserts GETATTR and READLINK: the attributes the core reports with its validity (a file's, a
/// directory's with its link count, a link's, the namespace's valid for no time), the target of a
/// link, and the refusals: an inode no tree has, and the target of a file.
#[test]
fn getattr_answers_the_attributes_and_readlink_the_target() {
    let wire = Wire::new("nodefs-getattr");
    let getattr = |ino| wire.ask(op::GETATTR, ino, &[0; 16]);
    let cc = attr_out(&getattr(node(CC)));
    assert_eq!(cc.attr_valid, TREE_TTL.as_secs());
    assert_eq!(
        (cc.attr.ino, cc.attr.size, cc.attr.mode),
        (node(CC), 3000, 0o100_755)
    );
    let bin = attr_out(&getattr(node(BIN))).attr;
    assert_eq!((bin.mode, bin.nlink, bin.size), (0o040_755, 2, 0));
    let lib = attr_out(&getattr(node(LIB))).attr;
    assert_eq!((lib.mode, lib.size), (0o120_777, 3));
    let root = attr_out(&getattr(ino::ROOT));
    assert_eq!(
        (root.attr_valid, root.attr.mode, root.attr.nlink),
        (0, 0o040_711, 3)
    );
    assert_eq!(getattr(node(99)).error, reads(rustix::io::Errno::NOENT));

    let link = wire.ask(op::READLINK, node(LIB), &[]);
    assert_eq!((link.error, link.body), (0, b"bin".to_vec()));
    let file = wire.error(op::READLINK, node(CC), &[]);
    assert_eq!(file, reads(rustix::io::Errno::INVAL));
}

/// Asserts that an OPEN is a cached open (`FOPEN_KEEP_CACHE`, no backing id) with a handle, that a
/// READ of the handle gives the file's bytes, that a RELEASE ends the open, that a READ of the
/// closed handle is `EINVAL`, and that a file of no bytes opens and reads as empty.
#[test]
fn an_open_is_cached_and_its_reads_reach_the_core_until_it_is_released() {
    let wire = Wire::new("nodefs-open");
    let opened = open_out(&wire.ask(op::OPEN, node(CC), &open_in(0)));
    assert_eq!(
        (opened.open_flags, opened.backing_id),
        (fopen::KEEP_CACHE, 0)
    );
    assert_eq!(wire.rig.core.open_files(), 1);
    let read = wire.ask(op::READ, node(CC), &read_in(opened.fh, 0, 16));
    let first: Vec<u8> = (0..16u32).map(|i| (i % 241) as u8).collect();
    assert_eq!((read.error, read.body), (0, first));
    let released = wire.ask(op::RELEASE, node(CC), &release_in(opened.fh));
    assert_eq!((released.error, released.body.len()), (0, 0));
    assert_eq!(wire.rig.core.open_files(), 0);
    let closed = wire.error(op::READ, node(CC), &read_in(opened.fh, 0, 16));
    assert_eq!(closed, reads(rustix::io::Errno::INVAL));

    let empty = open_out(&wire.ask(op::OPEN, node(EMPTY), &open_in(0)));
    let none = wire.ask(op::READ, node(EMPTY), &read_in(empty.fh, 0, 16));
    assert_eq!((none.error, none.body.len()), (0, 0));
}

/// Asserts that a READ that names a handle an earlier process gave out is made again as a cached
/// open and answered, and that its RELEASE ends it.
#[test]
fn a_read_of_a_handle_an_earlier_process_gave_out_is_answered() {
    let wire = Wire::new("nodefs-foreign-handle");
    let earlier = handle::make(7, 3);
    let read = wire.ask(op::READ, node(A_H), &read_in(earlier, 0, 100));
    assert_eq!((read.error, read.body), (0, b"int a;".to_vec()));
    assert_eq!(wire.rig.core.open_files(), 1);
    assert_eq!(wire.error(op::RELEASE, node(A_H), &release_in(earlier)), 0);
    assert_eq!(wire.rig.core.open_files(), 0);
}

/// Asserts the refusals of an OPEN: for writing (`O_WRONLY` or `O_RDWR`) is `EROFS`, a directory
/// `EISDIR` and a link `EINVAL`; none leaves a handle open.
#[test]
fn an_open_that_the_core_refuses_is_an_errno_and_leaves_no_handle() {
    let wire = Wire::new("nodefs-open-refused");
    let cases = [
        (node(CC), 0o1, rustix::io::Errno::ROFS),
        (node(CC), 0o2, rustix::io::Errno::ROFS),
        (node(BIN), 0, rustix::io::Errno::ISDIR),
        (node(LIB), 0, rustix::io::Errno::INVAL),
    ];
    for (ino, flags, errno) in cases {
        assert_eq!(
            wire.error(op::OPEN, ino, &open_in(flags)),
            reads(errno),
            "{ino:#x}"
        );
    }
    assert_eq!(wire.rig.core.open_files(), 0);
}

/// Asserts that an open of a blob the core holds a backing id of an earlier release for is
/// answered `FOPEN_PASSTHROUGH` with that id, and that the opens of other files are cached ones.
#[test]
fn an_open_of_a_blob_with_an_adopted_id_is_answered_with_the_id() {
    let wire = Wire::new("nodefs-adopted");
    wire.adopt(9, &|id| -> Guard { Arc::new(BackingId(id)) });
    let opened = open_out(&wire.ask(op::OPEN, node(CC), &open_in(0)));
    assert_eq!(
        (opened.open_flags, opened.backing_id),
        (fopen::PASSTHROUGH, 9)
    );
    assert_eq!(wire.rig.core.open_files(), 1);
    let other = open_out(&wire.ask(op::OPEN, node(TOOL), &open_in(0)));
    assert_eq!((other.open_flags, other.backing_id), (fopen::KEEP_CACHE, 0));
}

/// Asserts that an open of a blob the core holds an adopted id for, which the frontend cannot
/// answer with (an id too large for the kernel's `i32`, or a handle that is not a [`BackingId`],
/// here the test kernel's), fails with `EIO` and ends the open the core made, rather than
/// answering a cached open on an inode the kernel serves by passthrough; and that the opens of
/// other files are cached opens as before.
#[test]
fn an_adopted_id_too_large_or_of_another_kind_fails_the_open_and_ends_it() {
    let too_large = Wire::new("nodefs-adopted-large");
    too_large.adopt(u32::MAX, &|id| -> Guard { Arc::new(BackingId(id)) });
    let kernel = FakeKernel::new();
    let foreign = Wire::new("nodefs-adopted-foreign");
    foreign.adopt(9, &|id| kernel.wrap(id));
    for wire in [&too_large, &foreign] {
        let refused = wire.error(op::OPEN, node(CC), &open_in(0));
        assert_eq!(refused, reads(rustix::io::Errno::IO));
        assert_eq!(wire.rig.core.open_files(), 0);
        let other = open_out(&wire.ask(op::OPEN, node(TOOL), &open_in(0)));
        assert_eq!(other.open_flags, fopen::KEEP_CACHE);
    }
}

/// Asserts OPENDIR and RELEASEDIR: a directory opens with handle 0 and `FOPEN_CACHE_DIR`, the
/// namespace is `EACCES` and a file `ENOTDIR`, and a RELEASEDIR is answered whatever the handle.
#[test]
fn a_directory_opens_with_a_cached_listing_and_is_released() {
    let wire = Wire::new("nodefs-opendir");
    let opened = open_out(&wire.ask(op::OPENDIR, node(BIN), &open_in(0)));
    let flags = (opened.fh, opened.open_flags, opened.backing_id);
    assert_eq!(flags, (0, fopen::CACHE_DIR, 0));
    let refused = |ino| wire.error(op::OPENDIR, ino, &open_in(0));
    assert_eq!(refused(ino::TREES), reads(rustix::io::Errno::ACCESS));
    assert_eq!(refused(node(CC)), reads(rustix::io::Errno::NOTDIR));
    let released = wire.ask(op::RELEASEDIR, node(BIN), &release_in(5));
    assert_eq!((released.error, released.body.len()), (0, 0));
}

/// Asserts a READDIR: `.` and `..` then the entries in the manifest's order, each with its inode,
/// the position that follows it and its type, from a position, and an empty reply at the end; and
/// that a plain listing counts no lookup.
#[test]
fn a_plain_listing_has_the_entries_and_counts_no_lookup() {
    let wire = Wire::new("nodefs-readdir");
    let list = |offset| wire.ask(op::READDIR, node(BIN), &read_in(0, offset, 4096));
    let all = listing(&list(0).body, false);
    assert_eq!(names(&all), [".", "..", "cc", "tool"]);
    let at = |listed: &Listed| (listed.ino, listed.off, listed.typ);
    assert_eq!(at(&all[0]), (node(BIN), 1, 4));
    assert_eq!(at(&all[1]), (node(0), 2, 4));
    assert_eq!(at(&all[2]), (node(CC), 3, 8));
    assert_eq!(at(&all[3]), (node(TOOL), 4, 8));
    assert!(all.iter().all(|listed| listed.entry.is_none()));
    assert_eq!(names(&listing(&list(3).body, false)), ["tool"]);
    assert!(list(4).body.is_empty());
    assert_eq!(wire.served("count"), 0);

    let root = wire.ask(op::READDIR, node(0), &read_in(0, 0, 4096));
    let root = listing(&root.body, false);
    let kinds: Vec<u32> = root.iter().map(|listed| listed.typ).collect();
    assert_eq!(
        names(&root),
        [".", "..", "big.bin", "bin", "empty.h", "include", "lib"]
    );
    assert_eq!(kinds, [4, 4, 8, 4, 8, 4, 10]);
}

/// Asserts a READDIRPLUS: the same records with the entry of each, as a LOOKUP answers it, and a
/// lookup counted for every entry but `.` and `..`, the way the kernel takes them.
#[test]
fn a_listing_with_entries_counts_a_lookup_of_every_entry_but_dot_and_dotdot() {
    let wire = Wire::new("nodefs-readdirplus");
    let reply = wire.ask(op::READDIRPLUS, node(BIN), &read_in(0, 0, 4096));
    let all = listing(&reply.body, true);
    assert_eq!(names(&all), [".", "..", "cc", "tool"]);
    let entries: Vec<(u64, u64)> = all
        .iter()
        .map(|listed| {
            let entry = listed.entry.as_ref().unwrap();
            (entry.nodeid, entry.entry_valid)
        })
        .collect();
    let valid = TREE_TTL.as_secs();
    assert_eq!(
        entries,
        [
            (node(BIN), valid),
            (node(0), valid),
            (node(CC), valid),
            (node(TOOL), valid)
        ]
    );
    assert_eq!(all[2].entry.as_ref().unwrap().attr.size, 3000);
    assert_eq!((all[2].ino, all[2].off), (node(CC), 3));
    assert_eq!(wire.served("count"), 2);

    let root = wire.ask(op::READDIRPLUS, node(0), &read_in(0, 0, 4096));
    assert_eq!(listing(&root.body, true).len(), 7);
    assert_eq!(wire.served("count"), 2 + 5);
}

/// Asserts that a listing cut by a buffer too small for the next entry ends before it, with that
/// entry not counted (READDIRPLUS), and that the next request from the last position sent gives
/// the rest.
#[test]
fn a_listing_cut_by_a_short_buffer_ends_before_the_entry_that_does_not_fit() {
    let wire = Wire::new("nodefs-readdir-cut");
    // Two records fit: every name here is up to 8 bytes after the dirent.
    let plain = u32::try_from(2 * (Dirent::SIZE + 4).next_multiple_of(8)).unwrap();
    let cut = wire.ask(op::READDIR, node(BIN), &read_in(0, 0, plain));
    let cut = listing(&cut.body, false);
    assert_eq!(names(&cut), [".", ".."]);
    let rest = wire.ask(op::READDIR, node(BIN), &read_in(0, cut[1].off, 4096));
    assert_eq!(names(&listing(&rest.body, false)), ["cc", "tool"]);

    // Three records fit: every name here is up to 8 bytes after the entry and dirent.
    let record = (DirentPlus::SIZE + 4).next_multiple_of(8);
    let size = u32::try_from(3 * record).unwrap();
    let cut = wire.ask(op::READDIRPLUS, node(BIN), &read_in(0, 0, size));
    let cut = listing(&cut.body, true);
    assert_eq!(names(&cut), [".", "..", "cc"]);
    assert_eq!(wire.served("count"), 1);
    let rest = wire.ask(op::READDIRPLUS, node(BIN), &read_in(0, cut[2].off, 4096));
    assert_eq!(names(&listing(&rest.body, true)), ["tool"]);
    assert_eq!(wire.served("count"), 2);
}

/// Asserts that a listing the core refuses is its errno, plain and with entries, and counts no
/// lookup: the namespace is `EACCES`, a file `ENOTDIR`.
#[test]
fn a_listing_the_core_refuses_is_an_errno_and_counts_nothing() {
    let wire = Wire::new("nodefs-readdir-refused");
    for opcode in [op::READDIR, op::READDIRPLUS] {
        let namespace = wire.error(opcode, ino::TREES, &read_in(0, 0, 4096));
        assert_eq!(namespace, reads(rustix::io::Errno::ACCESS));
        let file = wire.error(opcode, node(CC), &read_in(0, 0, 4096));
        assert_eq!(file, reads(rustix::io::Errno::NOTDIR));
    }
    assert_eq!(wire.served("count"), 0);
}

/// Asserts that a name the kernel would refuse (here one longer than the longest it accepts)
/// fails the listing with `EIO`, plain and with entries, and counts no lookup.
#[test]
fn a_name_the_kernel_would_refuse_fails_the_listing() {
    let long = "x".repeat(NAME_MAX + 1);
    let built = Built::of(vec![("a", file(b"")), (long.as_str(), file(b""))]);
    let wire = Wire::over("nodefs-readdir-name", built, false);
    for opcode in [op::READDIR, op::READDIRPLUS] {
        let listed = wire.error(opcode, node(0), &read_in(0, 0, 1 << 16));
        assert_eq!(listed, reads(rustix::io::Errno::IO), "opcode {opcode}");
    }
    assert_eq!(wire.served("count"), 0);
}

/// Asserts STATFS: no blocks and no files, blocks of 512 and names of 255.
#[test]
fn statfs_has_no_blocks_blocks_of_512_and_names_of_255() {
    let wire = Wire::new("nodefs-statfs");
    let reply = wire.ask(op::STATFS, ino::ROOT, &[]);
    let expected = Kstatfs {
        bsize: 512,
        namelen: 255,
        ..Kstatfs::default()
    };
    assert_eq!(reply.error, 0);
    assert_eq!(
        StatfsOut::read_prefix(&reply.body).unwrap().0,
        StatfsOut { st: expected }
    );
}

/// Asserts that every request that would change a tree is `EROFS`, whatever it is: SETATTR, MKNOD,
/// MKDIR, UNLINK, RMDIR, SYMLINK, RENAME, RENAME2, LINK, WRITE, CREATE, SETXATTR and REMOVEXATTR.
#[test]
fn every_request_that_would_change_a_tree_is_answered_erofs() {
    let wire = Wire::new("nodefs-erofs");
    let bin = node(BIN);
    let renamed = |new: &[u8]| [cstr(b"cc"), cstr(new)].concat();
    let rename2 = Rename2In {
        newdir: bin,
        flags: 0,
        padding: 0,
    };
    let requests: Vec<(u32, Vec<u8>)> = vec![
        (op::SETATTR, SetattrIn::default().as_bytes().to_vec()),
        (
            op::MKNOD,
            [MknodIn::default().as_bytes(), &cstr(b"n")].concat(),
        ),
        (
            op::MKDIR,
            [MkdirIn::default().as_bytes(), &cstr(b"d")].concat(),
        ),
        (op::UNLINK, cstr(b"cc")),
        (op::RMDIR, cstr(b"d")),
        (op::SYMLINK, [cstr(b"l"), cstr(b"x")].concat()),
        (
            op::RENAME,
            [RenameIn { newdir: bin }.as_bytes(), &renamed(b"c2")].concat(),
        ),
        (op::RENAME2, [rename2.as_bytes(), &renamed(b"c2")].concat()),
        (
            op::LINK,
            [
                LinkIn {
                    oldnodeid: node(CC),
                }
                .as_bytes(),
                &cstr(b"l"),
            ]
            .concat(),
        ),
        (
            op::WRITE,
            [
                WriteIn {
                    size: 3,
                    ..WriteIn::default()
                }
                .as_bytes(),
                b"abc".as_slice(),
            ]
            .concat(),
        ),
        (
            op::CREATE,
            [CreateIn::default().as_bytes(), &cstr(b"n")].concat(),
        ),
        (op::SETXATTR, vec![0; 16]),
        (op::REMOVEXATTR, cstr(b"user.x")),
    ];
    for (opcode, body) in requests {
        let error = wire.error(opcode, bin, &body);
        assert_eq!(error, reads(rustix::io::Errno::ROFS), "opcode {opcode}");
    }
}

/// Asserts that FLUSH and FSYNC are `ENOSYS` (a tree is never written; the kernel stops sending
/// them after the first), and so is what the session answers itself and never hands over, if a
/// caller hands it over anyway: INIT, DESTROY, INTERRUPT, FORGET and BATCH_FORGET.
#[test]
fn flush_and_fsync_and_the_requests_of_the_session_are_enosys() {
    let wire = Wire::new("nodefs-enosys");
    let (flush, fsync) = (FlushIn::default(), FsyncIn::default());
    let requests: [(u32, &[u8]); 7] = [
        (op::FLUSH, flush.as_bytes()),
        (op::FSYNC, fsync.as_bytes()),
        (op::INIT, &[0; 64]),
        (op::DESTROY, &[]),
        (op::INTERRUPT, &[0; 8]),
        (op::FORGET, &[0; 8]),
        (op::BATCH_FORGET, &[0; 8]),
    ];
    for (opcode, body) in requests {
        let error = wire.error(opcode, node(CC), body);
        assert_eq!(error, reads(rustix::io::Errno::NOSYS), "opcode {opcode}");
    }
}

/// Asserts that a body too short for its opcode, or a name with no NUL, is `EIO`, and that an
/// opcode the library does not implement is `ENOSYS`, each answered once.
#[test]
fn a_malformed_request_is_answered_with_the_errno_of_the_parse() {
    let wire = Wire::new("nodefs-malformed");
    assert_eq!(
        wire.error(op::GETATTR, node(CC), &[0; 3]),
        reads(rustix::io::Errno::IO)
    );
    assert_eq!(
        wire.error(op::LOOKUP, node(BIN), b"cc"),
        reads(rustix::io::Errno::IO)
    );
    assert_eq!(
        wire.error(op::GETXATTR, node(CC), &[0; 8]),
        reads(rustix::io::Errno::NOSYS)
    );
    assert_eq!(
        wire.error(1000, node(CC), &[]),
        reads(rustix::io::Errno::NOSYS)
    );
}

#[path = "node_fs_session_tests.rs"]
mod session;

#[path = "node_fs_kernel_tests.rs"]
mod kernel;

#[path = "node_fs_shared_read_tests.rs"]
mod shared_reads;

#[path = "node_fs_stall_tests.rs"]
mod stall;

#[path = "node_fs_open_tests.rs"]
mod opens;

#[path = "node_fs_fault_tests.rs"]
mod faults;

#[path = "node_fs_request_tests.rs"]
mod requests;
