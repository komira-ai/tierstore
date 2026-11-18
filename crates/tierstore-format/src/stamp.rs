//! The cluster stamp (10.2): a hybrid logical clock value every storage node can compare.
//!
//! A stamp is a `u64`: the high 48 bits are wall-clock milliseconds since the Unix epoch, the low
//! 16 bits a counter. Stamps order by their `u64`, so "the newest fact about a key wins" is a
//! comparison of two stamps. The clock that advances a stamp on every local event and merges the
//! stamp of every peer message (`max`, then increment) is T-S2's; this module fixes the layout.

use crate::FormatError;

/// A cluster stamp. Ordered by its `u64`: wall milliseconds first, then the counter.
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
pub struct Stamp(u64);

/// The bits of a stamp that hold wall-clock milliseconds.
pub const WALL_BITS: u32 = 48;

/// The bits of a stamp that hold the counter.
pub const STAMP_COUNTER_BITS: u32 = 16;

/// The largest wall-clock millisecond value a stamp holds (the year 10889).
pub const WALL_MAX: u64 = (1 << WALL_BITS) - 1;

impl Stamp {
    /// The stamp before every stamp a node assigns.
    pub const ZERO: Self = Self(0);

    /// The stamp of `wall_ms` milliseconds and counter `counter`; a `wall_ms` above
    /// [`WALL_MAX`] does not fit.
    pub fn from_parts(wall_ms: u64, counter: u16) -> Result<Self, FormatError> {
        if wall_ms > WALL_MAX {
            return Err(FormatError::Invalid {
                what: "stamp",
                reason: "wall clock above 2^48 - 1 ms",
            });
        }
        Ok(Self((wall_ms << STAMP_COUNTER_BITS) | u64::from(counter)))
    }

    /// The stamp whose `u64` is `raw` (every `u64` is a stamp).
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The stamp as one `u64`, the form every format stores.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The wall-clock milliseconds of the stamp.
    #[must_use]
    pub const fn wall_ms(self) -> u64 {
        self.0 >> STAMP_COUNTER_BITS
    }

    /// The counter of the stamp.
    #[must_use]
    pub const fn counter(self) -> u16 {
        // The mask keeps the low 16 bits, so the cast loses nothing.
        (self.0 & 0xffff) as u16
    }
}
