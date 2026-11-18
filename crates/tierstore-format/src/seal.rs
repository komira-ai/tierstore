//! The ballot and the durable seal state of a segment (10.3).
//!
//! A segment's sealed length is decided once, by a single-decree agreement over the storage
//! nodes. A [`Ballot`] is `(round, node id)`, ordered lexicographically; a segment opens at round
//! 1 under its writer. Each node keeps, per segment it has voted on, a [`SealState`] in
//! `seals/<block_id>.seal`: the highest ballot it promised, the proposal it accepted (if any) and,
//! once it learns it, the committed proposal. Its bytes (little-endian): magic `TSSL`, version
//! `u16` (1), block id `u64`, promised ballot (`round u32, node u8`), accepted flag `u8` and, when
//! set, ballot and proposal, committed flag `u8` and, when set, the proposal, then the crc32c of
//! the bytes before it. A proposal is `(length u64, running crc32c u32)`: the sealed length and
//! the crc32c of the segment's bytes `[0, length)`.

use crate::FormatError;
use crate::codec::{Dec, Enc, expect_magic};
use crate::descriptor::crc_body;
use crate::ids::{BlockId, NodeId};

/// The magic of a seal state.
pub const SEAL_MAGIC: u32 = u32::from_le_bytes(*b"TSSL");

/// The version of the seal state this release writes and reads.
pub const SEAL_VERSION: u16 = 1;

const WHAT: &str = "seal state";

/// A ballot of the majority seal: ordered by round, then node id.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Ballot {
    /// The round.
    pub round: u32,
    /// The node that proposes under the ballot.
    pub node: NodeId,
}

impl Ballot {
    /// The ballot a segment opens with: round 1 under its writer.
    #[must_use]
    pub const fn opening(writer: NodeId) -> Self {
        Self {
            round: 1,
            node: writer,
        }
    }
}

/// A seal proposal: the sealed length and the running crc32c of `[0, length)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SealProposal {
    /// The sealed length.
    pub length: u64,
    /// The crc32c of the segment's bytes up to the length.
    pub running_crc: u32,
}

/// What one node has durably said about one segment's seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealState {
    /// The segment.
    pub block: BlockId,
    /// The highest ballot promised; batches and proposals under a lower one are refused.
    pub promised: Ballot,
    /// The proposal accepted, with the ballot it was accepted under.
    pub accepted: Option<(Ballot, SealProposal)>,
    /// The committed proposal, once a majority accepted it; a committed seal never changes.
    pub committed: Option<SealProposal>,
}

fn ballot(e: &mut Enc, b: Ballot) {
    e.u32(b.round).u8(b.node.0);
}

fn proposal(e: &mut Enc, p: SealProposal) {
    e.u64(p.length).u32(p.running_crc);
}

fn read_ballot(d: &mut Dec<'_>) -> Result<Ballot, FormatError> {
    Ok(Ballot {
        round: d.u32()?,
        node: NodeId(d.u8()?),
    })
}

fn read_proposal(d: &mut Dec<'_>) -> Result<SealProposal, FormatError> {
    Ok(SealProposal {
        length: d.u64()?,
        running_crc: d.u32()?,
    })
}

fn read_flag(d: &mut Dec<'_>) -> Result<bool, FormatError> {
    match d.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(FormatError::Invalid {
            what: WHAT,
            reason: "a flag is neither 0 nor 1",
        }),
    }
}

impl SealState {
    /// The state's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(SEAL_MAGIC).u16(SEAL_VERSION).u64(self.block.raw());
        ballot(&mut e, self.promised);
        match self.accepted {
            Some((b, p)) => {
                e.u8(1);
                ballot(&mut e, b);
                proposal(&mut e, p);
            }
            None => {
                e.u8(0);
            }
        }
        match self.committed {
            Some(p) => {
                e.u8(1);
                proposal(&mut e, p);
            }
            None => {
                e.u8(0);
            }
        }
        let mut out = e.finish();
        let crc = crate::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// The state in `buf` (the whole of it).
    pub fn decode(buf: &[u8]) -> Result<Self, FormatError> {
        let body = crc_body(buf, WHAT)?;
        let mut d = Dec::new(body, WHAT);
        expect_magic(WHAT, d.u32()?, SEAL_MAGIC)?;
        let version = d.u16()?;
        if version != SEAL_VERSION {
            return Err(FormatError::UnsupportedVersion {
                what: WHAT,
                version: u32::from(version),
            });
        }
        let block = BlockId::from_raw(d.u64()?);
        let promised = read_ballot(&mut d)?;
        let accepted = if read_flag(&mut d)? {
            Some((read_ballot(&mut d)?, read_proposal(&mut d)?))
        } else {
            None
        };
        let committed = if read_flag(&mut d)? {
            Some(read_proposal(&mut d)?)
        } else {
            None
        };
        Ok(Self {
            block,
            promised,
            accepted,
            committed,
        })
    }
}
