//! `TreeFs` against the kernel: a tree served by [`tsfuse::Session::serve`] on a real FUSE mount,
//! called with `std::fs` from the test (the read path of `docs/design/fetch-cache-serve.md`, 5.1
//! to 5.3 and 4.6, whose opcodes `tree_fs.rs` documents). The tree is the rig of `tierstore-tree`:
//! the real mount server over the real entry store, with a fake engine whose waits park until the
//! test answers them, so every wait here is on a count (the engine's parked waits, a counter) and never
//! on a timer.
//!
//! - **Serving**: the root's and a subdirectory's names, the length, permission bits, file type,
//!   owner and link count of every node, a link's target, the bytes of every file (all cells
//!   published first), `ENOENT` for a name that is not there and `ENOTDIR` below a file, and
//!   `statfs`.
//! - **No open**: the kernel sends one OPEN and one OPENDIR, each answered `ENOSYS`, and none after
//!   (K18 of `docs/design/tsfuse-kernel-findings.md`): any number of opens of the same and of other
//!   files and directories leave `opens_refused` at 2, and the reads and listings still work.
//! - **A parked read**: a file whose cell is absent is read by a thread; it returns once the cell
//!   is published and the wait answered `Ready`, with the file's bytes, and the read is counted
//!   as answered from the parker; two reads parked on two packs are each answered by their own
//!   pack's answer.
//! - **The lost path**: a parked read answered `Lost` fails with `EIO`; then `statfs` and the
//!   listing of the root and of a directory fail with `EIO` and so does a read of another file,
//!   while a lookup, the attributes and a link's target still answer from the image.
//!
//! Catches: a `TreeFs` that answers OPEN or OPENDIR with success (the kernel would send more of
//! them and a RELEASE for each), or that counts a refusal twice; a parked read whose reply is lost
//! or goes to the wrong request; a lost tree that keeps answering `STATFS` or `READDIR`, or one
//! that stops answering `LOOKUP`; an inode, size, mode or link count that differs from the image;
//! a listing that differs from the tree's names or a cut-off one.
//!
//! Where no FUSE mount can be made (no `/dev/fuse`, or no `fusermount3`) a test says why on stderr
//! and ends, as the other mount tests of `tierstore fs` do (`with_fuse`).

use std::collections::BTreeSet;
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::ids::CellRun;
use tierstore_list::{PackCells, PackInfo, sidecar_from_v2};
use tierstore_testkit::dir::TestDir;
use tierstore_tree::testing::Rig;
use tierstore_tree::{ImageOptions, Tree};
use tsfuse::abi::InitConfig;
use tsfuse::mount::{Mount, MountOptions, Mounter};
use tsfuse::{ReplyConfig, ReplyPath, Serving, Session};

use super::{TreeFs, TreeFsStats};
use crate::server::treefs::testing::{until, with_fuse};
use crate::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};

/// One MiB, the size of a cell.
const MIB: u64 = 1 << 20;

/// `ENOENT`, `EIO` and `ENOTDIR`.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOTDIR: i32 = 20;

/// The readers (descriptors) the session is served with: the mount's and one clone.
const READERS: usize = 2;

/// A file of the tree: its path, mode, the pack it is in (by key) and its range there.
struct File {
    path: &'static str,
    mode: u16,
    pack: &'static str,
    offset: u64,
    len: u64,
}

/// The files that have bytes. Pack `out/a/0` is 2.5 MiB (three cells, the last short) and its
/// files cover it with a gap; pack `out/b/0` is 100 bytes, one cell, covered whole.
const FILES: [File; 4] = [
    File {
        path: "bin/cc",
        mode: 0o755,
        pack: "out/a/0",
        offset: 0,
        len: MIB + 10,
    },
    File {
        path: "bin/ld",
        mode: 0o750,
        pack: "out/a/0",
        offset: MIB + 4096,
        len: MIB,
    },
    File {
        path: "lib/a.txt",
        mode: 0o644,
        pack: "out/b/0",
        offset: 0,
        len: 60,
    },
    File {
        path: "lib/b.txt",
        mode: 0o640,
        pack: "out/b/0",
        offset: 60,
        len: 40,
    },
];

/// The pack `key` as the rig numbers them (by key), and its length.
const PACKS: [(&str, usize); 2] = [("out/a/0", (2 * MIB + MIB / 2) as usize), ("out/b/0", 100)];

/// The rig's number of pack `key`.
fn pack_number(key: &str) -> u32 {
    u32::try_from(PACKS.iter().position(|(k, _)| *k == key).unwrap()).unwrap()
}

/// Bytes that differ per pack and per offset.
fn pack_bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            (at as u64)
                .wrapping_mul(31)
                .wrapping_add(u64::from(seed) * 7) as u8
        })
        .collect()
}

/// The version 2 list of the tree and its L2 sidecar, and the packs' bytes in the rig's order.
fn tree_lists() -> (Vec<u8>, Vec<Vec<u8>>) {
    let packs: Vec<Vec<u8>> = PACKS
        .iter()
        .zip(1u8..)
        .map(|((_, len), seed)| pack_bytes(seed, *len))
        .collect();
    let dir = |path: &str, mode: u16| ListEntry {
        path: path.to_owned(),
        kind: ListKind::Dir {
            mode: Mode::new(mode).unwrap(),
        },
    };
    let mut entries = vec![dir("bin", 0o755), dir("lib", 0o755), dir("tmp", 0o700)];
    for file in &FILES {
        let bytes = &packs[pack_number(file.pack) as usize]
            [file.offset as usize..(file.offset + file.len) as usize];
        entries.push(ListEntry {
            path: file.path.to_owned(),
            kind: ListKind::File {
                mode: Mode::new(file.mode).unwrap(),
                size: file.len,
                source: Some(BlobSource {
                    key: Arc::from(file.pack),
                    offset: file.offset,
                }),
                sha256: Some(TreeDigest::of(bytes)),
            },
        });
    }
    entries.push(ListEntry {
        path: "lib/empty".to_owned(),
        kind: ListKind::File {
            mode: Mode::new(0o644).unwrap(),
            size: 0,
            source: None,
            sha256: Some(TreeDigest::of(b"")),
        },
    });
    entries.push(ListEntry {
        path: "lib/self".to_owned(),
        kind: ListKind::Symlink {
            target: "a.txt".to_owned(),
        },
    });
    let v2 = TreeList { entries }.encode_v2().unwrap();
    let l2 = sidecar_from_v2(&v2, |key| {
        let index = PACKS.iter().position(|(k, _)| *k == key)?;
        Some(PackInfo {
            cells: PackCells::of(&packs[index]),
            upload_part_size: 0,
            shared: false,
        })
    })
    .unwrap();
    (l2, packs)
}

/// The mount, unmounted when it drops: aborted first (as root; the abort file is root's) so that a
/// caller still blocked ends with `ECONNABORTED`, then unmounted lazily.
struct Unmount(Option<Mount>);

impl Drop for Unmount {
    fn drop(&mut self) {
        if let Some(mount) = self.0.take() {
            let _ = mount.abort();
            let _ = mount.unmount(true);
        }
    }
}

/// A mount of the rig's tree, served by `TreeFs`. Dropping it stops the readers (every descriptor
/// of the connection closes, and the kernel ends a request still waiting), unmounts, and drops
/// the rig last.
struct Mounted {
    serving: Option<Serving>,
    _unmount: Unmount,
    fs: Arc<TreeFs>,
    tree: Arc<Tree>,
    mp: PathBuf,
    rig: Rig,
    _dir: TestDir,
}

impl Mounted {
    /// Mounts the tree in a scratch directory named after `name`. With `publish`, every cell of
    /// every pack is present.
    fn new(name: &str, publish: bool) -> Self {
        let scratch = std::env::temp_dir();
        let (l2, packs) = tree_lists();
        let rig = Rig::new(&scratch, name, l2, packs);
        // Opening the tree opens every pack, which publishing needs.
        let tree = Arc::new(rig.tree(ImageOptions::default()));
        if publish {
            rig.publish_all();
        }
        let dir = TestDir::under(&scratch, name);
        let mp = dir.path().join("mnt");
        std::fs::create_dir(&mp).unwrap();
        let options = MountOptions {
            allow_other: false,
            ..MountOptions::tree("tree-fs-kernel")
        };
        let (device, mount) = Mounter::default().mount(&mp, &options).unwrap();
        let unmount = Unmount(Some(mount));
        let mut session = Session::init(device, &InitConfig::tree()).unwrap();
        session
            .clone_readers(NonZeroUsize::new(READERS).unwrap())
            .unwrap();
        let reply = Arc::new(
            ReplyPath::new(ReplyConfig::for_session(session.negotiated(), READERS)).unwrap(),
        );
        let fs = Arc::new(TreeFs::new(
            tree.clone(),
            reply,
            rig.runtime().handle().clone(),
            (
                rustix::process::geteuid().as_raw(),
                rustix::process::getegid().as_raw(),
            ),
        ));
        let serving = Some(session.serve(fs.clone()).unwrap());
        Self {
            serving,
            _unmount: unmount,
            fs,
            tree,
            mp,
            rig,
            _dir: dir,
        }
    }

    /// The path of `name` in the mount.
    fn at(&self, name: &str) -> PathBuf {
        self.mp.join(name)
    }

    /// The counts of the mount's answers.
    fn stats(&self) -> TreeFsStats {
        self.fs.stats()
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            serving.stopper().stop();
        }
    }
}

/// Waits until `n` waits are parked on pack `pack`.
fn parked(mounted: &Mounted, pack: u32, n: usize) {
    until(|| mounted.rig.parked(pack) >= n);
    assert_eq!(mounted.rig.parked(pack), n);
}

/// The names in directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    let listed: BTreeSet<String> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    listed.into_iter().collect()
}

/// The errno of a listing of `path`: of opening it or of reading its entries.
fn listing_errno(path: &Path) -> Option<i32> {
    std::fs::read_dir(path)
        .and_then(|dir| dir.collect::<Result<Vec<_>, _>>())
        .err()
        .and_then(|e| e.raw_os_error())
}

/// The bytes of the file `file` as the pack holds them.
fn expected(mounted: &Mounted, file: &File) -> Vec<u8> {
    let bytes = mounted.rig.bytes(pack_number(file.pack));
    bytes[file.offset as usize..(file.offset + file.len) as usize].to_vec()
}

/// Asserts that the mount lists the tree's names, in the root and below it.
#[test]
fn listings_of_the_root_and_of_a_directory_are_the_trees_names() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-listing", true);
        assert_eq!(names(&mounted.mp), ["bin", "lib", "tmp"]);
        assert_eq!(names(&mounted.at("bin")), ["cc", "ld"]);
        assert_eq!(
            names(&mounted.at("lib")),
            ["a.txt", "b.txt", "empty", "self"]
        );
        assert_eq!(names(&mounted.at("tmp")), Vec::<String>::new());
    });
}

/// Asserts that length, permission bits, file type, owner and link count of every node are the
/// tree's, and that a link's target is read back.
#[test]
fn metadata_of_every_node_is_the_trees() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-metadata", true);
        let me = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        for file in &FILES {
            let meta = std::fs::metadata(mounted.at(file.path)).unwrap();
            assert!(meta.is_file(), "{}", file.path);
            assert_eq!(meta.len(), file.len, "{}", file.path);
            assert_eq!(meta.permissions().mode() & 0o7777, u32::from(file.mode));
            assert_eq!((meta.uid(), meta.gid()), me);
            assert_eq!(meta.nlink(), 1);
        }
        let empty = std::fs::metadata(mounted.at("lib/empty")).unwrap();
        assert!(empty.is_file());
        assert_eq!(empty.len(), 0);
        for (dir, mode, nlink) in [("bin", 0o755, 2), ("lib", 0o755, 2), ("tmp", 0o700, 2)] {
            let meta = std::fs::metadata(mounted.at(dir)).unwrap();
            assert!(meta.is_dir(), "{dir}");
            assert_eq!(meta.permissions().mode() & 0o7777, mode, "{dir}");
            assert_eq!(meta.nlink(), nlink, "{dir}");
        }
        // The root: two links and one for each of `bin`, `lib` and `tmp`.
        let root = std::fs::metadata(&mounted.mp).unwrap();
        assert!(root.is_dir());
        assert_eq!(root.nlink(), 5);
        let link = std::fs::symlink_metadata(mounted.at("lib/self")).unwrap();
        assert!(link.file_type().is_symlink());
        assert!(!link.file_type().is_fifo());
        assert_eq!(link.len(), "a.txt".len() as u64);
        assert_eq!(
            std::fs::read_link(mounted.at("lib/self")).unwrap(),
            Path::new("a.txt")
        );
        // Following the link reads the file it names.
        let a = FILES.iter().find(|f| f.path == "lib/a.txt").unwrap();
        assert_eq!(
            std::fs::read(mounted.at("lib/self")).unwrap(),
            expected(&mounted, a)
        );
    });
}

/// Asserts that the bytes of every file through the mount are the pack's, including a file that
/// spans cells and one that starts inside a cell.
#[test]
fn every_file_reads_as_its_pack_bytes() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-read", true);
        for file in &FILES {
            let read = std::fs::read(mounted.at(file.path)).unwrap();
            assert!(read == expected(&mounted, file), "{}", file.path);
        }
        assert_eq!(std::fs::read(mounted.at("lib/empty")).unwrap(), b"");
    });
}

/// Asserts that a name that is not there is `ENOENT`, a name below a file is `ENOTDIR`, a read of
/// a directory fails, and that `statfs` answers with the tree mount's terms.
#[test]
fn a_missing_name_is_not_found_and_statfs_answers() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-missing", true);
        for path in ["nothing", "bin/nothing", "lib/a.txt.bak"] {
            let error = std::fs::metadata(mounted.at(path)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "{path}");
            assert_eq!(error.raw_os_error(), Some(ENOENT), "{path}");
        }
        let below_file = std::fs::metadata(mounted.at("lib/a.txt/x")).unwrap_err();
        assert_eq!(below_file.raw_os_error(), Some(ENOTDIR));
        let a_directory = std::fs::read(mounted.at("bin")).unwrap_err();
        assert_eq!(a_directory.raw_os_error(), Some(21), "EISDIR");
        let st = rustix::fs::statfs(&mounted.mp).unwrap();
        assert_eq!(u64::try_from(st.f_bsize).unwrap(), 512);
        assert_eq!(u64::try_from(st.f_namelen).unwrap(), 255);
        assert_eq!(st.f_blocks, 0);
    });
}

/// Asserts that the kernel sends one OPEN and one OPENDIR, each refused with `ENOSYS`, and none
/// after: many opens of the same file and of other files and directories leave the count at 2,
/// and every open, read and listing still works.
#[test]
fn the_kernel_stops_sending_open_and_opendir_after_one_refusal_each() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-no-open", true);
        assert_eq!(mounted.stats().opens_refused, 0);
        let a = FILES.iter().find(|f| f.path == "lib/a.txt").unwrap();
        let b = FILES.iter().find(|f| f.path == "lib/b.txt").unwrap();
        let first = std::fs::File::open(mounted.at(a.path)).unwrap();
        assert_eq!(mounted.stats().opens_refused, 1, "the first OPEN");
        // More opens, of the same file while it is held open, again, and of another file.
        let second = std::fs::File::open(mounted.at(a.path)).unwrap();
        let third = std::fs::File::open(mounted.at(b.path)).unwrap();
        for _ in 0..3 {
            drop(std::fs::File::open(mounted.at(a.path)).unwrap());
        }
        assert_eq!(
            mounted.stats().opens_refused,
            1,
            "no OPEN after the first refusal, of this file or another"
        );
        let mut held = Vec::new();
        std::io::Read::read_to_end(&mut &first, &mut held).unwrap();
        assert_eq!(held, expected(&mounted, a));
        drop((first, second, third));
        assert_eq!(
            std::fs::read(mounted.at(a.path)).unwrap(),
            expected(&mounted, a)
        );
        assert_eq!(
            std::fs::read(mounted.at(b.path)).unwrap(),
            expected(&mounted, b)
        );
        assert_eq!(mounted.stats().opens_refused, 1, "reads open files too");

        // Directories: the first listing's OPENDIR is refused, no later one is sent.
        assert_eq!(names(&mounted.mp), ["bin", "lib", "tmp"]);
        assert_eq!(mounted.stats().opens_refused, 2, "the first OPENDIR");
        assert_eq!(names(&mounted.at("lib")).len(), 4);
        assert_eq!(names(&mounted.at("bin")), ["cc", "ld"]);
        assert_eq!(names(&mounted.mp), ["bin", "lib", "tmp"]);
        assert_eq!(
            mounted.stats().opens_refused,
            2,
            "no OPENDIR after the first refusal, of this directory or another"
        );
        assert_eq!(mounted.stats().parked_answered, 0);
    });
}

/// Asserts that a read of a file whose cell is absent is parked, not answered, and that it
/// returns the file's bytes once the cell is published and the wait answered ready.
#[test]
fn a_read_of_an_absent_cell_returns_the_bytes_once_the_cell_is_published() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-parked", false);
        let a = FILES.iter().find(|f| f.path == "lib/a.txt").unwrap();
        let pack = pack_number(a.pack);
        let path = mounted.at(a.path);
        let reader = std::thread::spawn(move || std::fs::read(path));
        parked(&mounted, pack, 1);
        assert_eq!(mounted.stats().parked_answered, 0);
        mounted.rig.publish(pack, CellRun { first: 0, count: 1 });
        assert_eq!(mounted.rig.answer(pack, WaitOutcome::Ready), 1);
        assert_eq!(reader.join().unwrap().unwrap(), expected(&mounted, a));
        until(|| mounted.stats().parked_answered == 1);
        assert!(!mounted.tree.is_lost());
    });
}

/// Asserts that two reads parked on two packs are each answered with their own file's bytes, by
/// the answer to their own pack's wait.
#[test]
fn two_parked_reads_on_two_packs_are_each_answered_with_their_own_bytes() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-two-parked", false);
        let a = FILES.iter().find(|f| f.path == "lib/a.txt").unwrap();
        let ld = FILES.iter().find(|f| f.path == "bin/ld").unwrap();
        let (pack_a, pack_ld) = (pack_number(a.pack), pack_number(ld.pack));
        assert_ne!(pack_a, pack_ld);
        let (path_a, path_ld) = (mounted.at(a.path), mounted.at(ld.path));
        let read_a = std::thread::spawn(move || std::fs::read(path_a));
        // The first ten bytes of `ld`: one cell of its pack.
        let read_ld = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let file = std::fs::File::open(path_ld)?;
            let mut buf = [0u8; 10];
            std::os::unix::fs::FileExt::read_exact_at(&file, &mut buf, 0)?;
            Ok(buf.to_vec())
        });
        parked(&mounted, pack_a, 1);
        parked(&mounted, pack_ld, 1);
        // Answering the other pack's wait leaves this one's reader parked.
        mounted.rig.publish(pack_a, CellRun { first: 0, count: 1 });
        assert_eq!(mounted.rig.answer(pack_a, WaitOutcome::Ready), 1);
        assert_eq!(read_a.join().unwrap().unwrap(), expected(&mounted, a));
        assert_eq!(mounted.rig.parked(pack_ld), 1);
        // `ld` starts 4096 bytes into cell 1 of its pack.
        mounted.rig.publish(pack_ld, CellRun { first: 1, count: 1 });
        assert_eq!(mounted.rig.answer(pack_ld, WaitOutcome::Ready), 1);
        assert_eq!(
            read_ld.join().unwrap().unwrap(),
            expected(&mounted, ld)[..10]
        );
        until(|| mounted.stats().parked_answered == 2);
    });
}

/// Asserts that a parked read answered `Lost` fails with `EIO`, that `statfs` and the listings of
/// the root and of a directory then fail with `EIO`, and so does a read of another file, while a
/// lookup of a name not yet looked up, the attributes and a link's target still answer.
#[test]
fn a_lost_wait_fails_the_read_with_eio_and_then_statfs_and_listings_and_reads_too() {
    with_fuse(|| {
        let mounted = Mounted::new("treefs-kernel-lost", false);
        let a = FILES.iter().find(|f| f.path == "lib/a.txt").unwrap();
        let pack = pack_number(a.pack);
        // Before: the mount lists and answers statfs.
        assert_eq!(names(&mounted.at("tmp")), Vec::<String>::new());
        rustix::fs::statfs(&mounted.mp).unwrap();
        let path = mounted.at(a.path);
        let reader = std::thread::spawn(move || std::fs::read(path));
        parked(&mounted, pack, 1);
        assert_eq!(mounted.rig.answer(pack, WaitOutcome::Lost), 1);
        let failed = reader.join().unwrap().unwrap_err();
        assert_eq!(failed.raw_os_error(), Some(EIO));
        assert!(mounted.tree.is_lost());
        until(|| mounted.stats().parked_answered == 1);

        let statfs = rustix::fs::statfs(&mounted.mp).unwrap_err();
        assert_eq!(statfs.raw_os_error(), EIO);
        assert_eq!(listing_errno(&mounted.mp), Some(EIO));
        assert_eq!(listing_errno(&mounted.at("lib")), Some(EIO));
        let b = FILES.iter().find(|f| f.path == "lib/b.txt").unwrap();
        // A name not looked up before: the kernel asks LOOKUP, which the image still answers.
        let meta = std::fs::metadata(mounted.at(b.path)).unwrap();
        assert_eq!(meta.len(), b.len);
        assert_eq!(
            std::fs::read_link(mounted.at("lib/self")).unwrap(),
            Path::new("a.txt")
        );
        let read = std::fs::read(mounted.at(b.path)).unwrap_err();
        assert_eq!(read.raw_os_error(), Some(EIO));
    });
}
