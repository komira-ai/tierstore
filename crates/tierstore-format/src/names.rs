//! The file names of the hot layout (10.3) and the cold layout (10.4), made and parsed.
//!
//! Hot, under the hot directory (`ts-hot/` on the NVMe, or the SSD partition of the NOW profile):
//!
//! ```text
//! NODE                                 node id, cluster id, format version
//! seg/<block_id>.seg                   a write segment (open, or sealed)
//! seg/<block_id>.idx                   sorted trailer index of a sealed segment
//! big/<hh>/<khash>.<gen>.<size>.obj    a large object, whole file
//! big/.<khash>.<nonce>.up              a large upload in progress
//! cache/<boot>.<seq>.seg               a cache segment (promoted copies only)
//! tomb/<origin>/<first_seq>.log        a file of one origin's tombstone log
//! blk/<block_id>.idx                   index file of a block this node indexes
//! index/                               the embedded ordered store
//! stripes/<stripe_id>.desc             stripe descriptor
//! stripes/<stripe_id>.free             stripe free
//! seals/<block_id>.seal                seal state
//! ```
//!
//! Cold, under each drive's `ts-cold/`:
//!
//! ```text
//! DRIVE                                drive id, node id, cluster id, format version, fs UUID
//! s/<hh>/<stripe_id>.<idx>.shard       one shard of a stripe
//! s/<hh>/<block_id>.copy               one copy of a block (replicas mode)
//! s/<hh>/.<stripe_id>.<idx>.<nonce>.up a shard being written
//! s/<hh>/.<block_id>.<nonce>.up        a copy being written
//! FAILED                               the drive is failed (10.7)
//! ```
//!
//! Block and stripe ids are 16 hex digits, key hashes and generations 32; a size, a sequence
//! number, an origin node id, a boot epoch, a shard index and a nonce are decimal (a nonce is 16 hex digits). `<hh>`
//! is the two hex digits of the low byte of the id (a cold file) or the first byte of the key hash
//! (a large object), so a directory holds at most 1/256 of the files. A name that is not one of
//! these parses to `None`: the startup sweep then leaves it alone.

use std::str::FromStr;

use crate::ids::{BlockId, Generation, KeyHash, NodeId, StripeId};
use crate::tomb::LargeFile;

/// The hot directory's identity file.
pub const NODE_FILE: &str = "NODE";
/// A drive's identity file.
pub const DRIVE_FILE: &str = "DRIVE";
/// The file present once a drive is failed.
pub const FAILED_FILE: &str = "FAILED";
/// The directory of the embedded ordered store.
pub const INDEX_DIR: &str = "index";

/// A file of the hot layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotName {
    /// `seg/<block_id>.seg`
    Segment(BlockId),
    /// `seg/<block_id>.idx`
    SegmentIndex(BlockId),
    /// `big/<hh>/<khash>.<gen>.<size>.obj`
    Large(LargeFile),
    /// `big/.<khash>.<nonce>.up`
    LargeUpload {
        /// The key hash.
        key_hash: KeyHash,
        /// The upload's nonce.
        nonce: u64,
    },
    /// `cache/<boot>.<seq>.seg`
    Cache {
        /// The boot epoch of the node that made it.
        boot: u64,
        /// Its sequence number within that boot.
        seq: u64,
    },
    /// `tomb/<origin>/<first_seq>.log`: a file of the tombstone log of `origin` (every origin's log
    /// is stored on every node). It holds consecutive entries starting at `first_seq`; a new file
    /// may be started at any entry, and the files of one origin sort by `first_seq`.
    TombLog {
        /// The node whose log the file holds, in decimal (0 to 255).
        origin: NodeId,
        /// The sequence number of the file's first entry, in decimal.
        first_seq: u64,
    },
    /// `blk/<block_id>.idx`
    BlockIndex(BlockId),
    /// `stripes/<stripe_id>.desc`
    Descriptor(StripeId),
    /// `stripes/<stripe_id>.free`
    Free(StripeId),
    /// `seals/<block_id>.seal`
    Seal(BlockId),
}

fn hh(byte: u8) -> String {
    format!("{byte:02x}")
}

impl HotName {
    /// The path of the file, relative to the hot directory.
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::Segment(b) => format!("seg/{b}.seg"),
            Self::SegmentIndex(b) => format!("seg/{b}.idx"),
            Self::Large(f) => format!(
                "big/{}/{}.{}.{}.obj",
                hh(f.key_hash.0[0]),
                f.key_hash,
                f.generation,
                f.size
            ),
            Self::LargeUpload { key_hash, nonce } => format!("big/.{key_hash}.{nonce:016x}.up"),
            Self::Cache { boot, seq } => format!("cache/{boot}.{seq}.seg"),
            Self::TombLog { origin, first_seq } => {
                format!("tomb/{}/{first_seq}.log", origin.0)
            }
            Self::BlockIndex(b) => format!("blk/{b}.idx"),
            Self::Descriptor(s) => format!("stripes/{s}.desc"),
            Self::Free(s) => format!("stripes/{s}.free"),
            Self::Seal(b) => format!("seals/{b}.seal"),
        }
    }

    /// The file a relative path names, if it is one of the hot layout's. The inverse of
    /// [`HotName::path`]: a path parses only in the exact form `path` writes.
    #[must_use]
    pub fn parse(path: &str) -> Option<Self> {
        let parts: Vec<&str> = path.split('/').collect();
        let name = match parts.as_slice() {
            ["seg", file] => parse_seg(file)?,
            ["big", file] => parse_upload(file)?,
            ["big", _, file] => Self::Large(parse_large(file)?),
            ["cache", file] => {
                let [boot, seq] = fields::<2>(file.strip_suffix(".seg")?)?;
                Self::Cache {
                    boot: decimal(boot)?,
                    seq: decimal(seq)?,
                }
            }
            ["tomb", origin, file] => Self::TombLog {
                origin: NodeId(decimal(origin)?.try_into().ok()?),
                first_seq: decimal(file.strip_suffix(".log")?)?,
            },
            ["blk", file] => Self::BlockIndex(file.strip_suffix(".idx")?.parse().ok()?),
            ["stripes", file] => parse_stripe_file(file)?,
            ["seals", file] => Self::Seal(file.strip_suffix(".seal")?.parse().ok()?),
            _ => return None,
        };
        (name.path() == path).then_some(name)
    }
}

fn parse_seg(file: &str) -> Option<HotName> {
    if let Some(id) = file.strip_suffix(".seg") {
        return Some(HotName::Segment(id.parse().ok()?));
    }
    Some(HotName::SegmentIndex(
        file.strip_suffix(".idx")?.parse().ok()?,
    ))
}

fn parse_upload(file: &str) -> Option<HotName> {
    let rest = file.strip_prefix('.')?.strip_suffix(".up")?;
    let [khash, nonce] = fields::<2>(rest)?;
    Some(HotName::LargeUpload {
        key_hash: khash.parse().ok()?,
        nonce: hex_u64(nonce)?,
    })
}

fn parse_large(file: &str) -> Option<LargeFile> {
    let [khash, generation, size] = fields::<3>(file.strip_suffix(".obj")?)?;
    Some(LargeFile {
        key_hash: KeyHash::from_str(khash).ok()?,
        generation: Generation::from_str(generation).ok()?,
        size: decimal(size)?,
    })
}

fn parse_stripe_file(file: &str) -> Option<HotName> {
    if let Some(id) = file.strip_suffix(".desc") {
        return Some(HotName::Descriptor(id.parse().ok()?));
    }
    Some(HotName::Free(file.strip_suffix(".free")?.parse().ok()?))
}

/// A file of the cold layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColdName {
    /// `s/<hh>/<stripe_id>.<idx>.shard`
    Shard {
        /// The stripe.
        stripe: StripeId,
        /// The shard index.
        index: u8,
    },
    /// `s/<hh>/<block_id>.copy`
    Copy(BlockId),
    /// `s/<hh>/.<stripe_id>.<idx>.<nonce>.up`
    ShardUpload {
        /// The stripe.
        stripe: StripeId,
        /// The shard index.
        index: u8,
        /// The write's nonce.
        nonce: u64,
    },
    /// `s/<hh>/.<block_id>.<nonce>.up`
    CopyUpload {
        /// The block.
        block: BlockId,
        /// The write's nonce.
        nonce: u64,
    },
}

impl ColdName {
    /// The path of the file, relative to the drive's cold directory.
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::Shard { stripe, index } => {
                format!("s/{}/{stripe}.{index}.shard", hh(low(stripe.raw())))
            }
            Self::Copy(b) => format!("s/{}/{b}.copy", hh(low(b.raw()))),
            Self::ShardUpload {
                stripe,
                index,
                nonce,
            } => format!(
                "s/{}/.{stripe}.{index}.{nonce:016x}.up",
                hh(low(stripe.raw()))
            ),
            Self::CopyUpload { block, nonce } => {
                format!("s/{}/.{block}.{nonce:016x}.up", hh(low(block.raw())))
            }
        }
    }

    /// The file a relative path names, if it is one of the cold layout's; the inverse of
    /// [`ColdName::path`].
    #[must_use]
    pub fn parse(path: &str) -> Option<Self> {
        let ["s", _, file] = path.split('/').collect::<Vec<_>>()[..] else {
            return None;
        };
        let name = if let Some(up) = file.strip_prefix('.') {
            let up = up.strip_suffix(".up")?;
            match up.split('.').collect::<Vec<_>>()[..] {
                [stripe, index, nonce] => Self::ShardUpload {
                    stripe: stripe.parse().ok()?,
                    index: decimal(index)?.try_into().ok()?,
                    nonce: hex_u64(nonce)?,
                },
                [block, nonce] => Self::CopyUpload {
                    block: block.parse().ok()?,
                    nonce: hex_u64(nonce)?,
                },
                _ => return None,
            }
        } else if let Some(shard) = file.strip_suffix(".shard") {
            let [stripe, index] = fields::<2>(shard)?;
            Self::Shard {
                stripe: stripe.parse().ok()?,
                index: decimal(index)?.try_into().ok()?,
            }
        } else {
            Self::Copy(file.strip_suffix(".copy")?.parse().ok()?)
        };
        (name.path() == path).then_some(name)
    }
}

fn low(id: u64) -> u8 {
    id.to_le_bytes()[0]
}

/// Exactly `N` fields separated by dots.
fn fields<const N: usize>(s: &str) -> Option<[&str; N]> {
    s.split('.').collect::<Vec<_>>().try_into().ok()
}

fn decimal(s: &str) -> Option<u64> {
    if s.bytes().all(|c| c.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

fn hex_u64(s: &str) -> Option<u64> {
    crate::ids::parse_hex::<8>("nonce", s)
        .ok()
        .map(u64::from_be_bytes)
}
