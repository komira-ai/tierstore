//! Deterministic fuzz and round-trip tests over every decoder of the crate (design section 18,
//! row T-S2): each format is encoded from random valid values and decoded back to an equal value
//! with identical re-encoded bytes; each decoder is then fed damaged copies of valid encodings
//! (flipped bits, every truncation, extensions, overwritten runs, and damage whose crc was
//! recomputed so the decode gets past the crc) and pure random bytes. Every random choice comes
//! from the `SplitMix64` generator below with a fixed seed and every loop has a fixed count, so a
//! failure reproduces.
//!
//! Catches:
//! - a decoder that panics (slice out of range, overflow, allocation from a hostile count) on a
//!   damaged or random input instead of returning an error;
//! - a crc-protected format that accepts a single flipped bit of a covered byte, a truncation, or an
//!   extension of the bytes it covers (a record's padding is the one uncovered span, and a
//!   trailer or footer is found from the end of its buffer, so bytes before it are allowed);
//! - an encode that is not the inverse of the decode (a lost flag bit, key, name, copy or kind, a
//!   different byte order, or re-encoded bytes that differ), including keys of 0 and 65535 bytes;
//! - a decode that returns `Ok` for damage that was crc-repaired yet yields a value the format
//!   would not itself encode (a descriptor that fails its own check, an unsorted trailer);
//! - `scan` or `scan_log` that lists a record that does not decode, reports a `valid_len` past the
//!   input or off a record boundary, or skips damage, a sequence gap or another origin's record;
//! - a file name that parses to a value whose path is a different string (so two names for one
//!   file), an id text that is accepted in upper case or at the wrong length, or an identity file
//!   accepted in a form this release would not write;
//! - a block builder whose result depends on how its input is split into `feed` calls, that
//!   accepts a flipped bit anywhere in a block, that finishes inside a record, or whose trailer
//!   differs from a scan of the same bytes by `record`, `trailer::cell_crcs` and `blake3_256`;
//! - a chain whose slices do not tile a requested range exactly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tierstore_format::block::{BlockBuilder, BlockError, BlockGeometry, Sealed};
use tierstore_format::descriptor::{Code, PlacedShard, ShardLoc, StripeDescriptor};
use tierstore_format::digest::blake3_256;
use tierstore_format::footer::{FooterSubject, ShardFooter};
use tierstore_format::free::StripeFree;
use tierstore_format::identity::{DriveIdentity, NodeIdentity};
use tierstore_format::ids::{
    BlockId, ClusterId, DriveId, ExpiryClass, Generation, KeyHash, NodeId, StripeId,
};
use tierstore_format::names::{ColdName, HotName};
use tierstore_format::record::{
    RECORD_MAGIC, RecordFlags, RecordHeader, RecordIn, decode_record, encode_record, record_len,
    scan,
};
use tierstore_format::seal::{Ballot, SealProposal, SealState};
use tierstore_format::stamp::{Stamp, WALL_MAX};
use tierstore_format::tomb::{
    BlockDrop, LargeFile, LogEntry, LogRecord, Named, TOMB_MAGIC, TombDrop, Tombstone, scan_log,
    scan_log_after,
};
use tierstore_format::trailer::{BlockKind, Trailer, TrailerEntry, cell_crcs};
use tierstore_format::{CELL_BYTES, FormatError, cells_for, crc32c};

#[path = "fuzz/blocks.rs"]
mod blocks;
#[path = "fuzz/formats.rs"]
mod formats;
#[path = "fuzz/records.rs"]
mod records;
#[path = "fuzz/support.rs"]
mod support;
#[path = "fuzz/text.rs"]
mod text;

use support::*;
