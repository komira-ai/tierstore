//! The stripe descriptor (10.2, 10.4) and the code of a stripe.
//!
//! A descriptor is a majority-committed record: the coordinator writes it to every live storage
//! node's NVMe (`stripes/<stripe_id>.desc`) and it is committed when a majority has it durable.
//! It is the atomic point of a destage (10.5); a rebuild or a placement move commits a new
//! revision of it. Its bytes (little-endian): magic `TSSD`, version `u16` (1), code (kind `u8`,
//! then `k u8, m u8` or `replicas u8`), stripe id `u64`, revision `u32`, expiry class `u8`,
//! coordinator `u8`, block count `u16`, `(block id u64, length u64)` per data block, shard count
//! `u16`, `(shard index u8, node u8, drive id 16 bytes)` per stored shard, then the crc32c of all
//! the bytes before it. A descriptor file is written like a seal state (see [`crate::seal`]): a
//! temporary name in `stripes/`, fsync, rename over the final name, fsync the directory, and only
//! then is the write answered; a file that fails its crc is a damaged NVMe.
//!
//! A stripe of `k + m` (10.4) has `k` data shards, indexes `0..k`, and `m` parity shards, indexes
//! `k..k + m`. A partial stripe (10.5) has `j < k` blocks; data shards `j..k` are virtual zeros
//! and are not stored, so they have no entry in [`StripeDescriptor::shards`]. In the replicas mode
//! (`cold.replicas = N`) a descriptor of code [`Code::Replicas`] describes one block and its `N`
//! copies, the copies being shards `0..N` of that block: the same commit point serves the
//! destage of a replicated block.

use crate::FormatError;
use crate::codec::{Dec, Enc, expect_crc, expect_magic};
use crate::ids::{BlockId, DriveId, ExpiryClass, NodeId, StripeId};

/// The magic of a stripe descriptor.
pub const DESCRIPTOR_MAGIC: u32 = u32::from_le_bytes(*b"TSSD");

/// The version of the descriptor this release writes and reads.
pub const DESCRIPTOR_VERSION: u16 = 1;

const WHAT: &str = "stripe descriptor";

/// The redundancy of a stripe, recorded per stripe so a later stripe may use another (10.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    /// Reed-Solomon with `k` data and `m` parity shards.
    Ec {
        /// Data shards.
        k: u8,
        /// Parity shards.
        m: u8,
    },
    /// `N` whole copies of one block.
    Replicas(u8),
}

impl Code {
    /// The shards a full stripe of this code has: `k + m`, or `N`.
    #[must_use]
    pub fn width(self) -> u16 {
        match self {
            Self::Ec { k, m } => u16::from(k) + u16::from(m),
            Self::Replicas(n) => u16::from(n),
        }
    }

    /// The data blocks a full stripe holds: `k`, or 1.
    #[must_use]
    pub fn data_blocks(self) -> u8 {
        match self {
            Self::Ec { k, .. } => k,
            Self::Replicas(_) => 1,
        }
    }

    pub(crate) fn encode(self, e: &mut Enc) {
        match self {
            Self::Ec { k, m } => {
                e.u8(1).u8(k).u8(m);
            }
            Self::Replicas(n) => {
                e.u8(2).u8(n);
            }
        }
    }

    pub(crate) fn decode(d: &mut Dec<'_>, what: &'static str) -> Result<Self, FormatError> {
        let code = match d.u8()? {
            1 => Self::Ec {
                k: d.u8()?,
                m: d.u8()?,
            },
            2 => Self::Replicas(d.u8()?),
            _ => {
                return Err(FormatError::Invalid {
                    what,
                    reason: "unknown code",
                });
            }
        };
        code.check(what)?;
        Ok(code)
    }

    /// Refuses a code with no data shard, an EC code with no parity, or no copies.
    pub(crate) fn check(self, what: &'static str) -> Result<(), FormatError> {
        let ok = match self {
            Self::Ec { k, m } => k > 0 && m > 0,
            Self::Replicas(n) => n > 0,
        };
        if ok {
            Ok(())
        } else {
            Err(FormatError::Invalid {
                what,
                reason: "a code needs at least one data shard and one redundant shard or copy",
            })
        }
    }
}

/// Where a shard lives: its node and drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardLoc {
    /// The storage node.
    pub node: NodeId,
    /// The drive of that node.
    pub drive: DriveId,
}

/// A stored shard of a stripe: its index and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacedShard {
    /// The shard index: data `0..k`, parity `k..k + m`; a copy index `0..N` in the replicas mode.
    pub index: u8,
    /// Its location.
    pub loc: ShardLoc,
}

/// A stripe descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeDescriptor {
    /// The stripe.
    pub stripe: StripeId,
    /// 0 at the destage commit; each committed change (a rebuilt or moved shard) adds 1.
    pub revision: u32,
    /// The stripe's code.
    pub code: Code,
    /// The data blocks and their lengths, data shard `i` holding `blocks[i]`; fewer than `k` for a
    /// partial stripe, exactly one in the replicas mode.
    pub blocks: Vec<(BlockId, u64)>,
    /// The stored shards, in ascending index order.
    pub shards: Vec<PlacedShard>,
    /// The expiry class of every block of the stripe.
    pub expiry: ExpiryClass,
    /// The node that committed the stripe and owns its free (10.5).
    pub coordinator: NodeId,
}

impl StripeDescriptor {
    /// Checks the shape: a valid code, between 1 and `data_blocks` blocks, shard indexes
    /// ascending, unique and inside the code, and every stored shard of a real block or of parity.
    pub fn check(&self) -> Result<(), FormatError> {
        self.code.check(WHAT)?;
        let j = self.blocks.len();
        if j == 0 || j > usize::from(self.code.data_blocks()) {
            return Err(FormatError::Invalid {
                what: WHAT,
                reason: "block count outside 1..=k",
            });
        }
        let k = u16::from(self.code.data_blocks());
        let width = self.code.width();
        let mut prev: Option<u8> = None;
        for s in &self.shards {
            let i = u16::from(s.index);
            let virtual_zero = matches!(self.code, Code::Ec { .. }) && i >= j as u16 && i < k;
            if i >= width || virtual_zero || prev.is_some_and(|p| p >= s.index) {
                return Err(FormatError::Invalid {
                    what: WHAT,
                    reason: "shard indexes not ascending, unique, stored and inside the code",
                });
            }
            prev = Some(s.index);
        }
        Ok(())
    }

    /// The descriptor's bytes, after [`StripeDescriptor::check`].
    pub fn encode(&self) -> Result<Vec<u8>, FormatError> {
        self.check()?;
        let mut e = Enc::new();
        e.u32(DESCRIPTOR_MAGIC).u16(DESCRIPTOR_VERSION);
        self.code.encode(&mut e);
        e.u64(self.stripe.raw())
            .u32(self.revision)
            .u8(self.expiry.0)
            .u8(self.coordinator.0);
        encode_blocks(&mut e, &self.blocks);
        encode_shards(&mut e, &self.shards);
        let mut out = e.finish();
        let crc = crate::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// The descriptor in `buf` (the whole of it), checked.
    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let body = crc_body(buf, WHAT)?;
        let mut d = Dec::new(body, WHAT);
        expect_magic(WHAT, d.u32()?, DESCRIPTOR_MAGIC)?;
        let version = d.u16()?;
        if version != DESCRIPTOR_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: WHAT,
                version: u32::from(version),
            });
        }
        let code = Code::decode(&mut d, WHAT)?;
        let stripe = StripeId::from_raw(d.u64()?);
        let revision = d.u32()?;
        let expiry = ExpiryClass(d.u8()?);
        let coordinator = NodeId(d.u8()?);
        let blocks = decode_blocks(&mut d)?;
        let shards = decode_shards(&mut d)?;
        let desc = Self {
            stripe,
            revision,
            code,
            blocks,
            shards,
            expiry,
            coordinator,
        };
        desc.check()?;
        Ok(desc)
    }
}

/// The bytes before a trailing crc32c, once the crc is checked.
pub(crate) fn crc_body<'a>(buf: &'a [u8], what: &'static str) -> Result<&'a [u8], FormatError> {
    let Some(split) = buf.len().checked_sub(4) else {
        return Err(FormatError::Short {
            what,
            need: 4,
            have: buf.len() as u64,
        });
    };
    let (body, crc) = buf.split_at(split);
    let mut c = [0u8; 4];
    c.copy_from_slice(crc);
    expect_crc(what, body, u32::from_le_bytes(c))?;
    Ok(body)
}

pub(crate) fn encode_blocks(e: &mut Enc, blocks: &[(BlockId, u64)]) {
    // `check` bounds the count by `k <= 255`, so it fits a `u16`.
    e.u16(blocks.len() as u16);
    for (b, len) in blocks {
        e.u64(b.raw()).u64(*len);
    }
}

pub(crate) fn decode_blocks(d: &mut Dec<'_>) -> Result<Vec<(BlockId, u64)>, FormatError> {
    let n = d.u16()?;
    let mut out = Vec::new();
    for _ in 0..n {
        out.push((BlockId::from_raw(d.u64()?), d.u64()?));
    }
    Ok(out)
}

pub(crate) fn encode_shards(e: &mut Enc, shards: &[PlacedShard]) {
    // Both callers have checked the shards: a stripe's by `StripeDescriptor::check` (strictly
    // ascending indexes below the width) and a copy's by `check_copies` (at most 255), so the
    // count fits a `u16`.
    e.u16(shards.len() as u16);
    for s in shards {
        e.u8(s.index).u8(s.loc.node.0).bytes(&s.loc.drive.0);
    }
}

pub(crate) fn decode_shards(d: &mut Dec<'_>) -> Result<Vec<PlacedShard>, FormatError> {
    let n = d.u16()?;
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(PlacedShard {
            index: d.u8()?,
            loc: ShardLoc {
                node: NodeId(d.u8()?),
                drive: DriveId(d.array()?),
            },
        });
    }
    Ok(out)
}
