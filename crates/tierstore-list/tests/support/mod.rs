//! What the tests of this crate share: a tree of files in packs whose bytes the test knows, its
//! version 2 list from tierstore's own writer, and its L2 sidecar from this crate's; an
//! independent SHA-256; and the CRC of a header a test has edited.

#![allow(
    dead_code,
    reason = "each test binary uses some of these helpers, never all"
)]
#![allow(
    clippy::unwrap_used,
    reason = "test helpers: a failed unwrap is the failure the test reports"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use sha2::{Digest as _, Sha256};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList};
use tierstore_list::{PackCells, PackInfo, sidecar_from_v2};

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

/// Sets the CRC-32C of the header of the L2 list `bytes` after a test edited it: the CRC of the
/// header's bytes (its length at bytes 16..20) with the CRC's own four bytes (188..192) as zero.
pub fn recrc(bytes: &mut [u8]) {
    let len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let mut header = bytes[..len].to_vec();
    header[188..192].fill(0);
    let crc = crc32c(&header);
    bytes[188..192].copy_from_slice(&crc.to_le_bytes());
}

/// The CRC-32C (Castagnoli) of `bytes`, bit by bit: independent of the `crc32c` crate.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Where section `index` (0 entries, 1 sha256, 2 packs, 3 cells, 4 keys, 5 names, 6 ranges,
/// 7 extensions) begins, and its count, as the header of `bytes` says.
pub fn section(bytes: &[u8], index: usize) -> (usize, u64) {
    let at = 56 + 16 * index;
    let offset = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
    let count = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
    (offset, count)
}
