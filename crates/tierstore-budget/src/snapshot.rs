//! What the controller holds at one moment, for the metrics of section 12 (`budget_ceiling_parts`,
//! `budget_reason`, `budget_ladder_step`, `drive_permits{drive}`, `drive_decrease_state{drive}`),
//! for the engine's hedge rule (no hedge to or from a device in a decrease state, none while the
//! ceiling is in one, 4.6) and for tests.

use std::collections::BTreeMap;

use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::NodeId;
use tierstore_metrics::labels::BudgetReason;

use crate::device::DriveStep;
use crate::level::LevelStep;
use crate::quantities::Quantities;

/// The controller's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The quantities of 4.3 at the active clients and the current `P` of the largest class.
    pub quantities: Quantities,
    /// The client ceiling in force, in parts.
    pub ceiling: u32,
    /// Parts in flight against the ceiling (a degraded read counts its `k`).
    pub parts_in_flight: u32,
    /// The ceiling's ladder step.
    pub ceiling_step: LevelStep,
    /// How many times `P` is halved from each class's steady size.
    pub part_steps: u8,
    /// Why the controller last moved (or held).
    pub reason: BudgetReason,
    /// Every storage node the client has sent a part to.
    pub nodes: BTreeMap<NodeId, NodeView>,
    /// Every device the client has traffic with; a device it has none with has no state.
    pub devices: BTreeMap<DeviceRef, DeviceView>,
}

/// One storage node's level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeView {
    /// The parts in flight it allows; `None` is unbounded.
    pub limit: Option<u32>,
    /// Parts in flight to it.
    pub in_flight: u32,
    /// Its ladder step.
    pub step: LevelStep,
}

/// One device's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceView {
    /// Its permits now.
    pub permits: u32,
    /// `per_drive_max`.
    pub max: u32,
    /// Parts in flight on it.
    pub in_flight: u32,
    /// Demand charges waiting with it among their devices (one of its permits is held for them).
    pub demand_waiting: u32,
    /// The pacing gap between part starts, 0 when not paced.
    pub pace_ms: u64,
    /// Whether it is in a decrease state.
    pub decrease: bool,
    /// Its ladder step.
    pub step: DriveStep,
}
