//! What the null file system on `tsfuse` answers to each request, through a fake device: the
//! tree's entries and attributes with their timeouts, the open modes, READ from the data file
//! by `writev` and by splice, listings in pieces, the errors of the tree, and the requests it
//! does not serve. Over a real mount: `tests/tsnull_integration.rs`.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::num::NonZeroUsize;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tsfuse::abi::{AttrOut, EntryOut, GetattrIn, OpenIn, OpenOut, ReadIn, ReleaseIn, fopen};
use tsfuse::{DevFd, Fs as _, Handled, ReplyDevice, Request, Tokens};

use super::{
    Options, Reply, Served, Transport, TsNullFs, URING_MAX_PAGES, mount_options, open_data,
    reply_config, start, transport_of,
};
use crate::list::{Entry, FileList};
use crate::nullfs::OpenMode;
use crate::tree::Tree;

const LOOKUP: u32 = 1;
const GETATTR: u32 = 3;
const OPEN: u32 = 14;
const READ: u32 = 15;
const RELEASE: u32 = 18;
const FLUSH: u32 = 25;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const RELEASEDIR: u32 = 29;
const READDIRPLUS: u32 = 44;
const ENOENT: i32 = -2;
const EIO: i32 = -5;
const ENOTDIR: i32 = -20;
const EISDIR: i32 = -21;
const ENOSYS: i32 = -38;

/// A device that keeps every reply written to it, and takes spliced replies into a pipe when it
/// has one.
#[derive(Default)]
struct FakeDevice {
    sent: Mutex<Vec<Vec<u8>>>,
    pipe: Option<(OwnedFd, OwnedFd)>,
}

impl ReplyDevice for FakeDevice {
    fn send(&self, parts: &[&[u8]]) -> std::io::Result<()> {
        self.sent.lock().unwrap().push(parts.concat());
        Ok(())
    }

    fn splice_target(&self) -> Option<BorrowedFd<'_>> {
        self.pipe.as_ref().map(|(_, write)| write.as_fd())
    }
}

/// A reply as the kernel reads it: `error` (a negative errno, 0 for success) and the bytes after
/// the header.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    error: i32,
    payload: Vec<u8>,
}

fn decode(unique: u64, bytes: &[u8]) -> Answer {
    let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len(), "the header's length is the reply's");
    assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), unique);
    Answer {
        error: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        payload: bytes[16..].to_vec(),
    }
}

fn list(paths: &[(&str, u64)]) -> FileList {
    FileList {
        files: paths
            .iter()
            .map(|&(path, size)| Entry {
                path: path.to_owned(),
                size,
            })
            .collect(),
        sha256: String::new(),
    }
}

/// `a/b/c.h` (3 bytes, inode 4), `a/d` (300,000 bytes, inode 5), `e` (empty, inode 6).
fn sample() -> FileList {
    list(&[("a/b/c.h", 3), ("a/d", 300_000), ("e", 0)])
}

fn options(open: OpenMode, reply: Reply) -> Options {
    Options {
        entry_timeout: Duration::from_millis(1500),
        attr_timeout: Duration::from_secs(7),
        open,
        readers: NonZeroUsize::new(2).unwrap(),
        reply,
        splice_min: 16 * 1024,
        move_pages: false,
        allow_other: false,
        transport: Transport::Dev,
    }
}

/// The byte at `i` of the data file of the tests: a pattern that differs at every offset a read
/// could be off by.
fn byte(i: u64) -> u8 {
    u8::try_from(i % 251).unwrap()
}

/// A null file system of `files` over a data file holding [`byte`] at every offset, and the
/// scratch directory that holds it.
fn fs_of(name: &str, files: &FileList, options: Options) -> (TsNullFs, TestDir) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let tree = Tree::of(files).unwrap();
    let path = dir.path().join("data");
    let bytes: Vec<u8> = (0..tree.data_len()).map(byte).collect();
    std::fs::write(&path, bytes).unwrap();
    let data = File::open(&path).unwrap();
    (
        TsNullFs::new(tree, options, (1000, 2000), data).unwrap(),
        dir,
    )
}

/// A request: the 40-byte header for `opcode`, `unique` and `nodeid`, then `body`.
fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(40 + body.len()).unwrap();
    let mut r = Vec::new();
    r.extend(len.to_le_bytes());
    r.extend(opcode.to_le_bytes());
    r.extend(unique.to_le_bytes());
    r.extend(nodeid.to_le_bytes());
    r.extend([0u8; 16]);
    r.extend(body);
    r
}

fn read_in(offset: u64, size: u32) -> Vec<u8> {
    ReadIn {
        offset,
        size,
        ..ReadIn::default()
    }
    .as_bytes()
    .to_vec()
}

/// Hands one request to `fs` on `device`: the reply written there, which must be the only one.
fn ask(fs: &TsNullFs, device: &Arc<FakeDevice>, opcode: u32, nodeid: u64, body: &[u8]) -> Answer {
    let unique = 7_000 + u64::from(opcode);
    let tokens = Tokens::new();
    let bytes = request(opcode, unique, nodeid, body);
    let replier = tokens.register(unique, device.clone());
    let handled = fs.handle(Request::parse(&bytes).unwrap(), replier);
    assert_eq!(handled, Handled::Done, "every request is answered inline");
    assert_eq!(tokens.registered(), 0, "and only once");
    let sent = std::mem::take(&mut *device.sent.lock().unwrap());
    assert_eq!(sent.len(), 1, "one reply");
    decode(unique, &sent[0])
}

/// The entry of a lookup: its node, its two timeouts, and the attributes of the node: the
/// mounting owner, mode 0444 and the size for a file, 0555 and two links for a directory.
#[test]
fn a_lookup_answers_the_entry_with_its_timeouts_and_attributes() {
    let (fs, _dir) = fs_of(
        "fsbench-tsnull-lookup",
        &sample(),
        options(OpenMode::Explicit, Reply::Writev),
    );
    let device = Arc::new(FakeDevice::default());
    let reply = ask(&fs, &device, LOOKUP, 1, b"a\0");
    assert_eq!(reply.error, 0);
    let (a, _) = EntryOut::read_prefix(&reply.payload).unwrap();
    assert_eq!((a.nodeid, a.generation), (2, 0));
    assert_eq!((a.entry_valid, a.entry_valid_nsec), (1, 500_000_000));
    assert_eq!((a.attr_valid, a.attr_valid_nsec), (7, 0));
    assert_eq!(
        (a.attr.ino, a.attr.mode, a.attr.nlink, a.attr.size),
        (2, 0o40_555, 2, 0)
    );
    assert_eq!((a.attr.uid, a.attr.gid, a.attr.blksize), (1000, 2000, 4096));
    let reply = ask(&fs, &device, LOOKUP, 2, b"d\0");
    let (d, _) = EntryOut::read_prefix(&reply.payload).unwrap();
    assert_eq!(
        (
            d.nodeid,
            d.attr.mode,
            d.attr.nlink,
            d.attr.size,
            d.attr.blocks
        ),
        (5, 0o100_444, 1, 300_000, 586)
    );
    assert_eq!(ask(&fs, &device, LOOKUP, 1, b"zz\0").error, ENOENT);
    assert_eq!(ask(&fs, &device, LOOKUP, 5, b"x\0").error, ENOTDIR);
    assert_eq!(ask(&fs, &device, LOOKUP, 99, b"a\0").error, ENOENT);
    assert_eq!(
        ask(&fs, &device, LOOKUP, 1, b"a").error,
        EIO,
        "a name without its NUL is a body the kernel never sends"
    );
}

/// GETATTR answers the node's attributes with the attribute timeout; an unknown node is ENOENT.
#[test]
fn getattr_answers_the_attributes_with_their_timeout() {
    let (fs, _dir) = fs_of(
        "fsbench-tsnull-getattr",
        &sample(),
        options(OpenMode::Explicit, Reply::Writev),
    );
    let device = Arc::new(FakeDevice::default());
    let body = GetattrIn::default();
    let reply = ask(&fs, &device, GETATTR, 4, body.as_bytes());
    assert_eq!(reply.error, 0);
    let (out, _) = AttrOut::read_prefix(&reply.payload).unwrap();
    assert_eq!((out.attr_valid, out.attr_valid_nsec), (7, 0));
    assert_eq!(
        (out.attr.ino, out.attr.size, out.attr.mode),
        (4, 3, 0o100_444)
    );
    assert_eq!(ask(&fs, &device, GETATTR, 0, body.as_bytes()).error, ENOENT);
}

/// The explicit mode answers OPEN with `FOPEN_KEEP_CACHE` and OPENDIR with no flag; the no-open
/// mode answers both `ENOSYS`. RELEASE and RELEASEDIR are answered with no payload; FLUSH and
/// any other request the library implements, `ENOSYS`.
#[test]
fn the_open_modes_answer_open_and_opendir() {
    let (fs, _dir) = fs_of(
        "fsbench-tsnull-open",
        &sample(),
        options(OpenMode::Explicit, Reply::Writev),
    );
    let device = Arc::new(FakeDevice::default());
    let open = OpenIn::default();
    let reply = ask(&fs, &device, OPEN, 5, open.as_bytes());
    assert_eq!(reply.error, 0);
    let (out, _) = OpenOut::read_prefix(&reply.payload).unwrap();
    assert_eq!(
        (out.fh, out.open_flags, out.backing_id),
        (0, fopen::KEEP_CACHE, 0)
    );
    let reply = ask(&fs, &device, OPENDIR, 1, open.as_bytes());
    let (out, _) = OpenOut::read_prefix(&reply.payload).unwrap();
    assert_eq!((reply.error, out.open_flags), (0, 0));
    let release = ReleaseIn::default();
    for opcode in [RELEASE, RELEASEDIR] {
        let reply = ask(&fs, &device, opcode, 5, release.as_bytes());
        assert_eq!((reply.error, reply.payload.len()), (0, 0));
    }
    assert_eq!(ask(&fs, &device, FLUSH, 5, &[0; 24]).error, ENOSYS);
    assert_eq!(ask(&fs, &device, 17, 1, &[]).error, ENOSYS, "STATFS");

    let (none, _dir) = fs_of(
        "fsbench-tsnull-noopen",
        &sample(),
        options(OpenMode::NoOpen, Reply::Writev),
    );
    assert_eq!(ask(&none, &device, OPEN, 5, open.as_bytes()).error, ENOSYS);
    assert_eq!(
        ask(&none, &device, OPENDIR, 1, open.as_bytes()).error,
        ENOSYS
    );
}

/// A READ by `writev` answers the file's bytes of the data file from the offset, up to the size
/// asked or the end of the file, none past it; a directory is EISDIR, an unknown node ENOENT.
/// Every byte goes by `pread`, whatever its size.
#[test]
fn a_read_by_writev_answers_the_files_bytes_of_the_data_file() {
    let (fs, _dir) = fs_of(
        "fsbench-tsnull-writev",
        &sample(),
        options(OpenMode::Explicit, Reply::Writev),
    );
    let device = Arc::new(FakeDevice::default());
    let at = |from: u64, n: u64| -> Vec<u8> { (from..from + n).map(byte).collect() };
    let reply = ask(&fs, &device, READ, 5, &read_in(0, 4096));
    assert_eq!(
        reply,
        Answer {
            error: 0,
            payload: at(4096, 4096)
        }
    );
    let reply = ask(&fs, &device, READ, 5, &read_in(299_000, 131_072));
    assert_eq!(
        reply.payload,
        at(4096 + 299_000, 1000),
        "the end of the file"
    );
    let reply = ask(&fs, &device, READ, 5, &read_in(0, 131_072));
    assert_eq!(reply.payload, at(4096, 131_072), "large: still by pread");
    let reply = ask(&fs, &device, READ, 4, &read_in(0, 4096));
    assert_eq!(reply.payload, at(0, 3), "never a byte of the next file");
    let reply = ask(&fs, &device, READ, 5, &read_in(300_000, 4096));
    assert_eq!(
        reply,
        Answer {
            error: 0,
            payload: Vec::new()
        }
    );
    assert_eq!(ask(&fs, &device, READ, 1, &read_in(0, 1)).error, EISDIR);
    assert_eq!(ask(&fs, &device, READ, 99, &read_in(0, 1)).error, ENOENT);
    let counts = fs.counts();
    assert_eq!(
        (counts.splice_bytes, counts.pread_bytes),
        (0, 4096 + 1000 + 131_072 + 3)
    );
}

/// With splice replies, a READ of at least the threshold onto a device that takes spliced replies
/// goes through a pipe: the device receives the header and the data in one splice, nothing by
/// `writev`, with the length the header names. A smaller READ, or one onto a device that takes
/// no splice, goes by `pread`.
#[test]
fn a_read_at_the_threshold_is_spliced_and_one_below_it_is_not() {
    let mut opts = options(OpenMode::Explicit, Reply::Splice);
    opts.move_pages = true;
    let (fs, _dir) = fs_of("fsbench-tsnull-splice", &sample(), opts);
    assert!(fs.pipe_size() >= 16 * 1024);
    let (read_end, write_end) = rustix::pipe::pipe().unwrap();
    let device = Arc::new(FakeDevice {
        sent: Mutex::default(),
        pipe: Some((read_end.try_clone().unwrap(), write_end)),
    });
    let unique = 99;
    let tokens = Tokens::new();
    let bytes = request(READ, unique, 5, &read_in(10, 16 * 1024));
    let handled = fs.handle(
        Request::parse(&bytes).unwrap(),
        tokens.register(unique, device.clone()),
    );
    assert_eq!(handled, Handled::Done);
    assert!(device.sent.lock().unwrap().is_empty(), "nothing by writev");
    let mut spliced = vec![0u8; 16 + 16 * 1024];
    File::from(read_end).read_exact(&mut spliced).unwrap();
    let expected: Vec<u8> = (4096 + 10..4096 + 10 + 16 * 1024).map(byte).collect();
    assert_eq!(
        decode(unique, &spliced),
        Answer {
            error: 0,
            payload: expected
        }
    );

    let below = ask(&fs, &device, READ, 5, &read_in(0, 16 * 1024 - 1));
    assert_eq!(below.payload.len(), 16 * 1024 - 1);
    let plain = Arc::new(FakeDevice::default());
    let no_splice = ask(&fs, &plain, READ, 5, &read_in(0, 32 * 1024));
    assert_eq!(no_splice.payload.len(), 32 * 1024);
    let counts = fs.counts();
    assert_eq!(
        (counts.splice_bytes, counts.pread_bytes),
        (16 * 1024, 16 * 1024 - 1 + 32 * 1024)
    );
}

/// A READ whose `pread` fails (a data file that cannot be read) is answered `EIO`.
#[test]
fn a_read_that_cannot_read_the_data_file_is_eio() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-tsnull-eio");
    let path = dir.path().join("data");
    let write_only = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    let tree = Tree::of(&sample()).unwrap();
    let opts = options(OpenMode::Explicit, Reply::Writev);
    let fs = TsNullFs::new(tree, opts, (0, 0), write_only).unwrap();
    let device = Arc::new(FakeDevice::default());
    assert_eq!(ask(&fs, &device, READ, 5, &read_in(0, 10)).error, EIO);
}

/// The names of one directory entry list: `(name, inode, offset)` of each record of a READDIR
/// reply (24-byte head, the name, padding to 8 bytes) or of a READDIRPLUS one (with the 128-byte
/// entry before it, whose node is returned too).
fn names(payload: &[u8], plus: bool) -> Vec<(String, u64, u64, Option<u64>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < payload.len() {
        let (entry, head) = match plus {
            true => {
                let (entry, _) = EntryOut::read_prefix(&payload[at..]).unwrap();
                (Some(entry.nodeid), at + EntryOut::SIZE)
            }
            false => (None, at),
        };
        let u64_at = |i: usize| u64::from_le_bytes(payload[i..i + 8].try_into().unwrap());
        let namelen =
            u32::from_le_bytes(payload[head + 16..head + 20].try_into().unwrap()) as usize;
        let name = String::from_utf8(payload[head + 24..head + 24 + namelen].to_vec()).unwrap();
        out.push((name, u64_at(head), u64_at(head + 8), entry));
        at = head + (24 + namelen).next_multiple_of(8);
    }
    out
}

/// READDIR lists `.`, `..` and the entries in name order from the offset asked, each with the
/// offset that resumes after it, as many as fit the size asked; READDIRPLUS lists the same with
/// each entry's lookup reply. A file is ENOTDIR, an unknown node ENOENT.
#[test]
fn a_listing_answers_entries_from_the_offset_as_many_as_fit() {
    let (fs, _dir) = fs_of(
        "fsbench-tsnull-list",
        &sample(),
        options(OpenMode::Explicit, Reply::Writev),
    );
    let device = Arc::new(FakeDevice::default());
    let reply = ask(&fs, &device, READDIR, 2, &read_in(0, 4096));
    let all = names(&reply.payload, false);
    let row = |n: &str, ino, off| (n.to_owned(), ino, off, None);
    assert_eq!(
        all,
        [
            row(".", 2, 1),
            row("..", 1, 2),
            row("b", 3, 3),
            row("d", 5, 4)
        ]
    );
    let reply = ask(&fs, &device, READDIR, 2, &read_in(2, 4096));
    assert_eq!(
        names(&reply.payload, false),
        [row("b", 3, 3), row("d", 5, 4)]
    );
    let reply = ask(&fs, &device, READDIR, 2, &read_in(0, 64 + 31));
    assert_eq!(
        names(&reply.payload, false),
        [row(".", 2, 1), row("..", 1, 2)],
        "two 32-byte records fit 95 bytes, a third does not"
    );
    let reply = ask(&fs, &device, READDIR, 2, &read_in(4, 4096));
    assert_eq!((reply.error, reply.payload.len()), (0, 0), "past the end");
    let plus = ask(&fs, &device, READDIRPLUS, 1, &read_in(0, 4096));
    let rows = names(&plus.payload, true);
    let plus_row = |n: &str, ino, off| (n.to_owned(), ino, off, Some(ino));
    assert_eq!(
        rows,
        [
            plus_row(".", 1, 1),
            plus_row("..", 1, 2),
            plus_row("a", 2, 3),
            plus_row("e", 6, 4)
        ]
    );
    assert_eq!(
        ask(&fs, &device, READDIR, 5, &read_in(0, 4096)).error,
        ENOTDIR
    );
    assert_eq!(
        ask(&fs, &device, READDIRPLUS, 99, &read_in(0, 4096)).error,
        ENOENT
    );
}

/// V3 has no splice threshold; V4 splices from the threshold asked, with or without
/// `SPLICE_F_MOVE`; either way the pipes are sized for a READ of 256 pages (the tree session's
/// `max_pages`) and one idle pipe and buffer is kept per reader.
#[test]
fn the_reply_configuration_follows_the_variant() {
    let page = rustix::param::page_size();
    let v3 = reply_config(&options(OpenMode::Explicit, Reply::Writev));
    assert_eq!(
        (v3.splice_min, v3.max_data, v3.pool, v3.move_pages),
        (None, 256 * page, 2, false)
    );
    let mut opts = options(OpenMode::NoOpen, Reply::Splice);
    opts.splice_min = 4096;
    opts.move_pages = true;
    let v4 = reply_config(&opts);
    assert_eq!((v4.splice_min, v4.move_pages), (Some(4096), true));
}

/// The mount is a read-only tree mount named and typed `fsbench-tsfuse`, open to its owner alone
/// unless asked.
#[test]
fn the_mount_is_a_tree_mount_named_fsbench_tsfuse() {
    let mut opts = options(OpenMode::Explicit, Reply::Writev);
    let owner = mount_options(&opts);
    assert_eq!(owner.fsname, "fsbench-tsfuse");
    assert_eq!(owner.subtype.as_deref(), Some("fsbench-tsfuse"));
    assert!(owner.read_only && owner.nosuid && owner.nodev && owner.default_permissions);
    assert!(!owner.allow_other);
    opts.allow_other = true;
    assert!(mount_options(&opts).allow_other);
}

/// The data file is made at the length asked when it is not there, extended when it is shorter
/// (its bytes kept), and left as it is when it is as long or longer; a path that cannot be
/// opened as a file is an error that names it.
#[test]
fn the_data_file_is_made_or_extended_to_its_length() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-tsnull-data");
    let path = dir.path().join("data");
    let made = open_data(&path, 8192).unwrap();
    assert_eq!(made.metadata().unwrap().len(), 8192);
    std::fs::write(&path, b"abc").unwrap();
    drop(open_data(&path, 10).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), b"abc\0\0\0\0\0\0\0");
    let mut longer = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    longer.write_all(b"xyz").unwrap();
    drop(open_data(&path, 4).unwrap());
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        13,
        "never shortened"
    );
    let error = open_data(dir.path(), 1).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with(&format!("open {}: ", dir.path().display())),
        "{error}"
    );
}

/// A device that ends before INIT (here a pipe whose writer is closed) starts no session, and the
/// error says so.
#[test]
fn a_device_that_ends_before_init_starts_no_session() {
    let (read_end, write_end) = rustix::pipe::pipe().unwrap();
    drop(write_end);
    let (fs, _dir) = fs_of(
        "fsbench-tsnull-start",
        &sample(),
        options(OpenMode::Explicit, Reply::Writev),
    );
    let fs: Arc<TsNullFs> = Arc::new(fs);
    let error = start(
        DevFd::from_fd(read_end),
        NonZeroUsize::MIN,
        Transport::Dev,
        fs.clone(),
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with("tsfuse-null-fs: init: "),
        "{error}"
    );
    let (read_end, write_end) = rustix::pipe::pipe().unwrap();
    drop(write_end);
    let error = start(
        DevFd::from_fd(read_end),
        NonZeroUsize::MIN,
        Transport::Uring(tsfuse::UringConfig::default()),
        fs,
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with("tsfuse-null-fs: init: "),
        "over io_uring too: {error}"
    );
}

/// A session on `/dev/fuse` asks for the tree's 256 pages, one over io_uring for 32 (so the
/// default depth fits 64 MiB on 88 CPUs), and the reply path's buffers follow.
#[test]
fn a_session_over_io_uring_asks_for_32_pages_and_its_buffers_follow() {
    let uring = Transport::Uring(tsfuse::UringConfig::default());
    assert_eq!(
        Transport::Dev.init_config(),
        tsfuse::abi::InitConfig::tree()
    );
    assert_eq!(
        uring.init_config(),
        tsfuse::abi::InitConfig {
            max_pages: 32,
            ..tsfuse::abi::InitConfig::tree()
        }
    );
    assert_eq!(URING_MAX_PAGES, 32);
    let page = rustix::param::page_size();
    let mut opts = options(OpenMode::Explicit, Reply::Writev);
    assert_eq!(reply_config(&opts).max_data, 256 * page);
    opts.transport = uring;
    assert_eq!(reply_config(&opts).max_data, 32 * page);
    let payload = tsfuse::UringMemory::payload(4096, URING_MAX_PAGES, 4096);
    assert_eq!(
        tsfuse::UringMemory::of(88, 5, payload).bytes,
        57_798_400,
        "the default depth on 88 CPUs registers 55 MiB, within 64 MiB"
    );
}

/// The transport is said as `io_uring` with what its entries register, or as `dev-fuse` with
/// the reason the session did not use io_uring.
#[test]
fn the_transport_is_said_with_its_memory_or_its_reason() {
    let memory = tsfuse::UringMemory::of(88, 5, 128 * 1024);
    assert_eq!(
        transport_of(tsfuse::TransportMode::Uring(memory)),
        ("io_uring".to_owned(), Some(memory))
    );
    assert_eq!(
        transport_of(tsfuse::TransportMode::DevFuse(tsfuse::Fallback::Disabled)),
        ("dev-fuse (Disabled)".to_owned(), None)
    );
    assert_eq!(
        transport_of(tsfuse::TransportMode::DevFuse(
            tsfuse::Fallback::OverMemoryLimit {
                memory,
                limit: 64 << 20
            }
        ))
        .0,
        format!("dev-fuse (OverMemoryLimit {{ memory: {memory:?}, limit: 67108864 }})")
    );
}

/// What the file system says when it has unmounted is one line of JSON with every count.
#[test]
fn what_it_served_is_one_line_of_json() {
    let served = Served {
        readers: 4,
        granted: "InitFlags(ASYNC_READ)".to_owned(),
        missing: "InitFlags()".to_owned(),
        max_pages: 256,
        pipe_size: 1 << 20,
        splice_bytes: 3,
        pread_bytes: 5,
        transport: "dev-fuse (NotAsked)".to_owned(),
        uring_registered_bytes: None,
        uring_depth: None,
        uring_queues: None,
    };
    assert_eq!(
        serde_json::to_string(&served).unwrap(),
        r#"{"readers":4,"granted":"InitFlags(ASYNC_READ)","missing":"InitFlags()","max_pages":256,"pipe_size":1048576,"splice_bytes":3,"pread_bytes":5,"transport":"dev-fuse (NotAsked)","uring_registered_bytes":null,"uring_depth":null,"uring_queues":null}"#
    );
    let ring = Served {
        transport: "io_uring".to_owned(),
        uring_registered_bytes: Some(57_798_400),
        uring_depth: Some(5),
        uring_queues: Some(88),
        max_pages: 32,
        ..served
    };
    let line = serde_json::to_value(&ring).unwrap();
    assert_eq!(
        (
            &line["transport"],
            &line["uring_registered_bytes"],
            &line["uring_depth"],
            &line["uring_queues"]
        ),
        (
            &serde_json::json!("io_uring"),
            &serde_json::json!(57_798_400),
            &serde_json::json!(5),
            &serde_json::json!(88)
        )
    );
}
