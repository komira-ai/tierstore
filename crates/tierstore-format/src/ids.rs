//! Ids of the storage tier (10.2): nodes, blocks, stripes, drives, the cluster, and the values
//! that name an object's bytes.
//!
//! A block id and a stripe id are `(allocating node: 8 bits, per-node counter: 56 bits)`, so no
//! central allocator exists. Their text form, used in file names, is 16 lowercase hex digits of
//! the whole `u64`; a drive id, a cluster id, a generation and a key hash are 32 lowercase hex
//! digits of their 16 bytes.

use std::fmt;
use std::str::FromStr;

use crate::FormatError;

/// Serializes an id as its text form and deserializes it with `FromStr`.
macro_rules! text_serde {
    ($name:ident) => {
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

/// A storage node's id (`node.id` of 10.14), the high 8 bits of the ids it allocates.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct NodeId(pub u8);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The bits of a block or stripe id that hold the per-node counter.
pub const COUNTER_BITS: u32 = 56;

/// The largest per-node counter a block or stripe id holds.
pub const COUNTER_MAX: u64 = (1 << COUNTER_BITS) - 1;

macro_rules! allocated_id {
    ($(#[$doc:meta])* $name:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// The id that `node` allocates as its `counter`-th; a counter above
            /// [`COUNTER_MAX`] does not fit.
            pub fn new(node: NodeId, counter: u64) -> Result<Self, FormatError> {
                if counter > COUNTER_MAX {
                    return Err(FormatError::Invalid {
                        what: $what,
                        reason: "counter above 2^56 - 1",
                    });
                }
                Ok(Self((u64::from(node.0) << COUNTER_BITS) | counter))
            }

            /// The id whose `u64` is `raw` (every `u64` is an id).
            #[must_use]
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// The id as one `u64`, the node in the high 8 bits.
            #[must_use]
            pub const fn raw(self) -> u64 {
                self.0
            }

            /// The node that allocated the id.
            #[must_use]
            pub const fn node(self) -> NodeId {
                NodeId((self.0 >> COUNTER_BITS) as u8)
            }

            /// The allocating node's counter.
            #[must_use]
            pub const fn counter(self) -> u64 {
                self.0 & COUNTER_MAX
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:016x}", self.0)
            }
        }

        text_serde!($name);

        impl FromStr for $name {
            type Err = FormatError;

            /// Exactly 16 lowercase hex digits, the form [`fmt::Display`] writes.
            fn from_str(s: &str) -> Result<Self, FormatError> {
                let bytes: [u8; 8] = parse_hex($what, s)?;
                Ok(Self(u64::from_be_bytes(bytes)))
            }
        }
    };
}

allocated_id!(
    /// A block's id (10.2): a sealed or open segment, a pack block, a slice of a chain.
    BlockId,
    "block id"
);

allocated_id!(
    /// A stripe's id (10.2, 10.4): `k` data blocks and `m` parity blocks destaged together.
    StripeId,
    "stripe id"
);

macro_rules! hex16 {
    ($(#[$doc:meta])* $name:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; 16]);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for b in self.0 {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }

        text_serde!($name);

        impl FromStr for $name {
            type Err = FormatError;

            /// Exactly 32 lowercase hex digits, the form [`fmt::Display`] writes.
            fn from_str(s: &str) -> Result<Self, FormatError> {
                Ok(Self(parse_hex($what, s)?))
            }
        }
    };
}

hex16!(
    /// A hard drive's id (the UUID in its `DRIVE` file, 10.4).
    DriveId,
    "drive id"
);

hex16!(
    /// The cluster's id, in every `NODE` and `DRIVE` file, so a drive of another cluster is
    /// never opened (10.8).
    ClusterId,
    "cluster id"
);

hex16!(
    /// An object's generation (10.2): the 128-bit blake3 of its bytes, computed while they stream
    /// in. It is the object's ETag and the `If-Match` of every GET of a fill (4.6); two copies of
    /// the same bytes have the same generation.
    Generation,
    "generation"
);

hex16!(
    /// The hash of an object's key that names its large file (`big/<hh>/<khash>...`, 10.3): the
    /// first 16 bytes of the blake3 of the key bytes.
    KeyHash,
    "key hash"
);

/// The expiry class of an object (the `x-ts-expiry` hint of 10.2): it groups bytes that are
/// expected to die together, and never deletes anything. Values at or above `gc.classes_max`
/// fall into class 0 (10.5); that fold is the store's, not the format's.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct ExpiryClass(pub u8);

/// `N` bytes from exactly `2 * N` lowercase hex digits.
pub(crate) fn parse_hex<const N: usize>(
    what: &'static str,
    s: &str,
) -> Result<[u8; N], FormatError> {
    let digits = s.as_bytes();
    if digits.len() != 2 * N {
        return Err(FormatError::Invalid {
            what,
            reason: "wrong number of hex digits",
        });
    }
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *byte = (nibble(what, pair[0])? << 4) | nibble(what, pair[1])?;
    }
    Ok(out)
}

fn nibble(what: &'static str, c: u8) -> Result<u8, FormatError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(FormatError::Invalid {
            what,
            reason: "not a lowercase hex digit",
        }),
    }
}
