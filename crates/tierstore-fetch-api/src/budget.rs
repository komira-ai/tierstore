//! `Budget`: flow control of a build node against what each drive and storage node advertise
//! (4.3). T4 implements the three levels (per drive, per storage node, client ceiling), the
//! ladders and the demand reserve.

use tierstore_format::ids::NodeId;

use crate::ids::Class;
use crate::upstream::{DeviceRef, DriveSignals};

/// What one part costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charge {
    /// The part's class (the demand reserve applies to `demand`).
    pub class: Class,
    /// Parts against the client ceiling: 1, or `k` for a degraded read.
    pub parts: u32,
    /// One permit on each of these devices: the serving device, or the `k` source drives of a
    /// degraded read.
    pub devices: Vec<DeviceRef>,
    /// The storage node the part goes to.
    pub node: Option<NodeId>,
}

/// A granted charge. Dropping it returns the permits.
#[must_use = "dropping a permit returns it at once"]
pub struct Permit {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl Permit {
    /// A permit that runs `release` once when dropped.
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        // `release` is `Some` until this drop takes it; the option only lets a `FnOnce` run from
        // `&mut self`.
        self.release
            .take()
            .into_iter()
            .for_each(|release| release());
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit").finish_non_exhaustive()
    }
}

/// How a response ended, for the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Bytes arrived.
    Served,
    /// `503 Retry-After` from the drive: a decrease signal for that drive alone.
    Busy,
    /// `stalled` or `indexing`: not a load signal; no permit changes.
    NotLoad,
    /// A transport failure.
    Transport,
}

/// One observation the controller learns from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// A response from `device`.
    Response {
        /// The device that served (or refused) it.
        device: DeviceRef,
        /// The pack size class the part belongs to (4.4: signals are normalized per size class).
        size_class: u8,
        /// Milliseconds from the request to the first byte, if one came.
        first_byte_ms: Option<u32>,
        /// The headers' signals.
        signals: DriveSignals,
        /// How it ended.
        outcome: Outcome,
    },
    /// Network time per MiB of a finished part from `node`: `(last byte - first byte) / size`.
    Network {
        /// The storage node.
        node: Option<NodeId>,
        /// Milliseconds per MiB.
        ms_per_mib: u32,
    },
    /// The jobscheduler's hint of active clients (`clients_active`; missing or older than 10 s
    /// means `N`).
    ClientsActive(u32),
}

/// The client's budget.
///
/// Section 14 rows: "Budget: floor, share, ceiling and bytes in flight equal the table of 4.3";
/// "Scripted 2x first-byte or drive queue wait with every node at its floor: the ladder steps
/// down"; "Demand reserve granted with prepare saturating the node"; "Controller with three
/// levels"; "Budget with one storage node down (fake cluster)"; "Degraded read: ... the client
/// charges `k` parts".
#[async_trait::async_trait]
pub trait Budget: Send + Sync {
    /// Waits until `charge` fits every level, and takes it. Cancel-safe: dropping the future
    /// before it completes takes no permit and leaves no reservation behind.
    async fn acquire(&self, charge: Charge) -> Permit;

    /// Feeds the controller.
    fn observe(&self, observation: Observation);

    /// The current part size `P` for a pack of `pack_size` bytes (4.4: one client-level value per
    /// size class, never per drive).
    fn part_size(&self, pack_size: u64) -> u64;
}
