//! The registry: every group of families, made from the node's configuration, rendered for its
//! role.

use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_store_api::view::Role;

use crate::family::seal::Seed;
use crate::family::{FamilyInfo, Registered};
use crate::groups::{Cache, Cold, Drives, Fetch, Front, Fuse, Hot, Movement, Node};
use crate::labels::Disk;

/// The side of a node a group belongs to, which decides the roles that render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// The build-node read path: rendered by `build` and `combined` nodes.
    Build,
    /// The storage tier: rendered by `storage` and `combined` nodes.
    Storage,
    /// Rendered by every node.
    Node,
}

impl Side {
    /// Whether a node of `role` renders this side.
    fn rendered_by(self, role: Role) -> bool {
        match self {
            Self::Build => role != Role::Storage,
            Self::Storage => role.stores(),
            Self::Node => true,
        }
    }
}

/// What the registry needs of a node's configuration (10.14, section 21): its role, and the ids
/// and settings its labels take values from. Each profile of section 21 is a value of this (for
/// example NOW: `combined`, two hard drives and a hot SSD partition per node, `hot.replicas = 2`,
/// `4+2`; LATER: a `storage` node with 15 hard drives and an NVMe device, `hot.replicas = 3`,
/// `4+4`, or a `build` node with only its cache); nothing branches on which profile it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    /// The node's role: which groups it renders.
    pub role: Role,
    /// The release the process runs (`tierstore_build_info{version}`).
    pub version: &'static str,
    /// Every drive of the cluster the fetch engine may read from (the `{drive}` of the build
    /// side's families), hard drives and hot devices.
    pub cluster_drives: Vec<DriveId>,
    /// The node's own drives, hard drives and hot device (the `{drive}` of the storage side's
    /// families and the drives of the per-disk gauges).
    pub local_drives: Vec<DriveId>,
    /// The other storage nodes, the `{node}` of the hot tier's replica families.
    pub peers: Vec<NodeId>,
    /// `hot.replicas`.
    pub hot_replicas: u8,
    /// The stripe codes the cold tier holds (the configured code, and during a conversion or a
    /// migration the code it moves from).
    pub codes: Vec<Code>,
}

/// Every family of section 12, by group. Each group is a public field whose families are public
/// fields; a family's series is reached with its label values (`with`, or the shorthand of each
/// kind: `inc`, `set`, `observe`).
///
/// Every group exists whatever the role, so code never asks which role it runs in to count;
/// [`Metrics::render`] writes only the groups of the node's role (section 21: a `combined` node
/// renders both sides).
pub struct Metrics {
    role: Role,
    /// Every node.
    pub node: Node,
    /// The fetch engine and its budget.
    pub fetch: Fetch,
    /// The node cache.
    pub cache: Cache,
    /// The FUSE layer and mounts.
    pub fuse: Fuse,
    /// The hot tier.
    pub hot: Hot,
    /// The S3 front, index, audit and migration.
    pub front: Front,
    /// The cold tier.
    pub cold: Cold,
    /// Destage, promotion and GC.
    pub movement: Movement,
    /// The node's drives.
    pub drives: Drives,
}

impl Metrics {
    /// The registry of a node of `topology`, every seeded series at zero and
    /// `tierstore_build_info` at 1.
    #[must_use]
    pub fn new(topology: &Topology) -> Self {
        let mut disks: Vec<Disk> = Vec::new();
        if topology.role != Role::Storage {
            disks.push(Disk::Cache);
        }
        if topology.role.stores() {
            disks.extend(topology.local_drives.iter().copied().map(Disk::Drive));
        }
        let seed = |drives| Seed {
            drives,
            nodes: &topology.peers,
            disks: &disks,
            version: topology.version,
            hot_replicas: topology.hot_replicas,
            codes: &topology.codes,
        };
        let build = seed(&topology.cluster_drives);
        let storage = seed(&topology.local_drives);
        let metrics = Self {
            role: topology.role,
            node: Node::new(&storage),
            fetch: Fetch::new(&build),
            cache: Cache::new(&build),
            fuse: Fuse::new(&build),
            hot: Hot::new(&storage),
            front: Front::new(&storage),
            cold: Cold::new(&storage),
            movement: Movement::new(&storage),
            drives: Drives::new(&storage),
        };
        for version in metrics.node.tierstore_build_info.labels() {
            metrics.node.tierstore_build_info.set(version, 1.0);
        }
        metrics
    }

    /// The node's role.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// Every family of every group, the ones the role does not render included, in the order
    /// [`Metrics::render`] writes them.
    #[must_use]
    pub fn families(&self) -> Vec<FamilyInfo> {
        self.groups()
            .into_iter()
            .flat_map(|(_, families)| families)
            .map(|f| f.info())
            .collect()
    }

    /// The families the node's role renders.
    #[must_use]
    pub fn rendered_families(&self) -> Vec<FamilyInfo> {
        self.groups()
            .into_iter()
            .filter(|(side, _)| side.rendered_by(self.role))
            .flat_map(|(_, families)| families)
            .map(|f| f.info())
            .collect()
    }

    /// The text of every family the node's role renders, in the Prometheus text format (0.0.4):
    /// per family a `# HELP` and a `# TYPE` line, then its series.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (side, families) in self.groups() {
            if side.rendered_by(self.role) {
                for family in families {
                    family.render(&mut out);
                }
            }
        }
        out
    }

    /// Every group, with its side, in render order.
    fn groups(&self) -> Vec<(Side, Vec<&dyn Registered>)> {
        vec![
            (Node::SIDE, self.node.families()),
            (Fetch::SIDE, self.fetch.families()),
            (Cache::SIDE, self.cache.families()),
            (Fuse::SIDE, self.fuse.families()),
            (Hot::SIDE, self.hot.families()),
            (Front::SIDE, self.front.families()),
            (Cold::SIDE, self.cold.families()),
            (Movement::SIDE, self.movement.families()),
            (Drives::SIDE, self.drives.families()),
        ]
    }
}
