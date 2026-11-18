//! Shared helpers of the budget's tests: the two deployment profiles of section 21 as
//! topologies, a budget on a manual clock, and the observations that close a window.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "each test binary uses a different subset of the helpers; a failed expect is the failure a test reports"
)]

use std::sync::Arc;

use futures::FutureExt;
use tierstore_budget::{BudgetConfig, ClientBudget, Topology};
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch_api::budget::{Budget, Charge, Observation, Outcome, Permit};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::upstream::{DeviceRef, DriveSignals, DriveState};
use tierstore_format::ids::{DriveId, NodeId};

pub const MIB: u64 = 1 << 20;

/// More than the longest cooldown (one 2 s window + 50%): a window closed after this many
/// milliseconds is never inside the cooldown of the decrease before it.
pub const PAST_COOLDOWN: u64 = 3_001;

/// LATER (21.2): three storage nodes of one NVMe and 15 HDDs at two streams, 31 build nodes.
pub fn later() -> Topology {
    Topology {
        cold_drives: 45,
        hot_devices: 3,
        clients: 31,
        drive_streams: 2,
        hot_queue_depth: 64,
    }
}

/// NOW (21.1): six combined nodes of two SMR drives at one stream and an SSD hot partition.
pub fn now_profile() -> Topology {
    Topology {
        cold_drives: 12,
        hot_devices: 6,
        clients: 6,
        drive_streams: 1,
        hot_queue_depth: 64,
    }
}

pub struct Rig {
    pub budget: ClientBudget,
    pub clock: ManualClock,
}

impl Rig {
    pub fn new(config: BudgetConfig) -> Self {
        let clock = ManualClock::new();
        let timer = Arc::new(ManualTimer::new(clock.clone()));
        let budget = ClientBudget::new(config, timer).expect("valid config");
        Self { budget, clock }
    }

    pub fn later() -> Self {
        Self::new(BudgetConfig::new(later()))
    }

    pub fn advance(&self, ms: u64) {
        self.clock.advance(ms);
    }

    /// The charge, granted at once, or `None` if it would wait (the pending acquire is dropped,
    /// which takes nothing).
    pub fn try_acquire(&self, charge: Charge) -> Option<Permit> {
        self.budget.acquire(charge).now_or_never()
    }

    pub fn acquire(&self, charge: Charge) -> Permit {
        self.try_acquire(charge).expect("charge fits now")
    }

    pub fn observe(&self, o: Observation) {
        self.budget.observe(o);
    }

    /// One answer from `device` with these signals.
    pub fn served(&self, device: DeviceRef, first_byte_ms: u32, queue_wait_ms: u32) {
        self.observe(response(
            device,
            Some(first_byte_ms),
            signals(Some(queue_wait_ms), None),
            Outcome::Served,
        ));
    }

    /// Closes one window of `device` (its first, or one opened by the last close at least
    /// [`PAST_COOLDOWN`] ago) with every answer at these signals.
    pub fn drive_window(&self, device: DeviceRef, first_byte_ms: u32, queue_wait_ms: u32) {
        for _ in 0..15 {
            self.served(device, first_byte_ms, queue_wait_ms);
        }
        self.advance(PAST_COOLDOWN);
        self.served(device, first_byte_ms, queue_wait_ms);
    }

    /// Closes one window of every device in `devices` at the same signals.
    pub fn drive_windows(&self, devices: &[DeviceRef], first_byte_ms: u32, queue_wait_ms: u32) {
        for d in devices {
            for _ in 0..15 {
                self.served(*d, first_byte_ms, queue_wait_ms);
            }
        }
        self.advance(PAST_COOLDOWN);
        for d in devices {
            self.served(*d, first_byte_ms, queue_wait_ms);
        }
    }

    /// Closes one network window of `node` (and so one of the client) at `ms_per_mib`.
    pub fn network_window(&self, node: Option<NodeId>, ms_per_mib: u32) {
        for _ in 0..15 {
            self.observe(Observation::Network { node, ms_per_mib });
        }
        self.advance(PAST_COOLDOWN);
        self.observe(Observation::Network { node, ms_per_mib });
    }

    pub fn busy(&self, device: DeviceRef) {
        self.observe(response(
            device,
            None,
            DriveSignals::default(),
            Outcome::Busy,
        ));
    }
}

pub fn response(
    device: DeviceRef,
    first_byte_ms: Option<u32>,
    signals: DriveSignals,
    outcome: Outcome,
) -> Observation {
    Observation::Response {
        device,
        size_class: 2,
        first_byte_ms,
        signals,
        outcome,
    }
}

pub fn signals(queue_wait_ms: Option<u32>, state: Option<DriveState>) -> DriveSignals {
    DriveSignals {
        queue_wait_ms,
        service_ms: None,
        drive_queue: None,
        drive_state: state,
    }
}

/// Drive `drive` of storage node `node` (its id holds both numbers).
pub fn drive(node: u8, drive: u16) -> DeviceRef {
    let mut id = [0; 16];
    id[0] = node;
    id[14..].copy_from_slice(&drive.to_be_bytes());
    DeviceRef::Drive(NodeId(node), DriveId(id))
}

/// Every hard drive of `node` under LATER (15).
pub fn drives_of(node: u8) -> Vec<DeviceRef> {
    (0..15).map(|d| drive(node, d)).collect()
}

/// One part from one device of `node`.
pub fn part(class: Class, device: DeviceRef) -> Charge {
    Charge {
        class,
        parts: 1,
        devices: vec![device],
        node: Some(device.node()),
    }
}

/// A degraded part: `k` parts to the reconstructor `node`, one permit on each source drive.
pub fn degraded(class: Class, node: u8, sources: &[DeviceRef]) -> Charge {
    Charge {
        class,
        parts: u32::try_from(sources.len()).expect("few sources"),
        devices: sources.to_vec(),
        node: Some(NodeId(node)),
    }
}

/// A charge on the ceiling alone (no device, no node).
pub fn bare(class: Class) -> Charge {
    Charge {
        class,
        parts: 1,
        devices: Vec::new(),
        node: None,
    }
}
