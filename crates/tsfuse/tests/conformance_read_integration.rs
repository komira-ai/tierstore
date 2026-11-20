//! Kernel conformance, item 3 of `docs/design/fetch-cache-serve.md` 5.2 (READ, row T-F7): a
//! mount served by [`Session::serve`] whose [`Fs`] ([`conformance::Model`]) answers every READ
//! through a [`tsfuse::ReplyPath`] from a memfd data file, once with the splice arm
//! (`ReplyConfig::for_session`) and once with the `writev` arm (`splice_min: None`). The io_uring
//! arm of item 3 waits for the io_uring transport (row T-F5); no test is written for it here.
//!
//! Direct reads (`FOPEN_DIRECT_IO`) make every `pread` of a caller READs with a waiting caller; the
//! caller's bytes are compared by SHA-256 with the bytes the tree spec says the range holds, every
//! completion must be `Done::Written`, and the bytes the reply path counted must equal the bytes
//! answered.
//!
//! - [`direct_preads_of_every_range_shape_are_answered_byte_exact_by_splice`] and
//!   [`direct_preads_of_every_range_shape_are_answered_byte_exact_by_writev`]: a range inside the
//!   file, spanning a 1 MiB cell boundary, ending at one, at EOF (0 bytes), straddling EOF
//!   (exactly the bytes there), past EOF (0 bytes), of zero length (0 bytes, no READ reaches the
//!   server), just below and at the 16 KiB splice threshold, and 3 MiB at once into a page-aligned
//!   buffer: several READs of at most `min(max_pages, fs.fuse.max_pages_limit)` pages, in order,
//!   covering the range (K21). A reply is spliced exactly when it is at least `SPLICE_MIN` bytes and
//!   fits the pipe the kernel granted; none is on the `writev` arm.
//! - [`ranges_over_holes_read_zeros_there_and_the_file_elsewhere`]: a file with a 1 MiB aligned hole
//!   and an unaligned one punched into its data file reads zeros in the holes and the data around
//!   them, on both arms.
//! - [`a_short_file_and_an_empty_file_read_whole_return_their_bytes`]: a file of 1000 bytes returns
//!   them; an empty file returns nothing.
//! - [`a_reply_is_as_long_as_the_bytes_the_data_file_had`]: the file whose attribute size is 512 KiB
//!   but whose data file holds 300 KiB, read whole, is answered with 300 KiB, rebuilt by `pread`
//!   (the splice ended short), and the kernel takes it (K15: it refuses a reply whose length is not
//!   the bytes that follow the header) and the caller gets exactly 300 KiB.
//! - [`buffered_reads_of_a_mount_without_open_are_whole_and_no_larger_than_max_readahead`]: the tree
//!   mount's own mode (OPEN answered `ENOSYS` under `NO_OPEN_SUPPORT`, `ASYNC_READ`): a buffered
//!   `std::fs::read` of the big file and of the holey file returns them whole, each READ is at most
//!   the negotiated `max_readahead` (K20, K21), and exactly one OPEN is sent (K18).
//! - [`spliced_replies_of_four_readers_answering_at_once_each_reach_their_caller`]: four readers held
//!   at a barrier inside `Fs::handle`, so each holds one READ of a 256 KiB direct `pread`, answer
//!   by splice at once; the kernel takes every reply on the clone that read its request (K1) and
//!   each caller gets its bytes.
//!
//! The io_uring arm of item 3 waits for the io_uring transport (row T-F5). Every wait is on an
//! event (a caller's system call returning, the file system's own counters), never on a timer.
//!
//! An integration target: it mounts through `mount(2)` as root (the farm) or `fusermount3` (CI),
//! and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod conformance;
mod kernel;

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use conformance::{
    Answered, CELL, Done, FOPEN_DIRECT_IO, Mounted, Open, Opendir, ROOT, Read, Served, TreeSpec,
    answer_read, memfd, op, pattern, pread, reader_name, serve, sha256,
};
use kernel::{Aligned, skip};
use rustix::fs::{FallocateFlags, fallocate};
use tsfuse::abi::InitFlags;
use tsfuse::{Handled, ReaderEnd, ReplyConfig, ReplyPath, SPLICE_MIN};

const MIB: usize = 1 << 20;
const KIB: usize = 1 << 10;
/// The size of the big file: three cells and a few bytes, so that its end is in no page's edge.
const BIG: usize = 3 * MIB + 123;
/// The holey file's size.
const HOLEY: usize = 3 * MIB;
/// The two holes punched into the holey file's data file: one cell, and an unaligned range.
const HOLES: [(usize, usize); 2] = [(MIB, 2 * MIB), (100 * KIB + 5, 300 * KIB)];
/// The attribute size of the truncated file, and the bytes its data file holds.
const TRUNC_SIZE: usize = 512 * KIB;
const TRUNC_HAS: usize = 300 * KIB;

/// How the replies are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    Splice,
    Writev,
}

/// The inodes of the files of the tree.
struct Files {
    big: u64,
    holey: u64,
    short: u64,
    empty: u64,
    trunc: u64,
}

/// The bytes of the holey file: its pattern, zero in the holes.
fn holey_bytes() -> Vec<u8> {
    let mut bytes = pattern(2, HOLEY);
    for (from, to) in HOLES {
        bytes[from..to].fill(0);
    }
    bytes
}

/// The tree: the files `big`, `holey`, `short`, `empty` and `trunc`, each backed by a memfd.
fn tree() -> (Arc<TreeSpec>, Files) {
    let mut tree = TreeSpec::new();
    let big = tree.file(
        ROOT,
        "big",
        BIG as u64,
        Some(memfd("big", &pattern(1, BIG))),
    );
    let holey_fd = memfd("holey", &pattern(2, HOLEY));
    for (from, to) in HOLES {
        let punch = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
        fallocate(&holey_fd, punch, from as u64, (to - from) as u64).unwrap();
    }
    let holey = tree.file(ROOT, "holey", HOLEY as u64, Some(holey_fd));
    let short = tree.file(ROOT, "short", 1000, Some(memfd("short", &pattern(3, 1000))));
    let empty = tree.file(ROOT, "empty", 0, Some(memfd("empty", &[])));
    let trunc_fd = memfd("trunc", &pattern(4, TRUNC_HAS));
    let trunc = tree.file(ROOT, "trunc", TRUNC_SIZE as u64, Some(trunc_fd));
    let files = Files {
        big,
        holey,
        short,
        empty,
        trunc,
    };
    (Arc::new(tree), files)
}

/// One READ answered: what was asked, how it went, and the reader that read it.
#[derive(Clone)]
struct Entry {
    read: Read,
    answered: Answered,
    reader: String,
}

type Log = Arc<Mutex<Vec<Entry>>>;

/// A served mount of [`tree`] whose READs go through a reply path.
struct Rig {
    mounted: Mounted,
    served: Served,
    tree: Arc<TreeSpec>,
    files: Files,
    path: Arc<ReplyPath>,
    log: Log,
    arm: Arm,
    max_readahead: usize,
    max_pages: usize,
}

/// What one caller got and the READs that took.
struct Got {
    bytes: Vec<u8>,
    entries: Vec<Entry>,
}

/// Whether a reply of `len` bytes at `offset` fits a pipe of `size` bytes: one buffer for the
/// header and one per page the data touches.
fn fits(size: usize, offset: u64, len: usize) -> bool {
    let page = rustix::param::page_size();
    ((offset as usize % page) + len).div_ceil(page) < size / page
}

impl Rig {
    /// Mounts [`tree`] with `readers` readers, answering OPEN as `open`, replying as `arm`. `gate`
    /// holds every READ at a barrier before it is answered. `None` (said on stderr) where no mount
    /// can be made or the kernel did not grant a flag of `needs`.
    fn start(
        name: &str,
        readers: usize,
        arm: Arm,
        open: Open,
        needs: &[(InitFlags, &str)],
        gate: Option<Arc<Barrier>>,
    ) -> Option<Self> {
        let (mounted, session) = Mounted::new(name, readers)?;
        for (flag, what) in needs {
            if !session.negotiated().granted.contains(*flag) {
                let granted = session.negotiated().granted;
                skip(&format!("the kernel did not grant {what} ({granted:?})"));
                return None;
            }
        }
        let mut config = ReplyConfig::for_session(session.negotiated(), readers);
        if arm == Arm::Writev {
            config.splice_min = None;
        }
        let max_readahead = session.negotiated().reply.max_readahead as usize;
        let max_pages = usize::from(session.negotiated().reply.max_pages);
        let path = Arc::new(ReplyPath::new(config).unwrap());
        let (tree, files) = tree();
        let log = Log::default();
        let served = {
            let (path, log) = (path.clone(), log.clone());
            serve(
                session,
                tree.clone(),
                open,
                Opendir::Ok,
                move |tree, read, replier, events| {
                    if let Some(gate) = &gate {
                        gate.wait();
                    }
                    let reader = reader_name();
                    let answered = answer_read(&path, tree, read, replier, events);
                    log.lock().unwrap().push(Entry {
                        read,
                        answered,
                        reader,
                    });
                    events.update(|s| *s.counts.entry("answered").or_default() += 1);
                    Handled::Done
                },
            )
        };
        Some(Self {
            mounted,
            served,
            tree,
            files,
            path,
            log,
            arm,
            max_readahead,
            max_pages,
        })
    }

    /// Waits until every READ the file system received is answered and logged.
    fn settle(&self) {
        self.served.events.wait_for(|s| {
            let answered = s.counts.get("answered").copied().unwrap_or(0);
            (answered == s.reads.len()).then_some(())
        });
    }

    /// The entries logged from the `start`-th on.
    fn since(&self, start: usize) -> Vec<Entry> {
        self.settle();
        self.log.lock().unwrap()[start..].to_vec()
    }

    fn logged(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Whether a reply of `len` bytes at `offset` is spliced.
    fn spliced(&self, offset: u64, len: usize) -> bool {
        self.arm == Arm::Splice && len >= SPLICE_MIN && fits(self.path.pipe_size(), offset, len)
    }

    /// A caller `pread`s `len` bytes of `name` at `offset`.
    fn read(&self, name: &str, offset: u64, len: usize) -> Got {
        let start = self.logged();
        let caller = pread(self.mounted.path(name), offset, len);
        let bytes = caller.bytes();
        caller.join();
        Got {
            bytes,
            entries: self.since(start),
        }
    }

    /// [`Self::read`] into a page-aligned buffer, which a direct READ is split by.
    fn read_aligned(&self, name: &str, offset: u64, len: usize) -> Got {
        let start = self.logged();
        let path = self.mounted.path(name);
        let caller = thread::spawn(move || {
            let file = std::fs::File::open(path)?;
            let mut buf = Aligned::new(len);
            let n = file.read_at(buf.as_mut(), offset)?;
            Ok::<_, io::Error>(buf.as_mut()[..n].to_vec())
        });
        let bytes = caller.join().unwrap().unwrap();
        Got {
            bytes,
            entries: self.since(start),
        }
    }

    /// Checks `got`, the read of `len` bytes of file `ino` at `offset`: the bytes by hash against
    /// the tree spec's, and every READ answered whole, accepted, and sent the way the arm says.
    fn check(&self, ino: u64, offset: u64, len: usize, got: &Got) {
        let want = self.tree.bytes(ino, offset, len as u64);
        assert_eq!(
            got.bytes.len(),
            want.len(),
            "bytes returned for {offset}+{len}"
        );
        assert_eq!(sha256(&got.bytes), sha256(&want), "bytes of {offset}+{len}");
        let (size, _) = self.tree.data(ino).unwrap();
        for e in &got.entries {
            let have = size
                .saturating_sub(e.read.offset)
                .min(u64::from(e.read.size));
            assert_eq!(
                e.answered.len as u64, have,
                "length answered to {:?}",
                e.read
            );
            assert_eq!(e.answered.done, Done::Written, "{:?}", e.read);
            assert_eq!(
                e.answered.spliced,
                self.spliced(e.read.offset, e.answered.len),
                "the way {:?} went (pipe of {} bytes)",
                e.read,
                self.path.pipe_size()
            );
        }
    }

    /// Checks the reply path's counts against the log, then unmounts and ends the readers.
    fn finish(self) {
        let entries = self.since(0);
        let total: usize = entries.iter().map(|e| e.answered.len).sum();
        let spliced: usize = entries
            .iter()
            .filter(|e| e.answered.spliced)
            .map(|e| e.answered.len)
            .sum();
        let counts = self.path.counts();
        assert_eq!(counts.splice_bytes, spliced as u64, "bytes spliced");
        assert_eq!(
            counts.pread_bytes,
            (total - spliced) as u64,
            "bytes by pread"
        );
        assert_eq!(counts.splice_bytes + counts.pread_bytes, total as u64);
        if self.arm == Arm::Writev {
            assert_eq!(counts.splice_bytes, 0, "the writev arm splices nothing");
        }
        let Self {
            mounted, served, ..
        } = self;
        mounted.unmount();
        let ends = served.join();
        assert!(ends.iter().all(|e| *e == ReaderEnd::Unmounted), "{ends:?}");
    }
}

/// The `fs.fuse.max_pages_limit` sysctl: the cap the kernel puts on `max_pages`.
fn max_pages_limit() -> usize {
    std::fs::read_to_string("/proc/sys/fs/fuse/max_pages_limit")
        .map_or(256, |s| s.trim().parse().unwrap())
}

fn direct() -> Open {
    Open::Flags(FOPEN_DIRECT_IO)
}

/// The matrix of direct `pread`s of the big file on `arm`.
fn matrix(name: &str, arm: Arm) {
    let Some(rig) = Rig::start(name, 2, arm, direct(), &[], None) else {
        return;
    };
    let ino = rig.files.big;
    let size = BIG as u64;
    let cell = CELL as usize;
    // (what, offset, length)
    let cases: [(&str, u64, usize); 9] = [
        ("inside", 4096, 64 * KIB),
        (
            "spanning a cell boundary",
            (cell - 100 * KIB) as u64,
            256 * KIB,
        ),
        (
            "ending at a cell boundary",
            (2 * cell - 128 * KIB) as u64,
            128 * KIB,
        ),
        ("at EOF", size, 4096),
        ("straddling EOF", size - 1000, 64 * KIB),
        ("past EOF", size + 8192, 4096),
        ("below the threshold", 8192, SPLICE_MIN - 1),
        ("at the threshold", 8192, SPLICE_MIN),
        ("unaligned and long", 4095, 300 * KIB),
    ];
    for (what, offset, len) in cases {
        let got = rig.read("big", offset, len);
        rig.check(ino, offset, len, &got);
        let expect = (size.saturating_sub(offset) as usize).min(len);
        assert_eq!(got.bytes.len(), expect, "{what}: bytes returned");
    }

    // Zero length: the caller gets nothing and no READ reaches the server.
    let got = rig.read("big", 4096, 0);
    assert!(got.bytes.is_empty());
    assert!(got.entries.is_empty(), "READs for a zero-length pread");

    // 3 MiB at once into an aligned buffer: READs of the largest size, in order, covering it.
    let piece = rig.max_pages.min(max_pages_limit()) * 4096;
    let got = rig.read_aligned("big", 0, 3 * MIB);
    rig.check(ino, 0, 3 * MIB, &got);
    let reads: Vec<(u64, u32)> = got
        .entries
        .iter()
        .map(|e| (e.read.offset, e.read.size))
        .collect();
    let want: Vec<(u64, u32)> = (0..(3 * MIB).div_ceil(piece))
        .map(|i| {
            let from = i * piece;
            (from as u64, (3 * MIB - from).min(piece) as u32)
        })
        .collect();
    assert!(
        want.len() > 1,
        "a READ cannot be 3 MiB ({piece} bytes at most)"
    );
    assert_eq!(
        reads, want,
        "READs of at most {piece} bytes, in order (K21)"
    );
    rig.finish();
}

#[test]
fn direct_preads_of_every_range_shape_are_answered_byte_exact_by_splice() {
    matrix("t-f7-read-splice", Arm::Splice);
}

#[test]
fn direct_preads_of_every_range_shape_are_answered_byte_exact_by_writev() {
    matrix("t-f7-read-writev", Arm::Writev);
}

#[test]
fn ranges_over_holes_read_zeros_there_and_the_file_elsewhere() {
    let expected = holey_bytes();
    assert!(
        expected[..4096].iter().any(|b| *b != 0),
        "the model has data before its holes"
    );
    assert!(expected[HOLES[0].0..HOLES[0].1].iter().all(|b| *b == 0));
    for (name, arm) in [
        ("t-f7-holes-splice", Arm::Splice),
        ("t-f7-holes-writev", Arm::Writev),
    ] {
        let Some(rig) = Rig::start(name, 2, arm, direct(), &[], None) else {
            return;
        };
        let ino = rig.files.holey;
        // The whole file, a range that starts in the cell before the first hole and ends past it,
        // and one over the unaligned hole with data on both sides.
        let ranges = [
            (0usize, HOLEY),
            (MIB - 4096, MIB + 8192),
            (100 * KIB - 7, 256 * KIB + 9),
        ];
        for (offset, len) in ranges {
            let got = rig.read("holey", offset as u64, len);
            rig.check(ino, offset as u64, len, &got);
            let end = (offset + len).min(HOLEY);
            assert_eq!(
                sha256(&got.bytes),
                sha256(&expected[offset..end]),
                "{arm:?}: bytes of {offset}+{len}"
            );
            for (from, to) in HOLES {
                let (a, b) = (from.max(offset), to.min(end));
                if a < b {
                    let zeros = got.bytes[a - offset..b - offset].iter().all(|b| *b == 0);
                    assert!(zeros, "{arm:?}: the hole {from}..{to} reads zeros");
                }
            }
        }
        rig.finish();
    }
}

#[test]
fn a_short_file_and_an_empty_file_read_whole_return_their_bytes() {
    for (name, arm) in [
        ("t-f7-short-splice", Arm::Splice),
        ("t-f7-short-writev", Arm::Writev),
    ] {
        let Some(rig) = Rig::start(name, 2, arm, direct(), &[], None) else {
            return;
        };
        let got = rig.read("short", 0, MIB);
        rig.check(rig.files.short, 0, MIB, &got);
        assert_eq!(got.bytes, pattern(3, 1000), "{arm:?}: the short file whole");
        let got = rig.read("empty", 0, MIB);
        rig.check(rig.files.empty, 0, MIB, &got);
        assert!(
            got.bytes.is_empty(),
            "{arm:?}: the empty file returns nothing"
        );
        rig.finish();
    }
}

#[test]
fn a_reply_is_as_long_as_the_bytes_the_data_file_had() {
    let Some(rig) = Rig::start("t-f7-short-data", 2, Arm::Splice, direct(), &[], None) else {
        return;
    };
    if !fits(rig.path.pipe_size(), 0, TRUNC_SIZE) {
        skip(&format!(
            "the kernel granted a pipe of {} bytes, too small to take a 512 KiB reply",
            rig.path.pipe_size()
        ));
        return;
    }
    let got = rig.read("trunc", 0, TRUNC_SIZE);
    assert_eq!(got.entries.len(), 1, "one READ of the whole range");
    let e = &got.entries[0];
    assert_eq!((e.read.offset, e.read.size as usize), (0, TRUNC_SIZE));
    assert_eq!(
        e.answered.len, TRUNC_HAS,
        "the reply carries the bytes the data file had"
    );
    assert!(
        !e.answered.spliced,
        "the splice ended short: the reply is rebuilt by pread"
    );
    assert_eq!(
        e.answered.done,
        Done::Written,
        "the kernel took the reply (K15)"
    );
    assert_eq!(sha256(&got.bytes), sha256(&pattern(4, TRUNC_HAS)));
    assert_eq!(rig.path.counts().splice_bytes, 0);
    assert_eq!(rig.path.counts().pread_bytes, TRUNC_HAS as u64);
    assert_eq!(rig.tree.data(rig.files.trunc).unwrap().0, TRUNC_SIZE as u64);
    // `finish` would expect the length the tree gives; the counts were checked above.
    let Rig {
        mounted, served, ..
    } = rig;
    mounted.unmount();
    let ends = served.join();
    assert!(ends.iter().all(|e| *e == ReaderEnd::Unmounted), "{ends:?}");
}

#[test]
fn buffered_reads_of_a_mount_without_open_are_whole_and_no_larger_than_max_readahead() {
    let needs = [
        (InitFlags::ASYNC_READ, "ASYNC_READ"),
        (InitFlags::NO_OPEN_SUPPORT, "NO_OPEN_SUPPORT"),
    ];
    let Some(rig) = Rig::start("t-f7-buffered", 2, Arm::Splice, Open::Enosys, &needs, None) else {
        return;
    };
    let expected = [("big", pattern(1, BIG)), ("holey", holey_bytes())];
    for (name, want) in expected {
        let path: PathBuf = rig.mounted.path(name);
        let got = thread::spawn(move || std::fs::read(path))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), want.len(), "{name}: bytes read");
        assert_eq!(sha256(&got), sha256(&want), "{name}: bytes");
    }
    let entries = rig.since(0);
    assert!(
        entries.len() > 1,
        "a buffered read of 3 MiB is several READs"
    );
    for e in &entries {
        assert!(
            e.read.size as usize <= rig.max_readahead,
            "{:?} is larger than the negotiated max_readahead of {}",
            e.read,
            rig.max_readahead
        );
        assert_eq!(e.answered.done, Done::Written, "{:?}", e.read);
    }
    let covered = |ino: u64, size: usize| -> usize {
        let mut seen = HashSet::new();
        for e in entries.iter().filter(|e| e.read.ino == ino) {
            let end = (e.read.offset + u64::from(e.read.size)).min(size as u64);
            seen.extend((e.read.offset / 4096)..end.div_ceil(4096));
        }
        seen.len()
    };
    assert_eq!(
        covered(rig.files.big, BIG),
        BIG.div_ceil(4096),
        "every page of big was read"
    );
    assert_eq!(
        covered(rig.files.holey, HOLEY),
        HOLEY / 4096,
        "every page of holey was read"
    );
    let opens = rig.served.events.with(|s| s.count(op::OPEN));
    assert_eq!(opens, 1, "an OPEN answered ENOSYS is not sent again (K18)");
    rig.finish();
}

#[test]
fn spliced_replies_of_four_readers_answering_at_once_each_reach_their_caller() {
    const READERS: usize = 4;
    let gate = Arc::new(Barrier::new(READERS));
    let Some(rig) = Rig::start("t-f7-four", READERS, Arm::Splice, direct(), &[], Some(gate)) else {
        return;
    };
    let len = 256 * KIB;
    if !fits(rig.path.pipe_size(), 1000, len) {
        skip(&format!(
            "the kernel granted a pipe of {} bytes, too small to splice a 256 KiB reply",
            rig.path.pipe_size()
        ));
        return;
    }
    // Four callers, each at its own offset (not a page boundary: the data starts mid-page).
    let offsets: Vec<u64> = (0..READERS)
        .map(|i| (i * 512 * KIB) as u64 + 1000)
        .collect();
    let callers: Vec<_> = offsets
        .iter()
        .map(|offset| pread(rig.mounted.path("big"), *offset, len))
        .collect();
    let results: Vec<Vec<u8>> = callers.iter().map(conformance::Caller::bytes).collect();
    for caller in callers {
        caller.join();
    }
    let entries = rig.since(0);
    assert_eq!(entries.len(), READERS, "one READ per caller");
    let readers: HashSet<&str> = entries.iter().map(|e| e.reader.as_str()).collect();
    assert_eq!(
        readers.len(),
        READERS,
        "four different readers answered: {readers:?}"
    );
    for e in &entries {
        assert_eq!(
            e.answered.done,
            Done::Written,
            "{:?} on {}",
            e.read,
            e.reader
        );
        assert!(e.answered.spliced, "{:?} was spliced", e.read);
        assert_eq!(e.answered.len, len);
    }
    for (offset, got) in offsets.iter().zip(&results) {
        let want = rig.tree.bytes(rig.files.big, *offset, len as u64);
        assert_eq!(
            sha256(got),
            sha256(&want),
            "the bytes of the caller at {offset}"
        );
    }
    assert_eq!(rig.path.counts().splice_bytes, (READERS * len) as u64);
    rig.finish();
}
