//! The footer of a cold shard file or copy file (10.4).
//!
//! A shard file is its data region at offset 0 (a data shard: the block's bytes including its
//! trailer; a parity shard: parity bytes; a copy: the block's bytes), zero padding to 4 KiB, then
//! the footer, whose length is a multiple of 4 KiB and at least 4 KiB. The footer's last 16 bytes
//! locate it from the end of the file: footer length `u64`, magic `TSSF`, crc32c of every footer
//! byte before the crc. Before them, little-endian: version `u16` (1), subject kind `u8` (1
//! stripe shard, 2 block copy), the subject, the data length `u64`, the cell count `u64` and one
//! crc32c per 1 MiB cell of the data region, the blake3 of the data region (32 bytes), then zero
//! padding.
//!
//! A stripe shard's subject is the whole stripe at write time (stripe id, shard index, code,
//! the data blocks and lengths, the placement of every shard, the expiry class and the
//! coordinator), so a descriptor is rebuilt from any one footer of a stripe never converted
//! ([`ShardFooter::descriptor`]). A copy's subject names the block (10.5: a converted data shard
//! keeps its copy footer; the parity footers of the converted stripe carry the descriptor).

use crate::codec::{Dec, Enc, expect_crc, expect_magic, tail};
use crate::descriptor::{
    Code, PlacedShard, StripeDescriptor, decode_blocks, decode_shards, encode_blocks, encode_shards,
};
use crate::ids::{BlockId, ExpiryClass, NodeId, StripeId};
use crate::{FormatError, cells_for};

/// The magic of a shard footer.
pub const FOOTER_MAGIC: u32 = u32::from_le_bytes(*b"TSSF");

/// The version of the footer this release writes and reads.
pub const FOOTER_VERSION: u16 = 1;

/// The unit a shard's data region is padded to and its footer is a multiple of.
pub const FOOTER_ALIGN: u64 = 4096;

/// The length of the footer's locating tail.
pub const FOOTER_TAIL_LEN: usize = 16;

const WHAT: &str = "shard footer";

/// What a shard file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterSubject {
    /// One shard of a stripe.
    Stripe {
        /// The stripe, with every shard's placement at write time. Its revision is the one
        /// the shard was written under.
        stripe: StripeDescriptor,
        /// This shard's index.
        index: u8,
    },
    /// One copy of a block (the replicas mode, or a data shard converted in place).
    Copy {
        /// The block.
        block: BlockId,
        /// The block's length.
        length: u64,
        /// The expiry class of the block.
        expiry: ExpiryClass,
        /// Where every copy of the block was placed when this one was written.
        copies: Vec<PlacedShard>,
    },
}

/// A shard file's footer, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardFooter {
    /// What the file holds.
    pub subject: FooterSubject,
    /// The bytes of the data region (before the padding).
    pub data_len: u64,
    /// The crc32c of each 1 MiB cell of the data region.
    pub cell_crcs: Vec<u32>,
    /// The blake3 of the data region.
    pub blake3: [u8; 32],
}

/// The offset of the footer in a shard file whose data region is `data_len` bytes.
#[must_use]
pub fn footer_offset(data_len: u64) -> u64 {
    data_len.next_multiple_of(FOOTER_ALIGN)
}

/// A copy footer lists at most 255 copies, with strictly ascending indexes.
fn check_copies(copies: &[PlacedShard]) -> Result<(), FormatError> {
    if copies.len() > 255 || copies.windows(2).any(|w| w[0].index >= w[1].index) {
        return Err(FormatError::Invalid {
            what: WHAT,
            reason: "copies are more than 255 or their indexes are not ascending",
        });
    }
    Ok(())
}

impl ShardFooter {
    /// The footer's bytes (a multiple of [`FOOTER_ALIGN`]); the cell table must cover the data.
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        if self.cell_crcs.len() as u64 != cells_for(self.data_len) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "cell table does not cover the data region",
            });
        }
        let mut e = Enc::new();
        e.u16(FOOTER_VERSION);
        match &self.subject {
            FooterSubject::Stripe { stripe, index } => {
                stripe.check()?;
                e.u8(1).u8(*index);
                stripe.code.encode(&mut e);
                e.u64(stripe.stripe.raw())
                    .u32(stripe.revision)
                    .u8(stripe.expiry.0)
                    .u8(stripe.coordinator.0);
                encode_blocks(&mut e, &stripe.blocks);
                encode_shards(&mut e, &stripe.shards);
            }
            FooterSubject::Copy {
                block,
                length,
                expiry,
                copies,
            } => {
                check_copies(copies)?;
                e.u8(2).u64(block.raw()).u64(*length).u8(expiry.0);
                encode_shards(&mut e, copies);
            }
        }
        e.u64(self.data_len).u64(self.cell_crcs.len() as u64);
        for crc in &self.cell_crcs {
            e.u32(*crc);
        }
        e.bytes(&self.blake3);
        let body_len = e.len() as u64 + FOOTER_TAIL_LEN as u64;
        let total = body_len.next_multiple_of(FOOTER_ALIGN);
        let mut out = e.finish();
        // `total - FOOTER_TAIL_LEN` is at least the body's length.
        out.resize((total as usize) - FOOTER_TAIL_LEN, 0);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&FOOTER_MAGIC.to_le_bytes());
        let crc = crate::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// The footer that ends `buf` (the whole shard file, or any suffix holding the footer).
    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let t = tail(buf, FOOTER_TAIL_LEN as u64, WHAT)?;
        let mut d = Dec::new(t, WHAT);
        let total = d.u64()?;
        expect_magic(WHAT, d.u32()?, FOOTER_MAGIC)?;
        let stored = d.u32()?;
        if total < FOOTER_ALIGN || !total.is_multiple_of(FOOTER_ALIGN) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "footer length is not a positive multiple of 4 KiB",
            });
        }
        let whole = tail(buf, total, WHAT)?;
        expect_crc(WHAT, &whole[..whole.len() - 4], stored)?;
        let mut d = Dec::new(whole, WHAT);
        let version = d.u16()?;
        if version != FOOTER_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: WHAT,
                version: u32::from(version),
            });
        }
        let subject = match d.u8()? {
            1 => {
                let index = d.u8()?;
                let code = Code::decode(&mut d, WHAT)?;
                let stripe = StripeDescriptor {
                    code,
                    stripe: StripeId::from_raw(d.u64()?),
                    revision: d.u32()?,
                    expiry: ExpiryClass(d.u8()?),
                    coordinator: NodeId(d.u8()?),
                    blocks: decode_blocks(&mut d)?,
                    shards: decode_shards(&mut d)?,
                };
                stripe.check()?;
                FooterSubject::Stripe { stripe, index }
            }
            2 => {
                let block = BlockId::from_raw(d.u64()?);
                let length = d.u64()?;
                let expiry = ExpiryClass(d.u8()?);
                let copies = decode_shards(&mut d)?;
                check_copies(&copies)?;
                FooterSubject::Copy {
                    block,
                    length,
                    expiry,
                    copies,
                }
            }
            _ => {
                return Err(FormatError::Invalid {
                    what: WHAT,
                    reason: "unknown subject kind",
                });
            }
        };
        let data_len = d.u64()?;
        let cell_count = d.u64()?;
        if cell_count != cells_for(data_len) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "cell table does not cover the data region",
            });
        }
        let mut cell_crcs = Vec::new();
        for _ in 0..cell_count {
            cell_crcs.push(d.u32()?);
        }
        let blake3 = d.array()?;
        Ok(Self {
            subject,
            data_len,
            cell_crcs,
            blake3,
        })
    }

    /// The stripe descriptor this footer rebuilds: the stripe of a stripe shard's footer, none
    /// for a copy's (10.2: a descriptor is rebuilt from any footer of a stripe never converted,
    /// and from a parity footer of a converted one).
    #[must_use]
    pub fn descriptor(&self) -> Option<&StripeDescriptor> {
        match &self.subject {
            FooterSubject::Stripe { stripe, .. } => Some(stripe),
            FooterSubject::Copy { .. } => None,
        }
    }
}
