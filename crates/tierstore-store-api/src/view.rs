//! The membership view (10.8) and the quorum rules every route applies (10.2, 10.6).
//!
//! The membership document (`membership.toml`, distributed by hostkeeper) names the epoch, the
//! cluster settings every node must agree on, the storage nodes and the drives, each `in`,
//! `drain` or `out`. A node's view adds the states it observes or is told (`up`, `indexing`,
//! `stalled`, `suspect`, `down`, `failed`). T-S1 parses and validates the document; this module
//! fixes what the rest of the store reads from it. Nothing branches on which deployment profile
//! (section 21) runs: a profile is only this document and the configuration.

use serde::{Deserialize, Serialize};
use tierstore_fetch_api::upstream::DriveState;
use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};

/// What the membership document says of a node's hot device or of a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Admin {
    /// A placement target.
    In,
    /// Serves reads; no new placements; emptied by the migration (21.3).
    Drain,
    /// Not a candidate; its data is rebuilt elsewhere.
    Out,
}

/// The roles of a node (10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A storage node: S3 front, peer service, hot and cold stores.
    Storage,
    /// A build node: cache, mount, fetch engine; no durable data.
    Build,
    /// Both, in one process (the NOW profile, section 21).
    Combined,
}

impl Role {
    /// Whether the node runs the storage roles.
    #[must_use]
    pub const fn stores(self) -> bool {
        matches!(self, Self::Storage | Self::Combined)
    }
}

/// A node's state in a viewer's view (10.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    /// Answering.
    Up,
    /// Its index is not built: it answers `indexing`, never a miss.
    Indexing,
    /// Three failed pings or error responses; last in preference.
    Suspect,
    /// No ping answered for three intervals; not asked.
    Down,
}

/// The cluster settings every node must agree on (10.8); a node whose configuration disagrees
/// refuses to start. They take effect with the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClusterSettings {
    /// `hot.replicas`.
    pub hot_replicas: u8,
    /// `hot.min_acks`.
    pub hot_min_acks: u8,
    /// The cold code for new stripes; `None` for a hot-only install (`cold.mode = "none"`).
    pub cold_code: Option<Code>,
}

/// A node of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeView {
    /// The node's id.
    pub id: NodeId,
    /// Its failure domain.
    pub domain: u16,
    /// Its role.
    pub role: Role,
    /// Its hot device's admin state.
    pub hot: Admin,
    /// Its state in this view.
    pub state: NodeState,
}

/// A drive of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveView {
    /// The drive's id.
    pub id: DriveId,
    /// Its node.
    pub node: NodeId,
    /// Its admin state.
    pub admin: Admin,
    /// Its state in this view (a `down` node's drives are `down`).
    pub state: DriveState,
    /// Its capacity in bytes (the membership's weight).
    pub capacity: u64,
    /// Its free bytes (`statfs`), the weight of placement (10.4).
    pub free: u64,
    /// Whether its fill is at or above `cold.fill_limit`.
    pub above_fill_limit: bool,
}

/// A node's view of the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The membership epoch.
    pub epoch: u64,
    /// The cluster settings of the epoch.
    pub settings: ClusterSettings,
    /// The nodes of the document (storage and combined; build nodes are not listed).
    pub nodes: Vec<NodeView>,
    /// The drives of the document.
    pub drives: Vec<DriveView>,
}

impl View {
    /// The node `id`.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&NodeView> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The storage nodes that are live: up and not `out` (a node whose hot device is `out` and
    /// that has no drive left is still listed, but it is not a candidate).
    pub fn live_nodes(&self) -> impl Iterator<Item = &NodeView> {
        self.nodes
            .iter()
            .filter(|n| n.role.stores() && n.state == NodeState::Up && n.hot != Admin::Out)
    }

    /// The drives of `node`.
    pub fn drives_of(&self, node: NodeId) -> impl Iterator<Item = &DriveView> {
        self.drives.iter().filter(move |d| d.node == node)
    }
}

/// The number of `absent` answers, from live nodes none of them `indexing`, that make a miss
/// definitive (10.6 step 3): `N - min_acks + 1`, where `N` is the number of storage nodes. An
/// acknowledged object is indexed on at least `min_acks` nodes, and any `N - min_acks + 1` nodes
/// include one of them. `None` when the settings cannot be met (`min_acks` 0 or above `N`).
#[must_use]
pub fn definitive_miss_answers(storage_nodes: usize, min_acks: u8) -> Option<usize> {
    let acks = usize::from(min_acks);
    (acks >= 1 && acks <= storage_nodes).then(|| storage_nodes - acks + 1)
}

/// The nodes whose durable copy commits a majority-committed record (a seal, a descriptor, a
/// stripe free; 10.2): `floor(nodes / 2) + 1`. A single-node install commits on its own.
#[must_use]
pub const fn majority(storage_nodes: usize) -> usize {
    storage_nodes / 2 + 1
}
