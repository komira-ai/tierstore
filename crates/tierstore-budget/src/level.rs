//! The state of a storage node's level and of the client ceiling (4.3, "Per storage node n" and
//! "Client ceiling"): a limit on the parts in flight, a decrease state and cooldown, a window
//! and the baseline of the network signal of that scope.

use crate::signal::{Series, Window};

/// The step of a level's ladder (`budget_ladder_step`, section 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LevelStep {
    /// Not in a decrease state.
    None,
    /// In a decrease state, with fewer parts in flight.
    FewerParts,
    /// In a decrease state at its floor of parts: `P` stepped toward 16 MiB.
    SmallerPart,
}

impl LevelStep {
    /// The gauge value: 0 none, 1 fewer parts, 2 a smaller part size.
    #[must_use]
    pub const fn gauge(self) -> u8 {
        match self {
            Self::None => 0,
            Self::FewerParts => 1,
            Self::SmallerPart => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Level {
    /// The parts in flight it allows; `None` is unbounded (a node level before T0-S).
    pub limit: Option<u32>,
    pub in_flight: u32,
    pub decrease: bool,
    /// Whether its last decrease stepped `P` rather than the parts.
    pub stepped_part: bool,
    pub cooldown_until: u64,
    /// Consecutive windows with the network signal above twice its baseline.
    pub above: u32,
    pub window: Window,
    pub network: Series,
}

impl Level {
    pub fn new(limit: Option<u32>, now: u64) -> Self {
        Self {
            limit,
            in_flight: 0,
            decrease: false,
            stepped_part: false,
            cooldown_until: 0,
            above: 0,
            window: Window::at(now),
            network: Series::default(),
        }
    }

    pub fn step(&self) -> LevelStep {
        match (self.decrease, self.stepped_part) {
            (false, _) => LevelStep::None,
            (true, false) => LevelStep::FewerParts,
            (true, true) => LevelStep::SmallerPart,
        }
    }

    /// Whether `parts` more fit under `limit` (a level with nothing in flight takes any charge,
    /// so a charge larger than the limit, a degraded read's `k`, still runs).
    pub fn fits(in_flight: u32, parts: u32, limit: Option<u32>) -> bool {
        in_flight == 0 || limit.is_none_or(|l| in_flight.saturating_add(parts) <= l)
    }

    /// One step down the parts ladder from `eff` (the limit in force): `x 0.75` rounded down and
    /// at least one part, not below `floor`. `false` when already at the floor: the next step of
    /// the ladder is `P`.
    pub fn step_down(&mut self, eff: u32, floor: u32) -> bool {
        if eff > floor {
            self.limit = Some((eff * 3 / 4).min(eff - 1).max(floor));
            true
        } else {
            false
        }
    }
}
