//! The formats of the tierstore storage tier, as section 10 of
//! `docs/design/fetch-cache-serve.md` (revision 7) states them, frozen by the interface-freeze
//! change (section 18, row T-I). Every other task of the storage tier writes and reads bytes
//! through these types; a change to a layout here is a change to a frozen format and says so.
//!
//! What is here, by module:
//! - [`ids`]: node, block, stripe, drive and cluster ids, the generation (the 128-bit blake3 of
//!   an object's bytes, its ETag), the key hash and the expiry class (10.2).
//! - [`stamp`]: the cluster stamp, a hybrid logical clock value of 48 bits of wall-clock
//!   milliseconds and 16 bits of counter (10.2). The clock that advances and merges it is T-S2's.
//! - [`record`]: the 64-byte record header, the record framing of a segment and the scan that
//!   stops at the last valid record (10.2).
//! - [`tomb`]: the tombstone log of `tomb/<seq>.log`: tombstones, block drops and tomb drops,
//!   each framed with a crc; a kind this release does not know is kept and skipped, never refused
//!   (10.2, 10.14).
//! - [`trailer`]: the block trailer (sorted entries with stamps, a crc32c per 1 MiB cell, the
//!   block's blake3), which is also the content of `seg/<block_id>.idx` and `blk/<block_id>.idx`
//!   (10.2).
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
//! The blake3 values the formats carry (the generation, the block and shard digests) are stored
//! as bytes here; computing them is T-S2's (`format` helpers), as is the hybrid logical clock.

#![forbid(unsafe_code)]

mod codec;
pub mod descriptor;
mod error;
pub mod footer;
pub mod free;
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
