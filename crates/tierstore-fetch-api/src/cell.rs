//! The cell byte and its transitions (section 2).
//!
//! A cell's state is one byte of the state page: the low 3 bits are the state, bit 7 is
//! `durable` (set by the server after the batch `fdatasync` of section 8, meaningful only on
//! `present`). Bits 3 to 6 are zero. Every transition is a CAS from a named predecessor state; no
//! state is ever overwritten by a plain store. The mount's RPC-free read path treats anything but
//! `present` as "not ready".

/// The state of a cell (the low 3 bits of its byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellState {
    /// No bytes on disk, or not trusted.
    Absent,
    /// A fetch attempt owns it; bytes in flight.
    Claimed,
    /// Verified bytes on disk, servable by the mount without RPC.
    Present,
    /// Bytes on disk from before an unclean start in another boot; re-hashed before serving.
    Recheck,
    /// The evictor owns it; readers must not use it.
    Evicting,
    /// Failed verification twice; EIO until expiry.
    Bad,
}

impl CellState {
    /// The state's value in the low 3 bits of the cell byte.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Self::Absent => 0,
            Self::Claimed => 1,
            Self::Present => 2,
            Self::Recheck => 3,
            Self::Evicting => 4,
            Self::Bad => 5,
        }
    }

    /// The state of 3 bits; 6 and 7 are not states.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            0 => Some(Self::Absent),
            1 => Some(Self::Claimed),
            2 => Some(Self::Present),
            3 => Some(Self::Recheck),
            4 => Some(Self::Evicting),
            5 => Some(Self::Bad),
            _ => None,
        }
    }
}

/// The bit of the cell byte that says the cell's bytes are durable.
pub const DURABLE_BIT: u8 = 1 << 7;

/// The bits of the cell byte that hold the state.
pub const STATE_MASK: u8 = 0b111;

/// A cell byte as it is stored in the state page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellByte(pub u8);

impl CellByte {
    /// The byte of `state`, with the `durable` bit when `durable` (only meaningful on present).
    #[must_use]
    pub const fn new(state: CellState, durable: bool) -> Self {
        let d = if durable { DURABLE_BIT } else { 0 };
        Self(state.bits() | d)
    }

    /// The cell's state, or `None` for a byte whose state bits are 6 or 7 or that has any of
    /// bits 3 to 6 set (a byte a later release gives another meaning is no state of this one).
    #[must_use]
    pub const fn state(self) -> Option<CellState> {
        CellState::from_bits(self.0 & !DURABLE_BIT)
    }

    /// Whether the `durable` bit is set.
    #[must_use]
    pub const fn durable(self) -> bool {
        self.0 & DURABLE_BIT != 0
    }

    /// Whether the mount may serve the cell: present, durable or not. A byte with any of bits 3
    /// to 6 set is not servable, so a byte a later release gives another meaning is never
    /// mistaken for present.
    #[must_use]
    pub const fn servable(self) -> bool {
        self.0 & !DURABLE_BIT == CellState::Present.bits()
    }
}

/// Every transition the state table of section 2 allows, as `(from, to)`. Each is one CAS of the
/// cell byte; the server sets `durable` with [`Transition::Durable`] after the batch `fdatasync`
/// (see [`Transition::apply`] for which transitions keep it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transition {
    /// `absent -> claimed`: a fetch attempt claims the cell.
    Claim,
    /// `claimed -> absent`: the attempt failed.
    Abandon,
    /// `claimed -> present`: the verified bytes are written (one winner, so a hedge or retry
    /// cannot publish twice).
    Publish,
    /// `claimed -> bad`: the second verification failure.
    MarkBad,
    /// `present -> present + durable`: the batch `fdatasync` covered the cell.
    Durable,
    /// `present -> evicting`: the evictor won the cell (5.1 step 1). The `durable` bit stays, so
    /// the CAS expects the loaded byte (`0x02` or `0x82`) and the evicting byte is `0x04` or
    /// `0x84`.
    BeginEvict,
    /// `evicting -> present`: a reader stayed past `evict_wait` (5.1 step 3).
    AbortEvict,
    /// `evicting -> absent`: punched (5.1 step 4).
    FinishEvict,
    /// `recheck -> present`: the re-hash matched.
    RecheckOk,
    /// `recheck -> absent`: the re-hash did not match.
    RecheckFailed,
    /// `bad -> absent`: the 60 s of a bad cell expired.
    ExpireBad,
}

impl Transition {
    /// The state the cell must be in (the CAS's expected value).
    #[must_use]
    pub const fn from(self) -> CellState {
        match self {
            Self::Claim => CellState::Absent,
            Self::Abandon | Self::Publish | Self::MarkBad => CellState::Claimed,
            Self::Durable | Self::BeginEvict => CellState::Present,
            Self::AbortEvict | Self::FinishEvict => CellState::Evicting,
            Self::RecheckOk | Self::RecheckFailed => CellState::Recheck,
            Self::ExpireBad => CellState::Bad,
        }
    }

    /// The state the cell is in after it.
    #[must_use]
    pub const fn to(self) -> CellState {
        match self {
            Self::Claim => CellState::Claimed,
            Self::Abandon | Self::FinishEvict | Self::RecheckFailed | Self::ExpireBad => {
                CellState::Absent
            }
            Self::Publish | Self::Durable | Self::AbortEvict | Self::RecheckOk => {
                CellState::Present
            }
            Self::MarkBad => CellState::Bad,
            Self::BeginEvict => CellState::Evicting,
        }
    }

    /// The byte after the transition from `current`, or `None` when `current` is not the
    /// transition's predecessor (the CAS fails). `Durable` keeps the state and sets the bit.
    /// `BeginEvict` and `AbortEvict` keep the bit, so a cell whose eviction is aborted comes back
    /// as durable as it was (its bytes on disk did not change); every other transition clears it.
    ///
    /// The `durable` bit therefore stays through `evicting`: an evicting cell's byte may be
    /// `0x84`. An implementation's CAS expects the byte it loaded (this method computes the
    /// byte after it), never the bare state constant, or a durable cell would never be won.
    #[must_use]
    pub const fn apply(self, current: CellByte) -> Option<CellByte> {
        // The byte must be exactly the predecessor state, with or without `durable`: a byte with
        // any of bits 3 to 6 set is no state any transition starts from.
        match current.0 & !DURABLE_BIT {
            bits if bits == self.from().bits() => {
                let durable = match self {
                    Self::Durable => true,
                    Self::BeginEvict | Self::AbortEvict => current.durable(),
                    _ => false,
                };
                Some(CellByte::new(self.to(), durable))
            }
            _ => None,
        }
    }
}
