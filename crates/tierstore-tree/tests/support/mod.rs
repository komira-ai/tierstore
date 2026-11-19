//! What the tests of this crate share: trees of files in packs whose bytes the test knows (the
//! version 2 list from tierstore's own writer, the L2 sidecar from `tierstore-list`), a stage
//! that reads into a `Vec` and records what became of its bytes, and helpers that say what a
//! read ended in.

#![allow(
    dead_code,
    reason = "each test binary uses some of these helpers, never all"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers: a failed unwrap or a panic is the failure the test reports"
)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_list::{PackCells, PackInfo, sidecar_from_v2};
use tierstore_tree::{Image, ReadFailure, ReadStep, Stage, Staged, Wait};

/// One mebibyte: the size of a cell.
pub const MIB: u64 = 1 << 20;

/// The SHA-256 of `bytes`, by the `sha2` crate: not the `ring` the crate under test uses.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// `len` bytes that differ for each `seed` and at each offset.
pub fn pack_bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            (at as u64)
                .wrapping_mul(31)
                .wrapping_add(u64::from(seed) * 7) as u8
        })
        .collect()
}

/// A tree being made: its entries, and the bytes of each pack its files are in.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub entries: Vec<ListEntry>,
    pub packs: BTreeMap<String, Vec<u8>>,
}

impl Tree {
    /// Adds a pack of `bytes` under `key`.
    pub fn pack(mut self, key: &str, bytes: Vec<u8>) -> Self {
        self.packs.insert(key.to_owned(), bytes);
        self
    }

    /// Adds a file at `path` whose bytes are `len` bytes at `offset` of pack `key`, its digest
    /// that of those bytes. The pack must be added first.
    pub fn file(mut self, path: &str, key: &str, offset: u64, len: u64) -> Self {
        let bytes = &self.packs[key][offset as usize..(offset + len) as usize];
        let sha = sha256(bytes);
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::File {
                mode: Mode::FILE,
                size: len,
                source: (len > 0).then(|| BlobSource {
                    key: Arc::from(key),
                    offset,
                }),
                sha256: Some(TreeDigest::from_bytes(sha)),
            },
        });
        self
    }

    /// Adds an empty file at `path`.
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

    /// Adds a link at `path` to `target`.
    pub fn link(mut self, path: &str, target: &str) -> Self {
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::Symlink {
                target: target.to_owned(),
            },
        });
        self
    }

    /// Adds a directory at `path` of mode `mode`.
    pub fn dir(mut self, path: &str, mode: u16) -> Self {
        self.entries.push(ListEntry {
            path: path.to_owned(),
            kind: ListKind::Dir {
                mode: Mode::new(mode).unwrap(),
            },
        });
        self
    }

    /// The version 2 list of the tree, by tierstore's writer.
    pub fn v2(&self) -> Vec<u8> {
        TreeList {
            entries: self.entries.clone(),
        }
        .encode_v2()
        .unwrap()
    }

    /// The `PackInfo` of pack `key`: its cells, no parts, not shared.
    pub fn info(&self, key: &str) -> Option<PackInfo> {
        self.packs.get(key).map(|bytes| PackInfo {
            cells: PackCells::of(bytes),
            upload_part_size: 0,
            shared: false,
        })
    }

    /// The packs' bytes in the order of their numbers in the list (the keys, sorted bytewise).
    pub fn pack_list(&self) -> Vec<Vec<u8>> {
        self.packs.values().cloned().collect()
    }

    /// The version 2 list and its L2 sidecar.
    pub fn lists(&self) -> (Vec<u8>, Vec<u8>) {
        let v2 = self.v2();
        let l2 = sidecar_from_v2(&v2, |key| self.info(key)).unwrap();
        (v2, l2)
    }
}

/// A small tree of two packs: one of 2.5 MiB (three cells, the last short) whose files cover it
/// with a gap, one of 100 bytes covered whole; a directory of its own mode, an empty file, a link.
pub fn small_tree() -> Tree {
    const MIB: u64 = 1 << 20;
    Tree::default()
        .pack("out/a/0", pack_bytes(1, (2 * MIB + MIB / 2) as usize))
        .pack("out/b/0", pack_bytes(2, 100))
        .file("bin/cc", "out/a/0", 0, MIB + 10)
        .file("bin/ld", "out/a/0", MIB + 4096, MIB)
        .file("lib/a.txt", "out/b/0", 0, 60)
        .file("lib/b.txt", "out/b/0", 60, 40)
        .empty_file("lib/empty")
        .link("lib/self", "a.txt")
        .dir("tmp", 0o1777)
}

/// A tree for the image tests: nested directories, a name of the longest length, a directory of
/// many children, besides what `small_tree` has.
pub fn rich_tree() -> Tree {
    let long = "n".repeat(255);
    let mut tree = small_tree()
        .pack("out/c/0", pack_bytes(3, 5000))
        .file("deep/one/two/three.bin", "out/c/0", 100, 900)
        .file(&format!("deep/{long}"), "out/c/0", 1000, 50)
        .link("deep/one/up", "../../lib/a.txt")
        .link("deep/one/two/far", &"x".repeat(300))
        .dir("deep/one/two/empty-dir", 0o700);
    for n in 0..20u64 {
        tree = tree.file(&format!("many/f{n:02}"), "out/c/0", 2000 + n * 10, 10);
    }
    tree
}

/// The path of the image file written for `l2` in `dir`.
pub fn write_image(dir: &Path, l2: &[u8]) -> PathBuf {
    let path = dir.join("tree.l2");
    std::fs::write(&path, l2).unwrap();
    path
}

/// The tree id of the L2 list `l2`.
pub fn tree_id(l2: &[u8]) -> [u8; 32] {
    *tierstore_list::TreeListL2::open(l2).unwrap().tree_id()
}

/// The image of `l2`, written to `dir` and opened.
pub fn open_image(dir: &Path, l2: &[u8]) -> Image {
    let path = write_image(dir, l2);
    Image::open(File::open(path).unwrap(), &tree_id(l2), Default::default()).unwrap()
}

/// The entry at `path` ("lib/a.txt") of `image`, by lookups from the root.
pub fn find(image: &Image, path: &str) -> NodeIndex {
    path.split('/').fold(NodeIndex(0), |dir, name| {
        image
            .lookup(dir, name.as_bytes())
            .unwrap_or_else(|| panic!("{path}: no {name}"))
    })
}

/// Where section `index` (0 entries, 5 names) begins, and its count, as the header of `bytes`
/// says.
pub fn section(bytes: &[u8], index: usize) -> (u64, u64) {
    let at = 56 + 16 * index;
    (
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()),
        u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()),
    )
}

/// Writes `bytes` at `at` of the file at `path`, in place (a file changed after it was mapped).
pub fn patch(path: &Path, at: u64, bytes: &[u8]) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(bytes, at).unwrap();
}

/// What happened to the bytes a stage read, in order.
#[derive(Clone, Debug, Default)]
pub struct Events(Arc<Mutex<Vec<&'static str>>>);

impl Events {
    /// The events so far.
    pub fn all(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().clone()
    }

    fn push(&self, event: &'static str) {
        self.0.lock().unwrap().push(event);
    }
}

/// A stage that reads the bytes into a `Vec`, records `fill`, `commit` and `discard` in its
/// events, can run a closure while it fills (a cell changing under the read), and can fail.
pub struct VecStage<'a> {
    events: Events,
    during_fill: Option<&'a dyn Fn()>,
    fail: bool,
}

impl<'a> VecStage<'a> {
    pub fn new(events: &Events) -> Self {
        Self {
            events: events.clone(),
            during_fill: None,
            fail: false,
        }
    }

    pub fn during_fill(events: &Events, hook: &'a dyn Fn()) -> Self {
        Self {
            during_fill: Some(hook),
            ..Self::new(events)
        }
    }

    pub fn failing(events: &Events) -> Self {
        Self {
            fail: true,
            ..Self::new(events)
        }
    }
}

/// Bytes a [`VecStage`] read.
pub struct Held<'a> {
    stage: VecStage<'a>,
    bytes: Vec<u8>,
}

impl<'a> Stage for VecStage<'a> {
    type Staged = Held<'a>;
    type Error = io::Error;

    fn fill(self, data: BorrowedFd<'_>, offset: u64, len: usize) -> io::Result<Held<'a>> {
        if let Some(hook) = self.during_fill {
            hook();
        }
        if self.fail {
            return Err(io::Error::other("fill failed"));
        }
        let file = File::from(data.try_clone_to_owned()?);
        let mut bytes = vec![0u8; len];
        file.read_exact_at(&mut bytes, offset)?;
        self.events.push("fill");
        Ok(Held { stage: self, bytes })
    }
}

impl<'a> Staged for Held<'a> {
    type Back = VecStage<'a>;
    type Committed = Vec<u8>;

    fn commit(self) -> Vec<u8> {
        self.stage.events.push("commit");
        self.bytes
    }

    fn discard(self) -> VecStage<'a> {
        self.stage.events.push("discard");
        self.stage
    }
}

/// What a read ended in, as a word.
pub fn word<S: Stage>(step: &ReadStep<S>) -> &'static str {
    match step {
        ReadStep::Served(_) => "served",
        ReadStep::Empty(_) => "empty",
        ReadStep::Park(..) => "park",
        ReadStep::Failed(..) => "failed",
        ReadStep::FillFailed(_) => "fill-failed",
    }
}

/// The bytes a read served.
pub fn served(step: ReadStep<VecStage<'_>>) -> Vec<u8> {
    match step {
        ReadStep::Served(bytes) => bytes,
        other => panic!("expected served, got {}", word(&other)),
    }
}

/// What a read that parked waits for.
pub fn parked(step: ReadStep<VecStage<'_>>) -> Wait {
    match step {
        ReadStep::Park(_, wait) => wait,
        other => panic!("expected park, got {}", word(&other)),
    }
}

/// Why a read failed.
pub fn failed(step: ReadStep<VecStage<'_>>) -> ReadFailure {
    match step {
        ReadStep::Failed(_, why) => why,
        other => panic!("expected failed, got {}", word(&other)),
    }
}

/// Asserts a read ended empty.
pub fn empty(step: ReadStep<VecStage<'_>>) {
    assert_eq!("empty", word(&step));
}

/// The error of a read whose stage could not fill.
pub fn fill_failed(step: ReadStep<VecStage<'_>>) -> io::Error {
    match step {
        ReadStep::FillFailed(error) => error,
        other => panic!("expected fill-failed, got {}", word(&other)),
    }
}
