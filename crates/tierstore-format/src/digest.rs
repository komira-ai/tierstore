//! The blake3 and crc32c helpers of the storage tier's formats (10.2, 10.3, 10.4).
//!
//! - An object's [`Generation`] is the first 16 bytes of the blake3 of its bytes (the 128-bit
//!   blake3: blake3's output is extendable, so its first 16 bytes are the 128-bit hash), computed
//!   while the bytes stream in ([`GenerationHasher`]).
//! - A [`KeyHash`] is the first 16 bytes of the blake3 of the key bytes; it names a large file.
//! - [`hash64`] is the `H` of the rendezvous scores of 10.2 and 10.4 (`H(K || node_id)`,
//!   `H(stripe_id || node_id)`, `H(stripe_id || drive_id)`): the blake3 of the parts one after the
//!   other, reduced to a `u64` by its first 8 bytes, little-endian. The caller picks the parts and
//!   their byte form; this function only fixes the hash and the reduction, so every node computes
//!   the same score.
//! - [`DataHasher`] computes, in one pass over bytes that stream through it, what a block trailer
//!   and a shard footer carry about a region: its length, the crc32c of each 1 MiB cell and the
//!   blake3 of the whole ([`DataDigest`]). It never holds the bytes.
//! - [`crc32c_append`](crate::crc32c_append) continues a crc32c, the running crc of a segment that
//!   the replication protocol carries (10.3).

use crate::ids::{Generation, KeyHash};
use crate::{CELL_BYTES, crc32c_append};

/// The first 16 bytes of a blake3 output.
fn first16(hash: &blake3::Hash) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&hash.as_bytes()[..16]);
    out
}

impl Generation {
    /// The generation of an object whose bytes are `bytes` (the first 16 bytes of their blake3).
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(first16(&blake3::hash(bytes)))
    }
}

impl KeyHash {
    /// The hash of `key` that names its large file (the first 16 bytes of the key's blake3).
    #[must_use]
    pub fn of(key: &[u8]) -> Self {
        Self(first16(&blake3::hash(key)))
    }
}

/// The generation of an object whose bytes arrive in pieces: [`GenerationHasher::update`] with
/// each piece, in order, then [`GenerationHasher::finish`]. The result equals
/// [`Generation::of`] of the concatenation, however the bytes were cut.
#[derive(Debug, Clone, Default)]
pub struct GenerationHasher {
    hasher: blake3::Hasher,
}

impl GenerationHasher {
    /// A hasher that has seen no byte.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the next bytes of the object.
    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    /// The generation of every byte added.
    #[must_use]
    pub fn finish(&self) -> Generation {
        Generation(first16(&self.hasher.finalize()))
    }
}

/// The blake3 (32 bytes) of `bytes`: the digest a trailer and a shard footer carry.
#[must_use]
pub fn blake3_256(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// The rendezvous hash `H` (10.2, 10.4): the blake3 of `parts` one after the other, its first 8
/// bytes read as a little-endian `u64`. The same parts give the same value on every node.
#[must_use]
pub fn hash64(parts: &[&[u8]]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    let mut first = [0u8; 8];
    first.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    u64::from_le_bytes(first)
}

/// What a trailer or a footer says about a region of bytes: its length, the crc32c of each 1 MiB
/// cell (the last may be short) and the blake3 of the whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDigest {
    /// The bytes of the region.
    pub len: u64,
    /// The crc32c of each 1 MiB cell, `cells_for(len)` of them.
    pub cell_crcs: Vec<u32>,
    /// The blake3 of the region.
    pub blake3: [u8; 32],
}

/// Computes a [`DataDigest`] in one pass over bytes that stream through it, in pieces of any size;
/// the result does not depend on where the pieces were cut.
#[derive(Debug, Clone, Default)]
pub struct DataHasher {
    hasher: blake3::Hasher,
    cells: Vec<u32>,
    /// The crc32c of the current cell so far.
    cell_crc: u32,
    /// The bytes of the current cell so far, below [`CELL_BYTES`].
    cell_fill: u64,
    len: u64,
}

impl DataHasher {
    /// A hasher that has seen no byte.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes seen so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether no byte has been seen.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adds the next bytes of the region.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
        while !bytes.is_empty() {
            // `CELL_BYTES - cell_fill` is at most 1 MiB, so it fits a `usize`.
            let room = (CELL_BYTES - self.cell_fill) as usize;
            let (now, rest) = bytes.split_at(room.min(bytes.len()));
            self.cell_crc = crc32c_append(self.cell_crc, now);
            self.cell_fill += now.len() as u64;
            if self.cell_fill == CELL_BYTES {
                self.cells.push(self.cell_crc);
                self.cell_crc = 0;
                self.cell_fill = 0;
            }
            bytes = rest;
        }
    }

    /// The digest of every byte added.
    #[must_use]
    pub fn finish(&self) -> DataDigest {
        let mut cell_crcs = self.cells.clone();
        if self.cell_fill > 0 {
            cell_crcs.push(self.cell_crc);
        }
        DataDigest {
            len: self.len,
            cell_crcs,
            blake3: *self.hasher.finalize().as_bytes(),
        }
    }
}
