//! The harness of the tree mount's tests (`tree_fs_tests.rs`, `tree_fs_read_tests.rs`,
//! `tree_fs_adapter_tests.rs`): a tree built into an L2 list and served by the rig of
//! `tierstore-tree`, a [`TreeFs`] over it, and a fake device (a socket pair) whose replies the
//! test reads. Requests are raw bytes handed to [`TreeFs::handle`]; no kernel is involved.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use rustix::net::{AddressFamily, RecvFlags, SocketFlags, SocketType, recv, socketpair};
use sha2::{Digest as _, Sha256};
use tierstore_fetch_api::ids::CellRun;
use tierstore_fetch_api::list::NodeIndex;
use tierstore_list::{PackCells, PackInfo, sidecar_from_v2};
use tierstore_tree::testing::Rig;
use tierstore_tree::{ImageOptions, NodeKind, Tree};
use tsfuse::abi::{Attr, AttrOut, Dirent, DirentPlus, EntryOut, InHeader};
use tsfuse::{DevFd, Handled, ReplyConfig, ReplyDevice, ReplyPath, Request, Tokens};

use super::TreeFs;
use crate::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};

pub(super) const LOOKUP: u32 = 1;
pub(super) const GETATTR: u32 = 3;
pub(super) const READLINK: u32 = 5;
pub(super) const OPEN: u32 = 14;
pub(super) const READ: u32 = 15;
pub(super) const STATFS: u32 = 17;
pub(super) const RELEASE: u32 = 18;
pub(super) const FLUSH: u32 = 25;
pub(super) const OPENDIR: u32 = 27;
pub(super) const READDIR: u32 = 28;
pub(super) const RELEASEDIR: u32 = 29;
pub(super) const READDIRPLUS: u32 = 44;

pub(super) const S_IFDIR: u32 = 0o040_000;
pub(super) const S_IFREG: u32 = 0o100_000;
pub(super) const S_IFLNK: u32 = 0o120_000;

/// One MiB: the size of a cell.
pub(super) const MIB: u64 = 1 << 20;

/// The root's inode.
pub(super) const ROOT: u64 = 1;

/// A reply read from the device.
#[derive(Debug)]
pub(super) struct Msg {
    pub error: i32,
    pub unique: u64,
    pub payload: Vec<u8>,
}

impl Msg {
    /// The errno of an error reply (0 for an answer).
    pub fn errno(&self) -> i32 {
        -self.error
    }

    /// The payload of an answer.
    pub fn ok(&self) -> &[u8] {
        assert_eq!(self.error, 0, "an answer, got errno {}", self.errno());
        &self.payload
    }

    /// The reply as the lookup entry it carries.
    pub fn entry(&self) -> EntryOut {
        let (entry, rest) = EntryOut::read_prefix(self.ok()).expect("an entry");
        assert!(rest.is_empty(), "nothing after the entry");
        entry
    }

    /// The reply as the attributes it carries and how long they are valid, in seconds.
    pub fn attr(&self) -> (Attr, u64) {
        let (out, rest) = AttrOut::read_prefix(self.ok()).expect("attributes");
        assert!(rest.is_empty(), "nothing after the attributes");
        assert_eq!(out.attr_valid_nsec, 0);
        (out.attr, out.attr_valid)
    }
}

/// One entry of a listing, decoded.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Listed {
    pub name: String,
    pub ino: u64,
    pub off: u64,
    pub typ: u32,
    /// The entry READDIRPLUS carries: its node id and its attributes' mode.
    pub entry: Option<(u64, u32)>,
}

/// The entries in a READDIR (`plus` false) or READDIRPLUS reply.
pub(super) fn listing(payload: &[u8], plus: bool) -> Vec<Listed> {
    let mut rest = payload;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (entry, dirent, tail) = if plus {
            let (head, tail) = DirentPlus::read_prefix(rest).expect("a direntplus");
            (
                Some((head.entry_out.nodeid, head.entry_out.attr.mode)),
                head.dirent,
                tail,
            )
        } else {
            let (dirent, tail) = Dirent::read_prefix(rest).expect("a dirent");
            (None, dirent, tail)
        };
        let len = dirent.namelen as usize;
        out.push(Listed {
            name: String::from_utf8(tail[..len].to_vec()).expect("a utf-8 name"),
            ino: dirent.ino,
            off: dirent.off,
            typ: dirent.typ,
            entry,
        });
        // The name is padded to 8 bytes after the record's head.
        let head_len = rest.len() - tail.len();
        let record = (head_len + len).div_ceil(8) * 8;
        rest = &rest[record..];
    }
    out
}

/// The bytes of a pack of `len` bytes that differ at every offset and for each `seed`.
pub(super) fn pack_bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            (at as u64)
                .wrapping_mul(31)
                .wrapping_add(u64::from(seed) * 7) as u8
        })
        .collect()
}

/// A tree being made: its entries and its packs by key.
#[derive(Default)]
pub(super) struct Spec {
    entries: Vec<ListEntry>,
    packs: BTreeMap<String, Vec<u8>>,
}

impl Spec {
    pub fn pack(mut self, key: &str, bytes: Vec<u8>) -> Self {
        self.packs.insert(key.to_owned(), bytes);
        self
    }

    pub fn dir(mut self, path: &str, mode: u16) -> Self {
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::Dir {
                mode: Mode::new(mode).expect("a mode"),
            },
        });
        self
    }

    /// A file of `len` bytes at `offset` of pack `key`, of mode `mode`.
    pub fn file(mut self, path: &str, mode: u16, key: &str, offset: u64, len: u64) -> Self {
        let bytes = &self.packs[key][offset as usize..(offset + len) as usize];
        let sha: [u8; 32] = Sha256::digest(bytes).into();
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::File {
                mode: Mode::new(mode).expect("a mode"),
                size: len,
                source: Some(BlobSource {
                    key: Arc::from(key),
                    offset,
                }),
                sha256: Some(TreeDigest::from_bytes(sha)),
            },
        });
        self
    }

    pub fn empty_file(mut self, path: &str) -> Self {
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::File {
                mode: Mode::FILE,
                size: 0,
                source: None,
                sha256: None,
            },
        });
        self
    }

    pub fn link(mut self, path: &str, target: &str) -> Self {
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::Symlink {
                target: target.to_owned(),
            },
        });
        self
    }

    /// The L2 list of the tree.
    fn l2(&self) -> Vec<u8> {
        let v2 = TreeList {
            entries: self.entries.clone(),
        }
        .encode_v2()
        .expect("a version 2 list");
        sidecar_from_v2(&v2, |key| {
            self.packs.get(key).map(|bytes| PackInfo {
                cells: PackCells::of(bytes),
                upload_part_size: 0,
                shared: false,
            })
        })
        .expect("a sidecar")
    }
}

/// The tree of most tests: two packs (`out/a/0` of 2.5 MiB, three cells, the last short; `out/b/0`
/// of 110 bytes in one cell), directories of their own modes, an executable, an empty file, a link.
pub(super) fn sample() -> Spec {
    Spec::default()
        .pack("out/a/0", pack_bytes(1, (2 * MIB + MIB / 2) as usize))
        .pack("out/b/0", pack_bytes(2, 110))
        .dir("bin", 0o755)
        .file("bin/cc", 0o644, "out/a/0", 0, MIB + 10)
        .file("bin/ld", 0o644, "out/a/0", MIB + 4096, MIB)
        .file("bin/x", 0o755, "out/b/0", 100, 10)
        .dir("lib", 0o750)
        .file("lib/a.txt", 0o644, "out/b/0", 0, 60)
        .file("lib/b.txt", 0o644, "out/b/0", 60, 40)
        .empty_file("lib/empty")
        .link("lib/self", "a.txt")
        .dir("tmp", 0o1777)
}

/// A tree whose directory `wide` has `files` empty files `f000`.. and two subdirectories, beside
/// one file of a pack.
pub(super) fn wide(files: usize) -> Spec {
    let mut spec = Spec::default()
        .pack("out/p/0", pack_bytes(3, 100))
        .file("one", 0o644, "out/p/0", 0, 100)
        .dir("wide", 0o755)
        .dir("wide/d0", 0o755)
        .dir("wide/d1", 0o700);
    for n in 0..files {
        spec = spec.empty_file(&format!("wide/f{n:03}"));
    }
    spec
}

/// A mounted tree: the real tree on a rig, a [`TreeFs`] over it and the kernel's end of the
/// device its replies go to.
pub(super) struct Harness {
    pub rig: Rig,
    pub tree: Arc<Tree>,
    pub fs: TreeFs,
    pub tokens: Tokens,
    pub dev: Arc<DevFd>,
    pub l2: Vec<u8>,
    kernel: UnixStream,
    unique: u64,
}

/// A fake device: a stream socket pair, which is a splice target (a datagram or packet socket cuts
/// a spliced reply into several messages, which `/dev/fuse` does not); replies are told apart by
/// the length in their headers.
pub(super) fn device() -> (Arc<DevFd>, UnixStream) {
    let (device, kernel) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .expect("a socket pair");
    (Arc::new(DevFd::from_fd(device)), UnixStream::from(kernel))
}

impl Harness {
    /// The mount of `spec`, in the scratch directory `name`.
    pub fn new(name: &str, spec: &Spec) -> Self {
        let packs = spec.packs.values().cloned().collect();
        let l2 = spec.l2();
        let rig = Rig::new(
            &std::env::temp_dir(),
            &format!("treefs-{name}"),
            l2.clone(),
            packs,
        );
        let tree = Arc::new(rig.tree(ImageOptions { lock: false }));
        let reply = ReplyPath::new(ReplyConfig {
            splice_min: Some(16 * 1024),
            max_data: 128 * 1024,
            pool: 2,
            move_pages: false,
        })
        .expect("a reply path");
        let fs = TreeFs::new(
            Arc::clone(&tree),
            Arc::new(reply),
            rig.runtime().handle().clone(),
            (1000, 1000),
        );
        let (dev, kernel) = device();
        Self {
            rig,
            tree,
            fs,
            tokens: Tokens::new(),
            dev,
            l2,
            kernel,
            unique: 100,
        }
    }

    /// Hands a request to the mount, on the harness's device; its unique id and what the mount
    /// said. The reply, if one is written, is read with [`Self::recv`].
    pub fn submit(&mut self, opcode: u32, nodeid: u64, body: &[u8]) -> (u64, Handled) {
        let device: Arc<dyn ReplyDevice> = self.dev.clone();
        self.submit_on(device, opcode, nodeid, body)
    }

    /// [`Self::submit`] on `device`.
    pub fn submit_on(
        &mut self,
        device: Arc<dyn ReplyDevice>,
        opcode: u32,
        nodeid: u64,
        body: &[u8],
    ) -> (u64, Handled) {
        self.unique += 1;
        let unique = self.unique;
        let header = InHeader {
            len: (InHeader::SIZE + body.len()) as u32,
            opcode,
            unique,
            nodeid,
            ..InHeader::default()
        };
        let bytes = [header.as_bytes(), body].concat();
        let request = Request::parse(&bytes).expect("a request");
        let replier = self.tokens.register(unique, device);
        (unique, tsfuse::Fs::handle(&self.fs, request, replier))
    }

    /// Reads the next reply from the device, blocking until there is one.
    pub fn recv(&self) -> Msg {
        recv_from(&self.kernel)
    }

    /// Whether the device has a reply waiting now (looked at, not read). Call it only once the
    /// state that makes a reply impossible or certain has been reached by a count.
    pub fn has_reply(&self) -> bool {
        has_reply_on(&self.kernel)
    }

    /// A request answered inline: its reply.
    pub fn call(&mut self, opcode: u32, nodeid: u64, body: &[u8]) -> Msg {
        let (unique, handled) = self.submit(opcode, nodeid, body);
        assert!(
            matches!(handled, Handled::Done),
            "opcode {opcode} is answered inline"
        );
        let msg = self.recv();
        assert_eq!(msg.unique, unique);
        msg
    }

    pub fn lookup(&mut self, parent: u64, name: &str) -> Msg {
        self.call(LOOKUP, parent, &[name.as_bytes(), &[0]].concat())
    }

    pub fn getattr(&mut self, nodeid: u64) -> Msg {
        self.call(GETATTR, nodeid, &[0; 16])
    }

    /// The inode of `path` by LOOKUPs from the root; the last entry answered.
    pub fn resolve(&mut self, path: &str) -> EntryOut {
        let mut at = ROOT;
        let mut last = None;
        for name in path.split('/').filter(|n| !n.is_empty()) {
            let entry = self.lookup(at, name).entry();
            at = entry.nodeid;
            last = Some(entry);
        }
        last.expect("a non-empty path")
    }

    /// READDIR or READDIRPLUS of `nodeid` from `offset`, in a buffer of `size`.
    pub fn readdir(&mut self, nodeid: u64, offset: u64, size: u32, plus: bool) -> Msg {
        let opcode = if plus { READDIRPLUS } else { READDIR };
        self.call(opcode, nodeid, &read_body(offset, size))
    }

    /// Every entry of `nodeid`, read in buffers of `size` until an empty one.
    pub fn list_all(&mut self, nodeid: u64, size: u32, plus: bool) -> Vec<Listed> {
        let mut all: Vec<Listed> = Vec::new();
        loop {
            let offset = all.last().map_or(0, |last| last.off);
            let page = listing(self.readdir(nodeid, offset, size, plus).ok(), plus);
            if page.is_empty() {
                return all;
            }
            all.extend(page);
        }
    }

    /// The image's index of `path`.
    pub fn index_of(&self, path: &str) -> NodeIndex {
        let image = self.tree.image();
        let mut at = NodeIndex(0);
        for name in path.split('/').filter(|n| !n.is_empty()) {
            at = image
                .lookup(at, name.as_bytes())
                .expect("a path of the tree");
        }
        at
    }

    /// Where file `path`'s bytes are: its pack, its offset in it and its size.
    pub fn place(&self, path: &str) -> (Option<u32>, u64, u64) {
        let node = self.tree.image().node(self.index_of(path)).expect("a node");
        match node.kind {
            NodeKind::File { pack, offset, size } => (pack, offset, size),
            NodeKind::Dir { .. } | NodeKind::Symlink { .. } => (None, 0, 0),
        }
    }

    /// The bytes of file `path`.
    pub fn content(&self, path: &str) -> Vec<u8> {
        match self.place(path) {
            (Some(pack), offset, size) => {
                self.rig.bytes(pack)[offset as usize..(offset + size) as usize].to_vec()
            }
            (None, _, _) => Vec::new(),
        }
    }

    /// Makes every cell of file `path` `present`; its pack.
    pub fn publish_file(&self, path: &str) -> u32 {
        let (pack, offset, size) = self.place(path);
        let pack = pack.expect("a file with bytes");
        let first = (offset >> 20) as u32;
        let last = ((offset + size - 1) >> 20) as u32;
        self.rig.publish(
            pack,
            CellRun {
                first,
                count: last - first + 1,
            },
        );
        pack
    }

    /// Waits until the mount has answered `n` parked reads.
    pub fn until_answered(&self, n: u64) {
        let answered = || {
            std::thread::yield_now();
            self.fs.stats().parked_answered
        };
        let _ = std::iter::repeat_with(answered).find(|&got| got == n);
    }
}

/// Whether `kernel`'s end of a device has a reply waiting (looked at, not read).
pub(super) fn has_reply_on(kernel: &UnixStream) -> bool {
    let mut byte = [std::mem::MaybeUninit::<u8>::uninit(); 1];
    let peeked = recv(kernel, &mut byte, RecvFlags::PEEK | RecvFlags::DONTWAIT);
    peeked.is_ok()
}

/// The next reply on `kernel`'s end of a device: its header, then the bytes the header says.
pub(super) fn recv_from(kernel: &UnixStream) -> Msg {
    let mut header = [0u8; 16];
    (&*kernel).read_exact(&mut header).expect("a reply header");
    let len = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes")) as usize;
    assert!(len >= 16, "a reply is at least its header");
    let mut payload = vec![0u8; len - 16];
    (&*kernel).read_exact(&mut payload).expect("a reply body");
    Msg {
        error: i32::from_le_bytes(header[4..8].try_into().expect("4 bytes")),
        unique: u64::from_le_bytes(header[8..16].try_into().expect("8 bytes")),
        payload,
    }
}

/// The body of READ and READDIR: `offset` and `size`.
pub(super) fn read_body(offset: u64, size: u32) -> Vec<u8> {
    let mut body = vec![0u8; 40];
    body[8..16].copy_from_slice(&offset.to_le_bytes());
    body[16..20].copy_from_slice(&size.to_le_bytes());
    body
}
