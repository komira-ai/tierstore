//! The quantities of 4.3, stated separately and computed from the topology, the number of active
//! clients and the current part size `P`. Pure: no clock, no state.
//!
//! | Quantity | Formula |
//! |---|---|
//! | Cold stream capacity | `G = s* x D` parts (an install without a cold tier: its hot devices' queue depth) |
//! | Per-client share | `share = max(1, ceil(G / clients_active))` parts |
//! | Per-client floor | 1 part at `P_floor`, plus the demand reserve |
//! | Per-client ceiling | `2 x max(share, floor)` parts, or the measured knee |
//! | Bytes in flight at share | `share x P` |
//! | Per-drive permits | `per_drive_max`, one value per drive |
//! | Per-storage-node permits | `node_permits`, unbounded until T0-S |

use crate::config::{BudgetConfig, CEILING_FLOOR};

/// The per-client floor in parts (4.3: one part at depth 1 at `P_floor`).
pub const FLOOR_PARTS: u32 = 1;

/// The quantities of 4.3 for one `(topology, clients_active, P)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantities {
    /// The active clients the share divides by (the hint, or `N`).
    pub clients_active: u32,
    /// `G`, in parts.
    pub stream_capacity: u32,
    /// The per-client share, in parts; at least one whatever `P` is.
    pub share: u32,
    /// The per-client floor, in parts (one).
    pub floor_parts: u32,
    /// The per-client floor in bytes: one part at `P_floor`.
    pub floor_bytes: u64,
    /// The client ceiling, in parts.
    pub ceiling_parts: u32,
    /// The demand reserve at that ceiling: `max(1, reserve_percent of the ceiling)`, rounded up.
    pub demand_reserve: u32,
    /// The part size the bytes are computed at.
    pub part_size: u64,
    /// `share x P`.
    pub bytes_in_flight_at_share: u64,
    /// `per_drive_max` of a healthy hard drive.
    pub drive_permits: u32,
    /// `node_permits`; `None` is unbounded.
    pub node_permits: Option<u32>,
}

impl Quantities {
    /// The quantities for `config` with `clients_active` clients (at least one) at part size
    /// `part_size`.
    #[must_use]
    pub fn compute(config: &BudgetConfig, clients_active: u32, part_size: u64) -> Self {
        let t = &config.topology;
        let clients_active = clients_active.max(1);
        let stream_capacity = match t.cold_drives {
            0 => t.hot_devices.saturating_mul(t.hot_queue_depth),
            d => d.saturating_mul(t.drive_streams),
        };
        let share = stream_capacity.div_ceil(clients_active).max(1);
        let ceiling_parts = config
            .ceiling_parts
            .unwrap_or_else(|| share.max(FLOOR_PARTS).saturating_mul(2))
            .max(CEILING_FLOOR);
        Self {
            clients_active,
            stream_capacity,
            share,
            floor_parts: FLOOR_PARTS,
            floor_bytes: u64::from(FLOOR_PARTS) * config.part_floor,
            ceiling_parts,
            demand_reserve: reserve(config, ceiling_parts),
            part_size,
            bytes_in_flight_at_share: u64::from(share) * part_size,
            drive_permits: config.drive_permits,
            node_permits: config.node_permits,
        }
    }
}

/// The demand reserve of a ceiling of `ceiling` parts: `max(1 part, reserve_percent of it)`,
/// rounded up, and never the whole ceiling while it holds more than one part.
#[must_use]
pub fn reserve(config: &BudgetConfig, ceiling: u32) -> u32 {
    let pct = (u64::from(ceiling) * u64::from(config.reserve_percent)).div_ceil(100);
    u32::try_from(pct)
        .unwrap_or(u32::MAX)
        .max(1)
        .min(ceiling.saturating_sub(1).max(1))
}
