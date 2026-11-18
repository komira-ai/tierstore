//! The formats of the tierstore storage tier, as section 10 of
//! `docs/design/fetch-cache-serve.md` (revision 7) states them, frozen by the interface-freeze
//! change (section 18, row T-I). Every other task of the storage tier writes and reads bytes
//! through these types; a change to a layout here is a change to a frozen format and says so.
//!
//! What is here, by module:
//! - [`ids`]: node, block, stripe, drive and cluster ids, the generation (the 128-bit blake3 of
//!   an object's bytes, its ETag), the key hash and the expiry class (10.2).
//! - [`stamp`]: the cluster stamp, a hybrid logical clock value of 48 bits of wall-clock
//!   milliseconds and 16 bits of counter (10.2).
//! - [`hlc`]: the hybrid logical clock that assigns stamps, advanced on every local event and
//!   merged with the stamp of every peer message, on an injected wall clock (10.2).
//! - [`alloc`]: the per-node allocation of block and stripe ids, one at a time or as a run of
//!   consecutive ids for a chain (10.2).
//! - [`digest`]: the blake3 helpers (an object's generation, a key hash, the rendezvous hash `H`,
//!   a region's cell crc table and blake3 in one pass) beside the crc32c helpers of this module
//!   (10.2, 10.3, 10.4).
//! - [`record`]: the 64-byte record header, the record framing of a segment and the scan that
//!   stops at the last valid record (10.2).
//! - [`tomb`]: the tombstone log of `tomb/<origin>/<first_seq>.log`: tombstones, block drops and
//!   tomb drops, each a [`tomb::LogRecord`] framed with its origin, its position in the origin's
//!   log and a crc; a kind this release does not know is kept and skipped, never refused (10.2,
//!   10.14).
//! - [`trailer`]: the block trailer (sorted entries with stamps, a crc32c per 1 MiB cell, the
//!   block's blake3), which is also the content of `seg/<block_id>.idx` and `blk/<block_id>.idx`
//!   (10.2).
//! - [`block`]: the geometry of a block (`block_bytes`, `block_reserve`), the building of a
//!   segment, pack block or chain slice and its trailer from the block's bytes, and chains
//!   (10.2, 10.5).
//! - [`footer`]: the footer of a cold shard file or copy file (10.4).
//! - [`descriptor`]: the stripe descriptor and the code of a stripe (10.2, 10.4).
//! - [`seal`]: the ballot and the durable seal state of a segment (10.3).
//! - [`free`]: the stripe-free record (10.5).
//! - [`names`]: the file names of the hot and cold layouts, made and parsed (10.3, 10.4).
//! - [`identity`]: the `NODE` and `DRIVE` identity files (10.3, 10.4, 10.8).
//!
//! The rules every format here follows:
//! - integers are little-endian; a header, record or table carries a crc32c and a decode that
//!   finds a different crc is an error ([`FormatError::BadCrc`]);
//! - a reader skips what it does not know in exactly these places: record flag bits, tomb log
//!   entry kinds ([`tomb::LogEntry::Unknown`]), tombstone name kinds ([`tomb::Named::Unknown`])
//!   and block kinds ([`trailer::BlockKind::Other`]); every other format grows by its version
//!   field, so formats change additively (AGENTS.md B8);
//! - an encode of the same value gives the same bytes (the trailer sorts its entries), so two
//!   copies of the same block have byte-identical trailers.
//!
//! Nothing here reads a clock, a file or the network: the wall clock of [`hlc`] is a parameter of
//! every call, and the bytes of a block stream through [`block`] and [`digest`] without being held.

#![forbid(unsafe_code)]

pub mod alloc;
pub mod block;
mod codec;
pub mod descriptor;
pub mod digest;
mod error;
pub mod footer;
pub mod free;
pub mod hlc;
pub mod identity;
pub mod ids;
pub mod names;
pub mod record;
pub mod seal;
pub mod stamp;
pub mod tomb;
pub mod trailer;

pub use error::FormatError;

/// The cell of the storage tier's crc tables: 1 MiB of a block's or shard's bytes, on a grid
/// anchored at offset 0 (the last cell may be short). It equals the client's cell (section 2) so
/// that a cell the client verifies and a cell the store verifies are the same bytes.
pub const CELL_BYTES: u64 = 1 << CELL_SHIFT;

/// `log2` of [`CELL_BYTES`].
pub const CELL_SHIFT: u8 = 20;

/// The number of cells that cover `len` bytes: `ceil(len / CELL_BYTES)`.
#[must_use]
pub fn cells_for(len: u64) -> u64 {
    len.div_ceil(CELL_BYTES)
}

/// The crc32c the formats use (Castagnoli), of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}

/// The crc32c of the bytes whose crc32c is `crc` followed by `bytes`: `crc32c_append(crc32c(a), b)`
/// equals `crc32c(a ++ b)`. It continues the running crc of a segment (10.3) and a cell's crc.
#[must_use]
pub fn crc32c_append(crc: u32, bytes: &[u8]) -> u32 {
    crc32c::crc32c_append(crc, bytes)
}
