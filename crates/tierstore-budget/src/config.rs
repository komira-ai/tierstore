//! The budget's configuration: the topology it budgets against (4.3: `D`, `N`, `s*`, the hot
//! devices and their queue depth) and the controller's constants (4.3, 4.4). Every constant is
//! the design's starting value; T0-S and T10b replace them with measured ones.
//!
//! Nothing here names a deployment profile. NOW (six combined nodes: twelve SMR drives at one
//! stream each, six SATA SSD hot partitions, six clients) and LATER (three storage nodes: 45 HDDs
//! at two streams, three NVMe, 31 clients) are two [`Topology`] values; the code does not branch
//! on which one it is given.

const MIB: u64 = 1 << 20;

/// What the budget is flow control against: the storage tier's devices and the number of
/// clients sharing them (4.3), as the membership document and `drive.streams` give them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topology {
    /// `D`: the cold tier's hard drives (0 for an install without a cold tier).
    pub cold_drives: u32,
    /// The hot devices (an NVMe, or the SSD partition of a combined node), one per storage node.
    pub hot_devices: u32,
    /// `N`: the clients that share the storage tier (the build nodes, or every combined node).
    pub clients: u32,
    /// `s*`: the streams each hard drive admits at once (`drive.streams`, 10.7).
    pub drive_streams: u32,
    /// The queue depth each hot device admits (`hot.queue_depth`, 10.7).
    pub hot_queue_depth: u32,
}

/// The budget's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetConfig {
    /// The devices and clients.
    pub topology: Topology,
    /// The client ceiling in parts once T0-S has measured the knee; `None` is the design's
    /// starting value `2 x max(share, floor)`.
    pub ceiling_parts: Option<u32>,
    /// `node_permits[n]`: the parts in flight to one storage node; `None` is unbounded (bounded
    /// only by the client ceiling) until T0-S measures the knee.
    pub node_permits: Option<u32>,
    /// `per_drive_max` of a healthy hard drive (4.3: 1 or 2; `s*` by default).
    pub drive_permits: u32,
    /// `per_drive_max` of a hot device (4.3: a larger value for an NVMe; its queue depth by
    /// default).
    pub hot_permits: u32,
    /// The steady part size `P` of each size class of `tierstore_fetch::plan::size_class` (4.4:
    /// packs up to 32 MiB take no grid, up to 512 MiB 32 MiB, above 64 MiB).
    pub part_sizes: [u64; 3],
    /// `P_floor`: the smallest part size the ladder steps `P` down to (4.3: 16 MiB).
    pub part_floor: u64,
    /// The window is `max(window_parts parts, window_ms)` (4.3: 16 parts, 2 s).
    pub window_parts: u32,
    /// ... and its time floor, in milliseconds.
    pub window_ms: u64,
    /// The span of the windowed minimum a baseline is (4.3: 10 minutes).
    pub baseline_ms: u64,
    /// A drive counts as "with traffic" for the node and client levels while its last answer is
    /// at most this old (one baseline span).
    pub traffic_ms: u64,
    /// The jobscheduler's hint of active clients is used while it is at most this old (4.3:
    /// 10 s); older or missing, the budget uses `N`.
    pub hint_max_age_ms: u64,
    /// The smallest baseline of a first-byte or queue-wait signal, in milliseconds: an idle
    /// drive's queue wait is about zero, and twice zero is not a load signal.
    pub signal_floor_ms: u32,
    /// The smallest baseline of the network signal, in milliseconds per MiB.
    pub network_floor_ms_per_mib: u32,
    /// The first pacing gap between part starts to a drive at one permit (4.3: doubling per
    /// window up to 2 s).
    pub pace_start_ms: u64,
    /// The largest pacing gap (4.3: 2 s).
    pub pace_max_ms: u64,
    /// The demand reserve as a percentage of the client ceiling, at least one part (4.3: 25%).
    pub reserve_percent: u32,
    /// The seed of the cooldown's jitter (4.3: one window, +-50%).
    pub jitter_seed: u64,
}

impl BudgetConfig {
    /// The design's starting constants for `topology`.
    #[must_use]
    pub fn new(topology: Topology) -> Self {
        Self {
            topology,
            ceiling_parts: None,
            node_permits: None,
            // A hot-only install has no stream count; its (unused) drive permits are one.
            drive_permits: topology.drive_streams.max(1),
            hot_permits: topology.hot_queue_depth,
            part_sizes: [32 * MIB, 32 * MIB, 64 * MIB],
            part_floor: 16 * MIB,
            window_parts: 16,
            window_ms: 2_000,
            baseline_ms: 600_000,
            traffic_ms: 600_000,
            hint_max_age_ms: 10_000,
            signal_floor_ms: 10,
            network_floor_ms_per_mib: 1,
            pace_start_ms: 250,
            pace_max_ms: 2_000,
            reserve_percent: 25,
            jitter_seed: 0x9e37_79b9_7f4a_7c15,
        }
    }

    /// Refuses a configuration the budget cannot run: one that could never grant a part, or
    /// whose ladder has no step.
    ///
    /// # Errors
    ///
    /// The first [`ConfigError`] found, naming the key and its value.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let t = &self.topology;
        let whole = |p: u64| p != 0 && p.is_multiple_of(MIB);
        let bad_part = std::iter::once(self.part_floor)
            .filter(|&p| !whole(p))
            .chain(
                self.part_sizes
                    .iter()
                    .copied()
                    .filter(|&p| !whole(p) || p < self.part_floor),
            )
            .next();
        let checks: [(bool, ConfigError); 10] = [
            (t.clients == 0, ConfigError::NoClients),
            (
                t.cold_drives > 0 && t.drive_streams == 0,
                ConfigError::NoDriveStreams,
            ),
            (
                self.drive_permits == 0,
                ConfigError::ZeroPermits("drive_permits"),
            ),
            (
                self.hot_permits == 0,
                ConfigError::ZeroPermits("hot_permits"),
            ),
            (
                self.node_permits == Some(0),
                ConfigError::ZeroPermits("node_permits"),
            ),
            (
                self.ceiling_parts.is_some_and(|c| c < CEILING_FLOOR),
                ConfigError::CeilingBelowFloor(self.ceiling_parts.unwrap_or(0)),
            ),
            (
                bad_part.is_some(),
                ConfigError::PartSize(bad_part.unwrap_or(0)),
            ),
            (self.window_parts == 0, ConfigError::EmptyWindow),
            (
                self.pace_start_ms == 0 || self.pace_start_ms > self.pace_max_ms,
                ConfigError::Pacing {
                    start_ms: self.pace_start_ms,
                    max_ms: self.pace_max_ms,
                },
            ),
            (
                self.reserve_percent > 100,
                ConfigError::Reserve(self.reserve_percent),
            ),
        ];
        checks
            .into_iter()
            .find_map(|(bad, e)| bad.then_some(e))
            .map_or(Ok(()), Err)
    }
}

/// The smallest client ceiling: the floor's one part plus the demand reserve's one part (4.3).
pub const CEILING_FLOOR: u32 = 2;

/// A configuration the budget refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `topology.clients` is zero: the share has no divisor.
    #[error("budget: topology.clients is 0; at least one client shares the storage tier")]
    NoClients,
    /// A cold tier with `drive_streams = 0` could never serve a part.
    #[error(
        "budget: topology.drive_streams is 0 with a cold tier; a drive admits at least one stream"
    )]
    NoDriveStreams,
    /// A permit count of zero could never grant a part.
    #[error("budget: {0} is 0; a device or node admits at least one part")]
    ZeroPermits(&'static str),
    /// A measured ceiling below the floor of one part plus the demand reserve.
    #[error(
        "budget: ceiling_parts is {0}, below the floor of {CEILING_FLOOR} (one part plus the demand reserve)"
    )]
    CeilingBelowFloor(u32),
    /// A part size that is zero, not a whole number of 1 MiB cells, or below `part_floor`.
    #[error("budget: part size {0} is zero, not a multiple of 1 MiB, or below part_floor")]
    PartSize(u64),
    /// `window_parts` is zero.
    #[error("budget: window_parts is 0; a window holds at least one part")]
    EmptyWindow,
    /// A pacing ladder with no first step, or whose first step is above its cap.
    #[error("budget: pace_start_ms {start_ms} must be above 0 and at most pace_max_ms {max_ms}")]
    Pacing {
        /// `pace_start_ms`.
        start_ms: u64,
        /// `pace_max_ms`.
        max_ms: u64,
    },
    /// A demand reserve above the whole ceiling.
    #[error("budget: reserve_percent is {0}, above 100")]
    Reserve(u32),
}
