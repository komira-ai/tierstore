//! The stripe-free record (10.5).
//!
//! The owner of a stripe's free (its coordinator, else the next node of `rank(stripe_id)`) writes
//! a stripe free as a majority-committed record once every storage node answered zero live
//! entries for every data block of the stripe. From then the stripe is dead in every node's block
//! table; shard files are unlinked only after the commit. It is kept as
//! `stripes/<stripe_id>.free` beside the descriptor. Its bytes (little-endian): magic `TSFR`,
//! version `u16` (1), stripe id `u64`, the descriptor revision it frees `u32`, owner `u8`, stamp
//! `u64`, then the crc32c of the bytes before it.

use crate::FormatError;
use crate::codec::{Dec, Enc, expect_magic};
use crate::descriptor::crc_body;
use crate::ids::{NodeId, StripeId};
use crate::stamp::Stamp;

/// The magic of a stripe free.
pub const FREE_MAGIC: u32 = u32::from_le_bytes(*b"TSFR");

/// The version of the record this release writes and reads.
pub const FREE_VERSION: u16 = 1;

const WHAT: &str = "stripe free";

/// A stripe free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeFree {
    /// The stripe freed.
    pub stripe: StripeId,
    /// The descriptor revision the owner counted against; a rebuild that commits a later
    /// revision for a freed stripe is refused (10.5 step 5).
    pub revision: u32,
    /// The node that owns the free.
    pub owner: NodeId,
    /// The owner's stamp at the commit.
    pub stamp: Stamp,
}

impl StripeFree {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(FREE_MAGIC)
            .u16(FREE_VERSION)
            .u64(self.stripe.raw())
            .u32(self.revision)
            .u8(self.owner.0)
            .u64(self.stamp.raw());
        let mut out = e.finish();
        let crc = crate::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// The record in `buf` (the whole of it).
    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let body = crc_body(buf, WHAT)?;
        let mut d = Dec::new(body, WHAT);
        expect_magic(WHAT, d.u32()?, FREE_MAGIC)?;
        let version = d.u16()?;
        if version != FREE_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: WHAT,
                version: u32::from(version),
            });
        }
        Ok(Self {
            stripe: StripeId::from_raw(d.u64()?),
            revision: d.u32()?,
            owner: NodeId(d.u8()?),
            stamp: Stamp::from_raw(d.u64()?),
        })
    }
}
