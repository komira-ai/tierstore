//! The registry: the families of the components a process runs, each group registered by the
//! component that updates it when that component is configured.
//!
//! There is no role here. A process makes one [`Metrics`] and each component it enables
//! registers its families with its own configuration: the hot store, the cold store, the peer
//! service, the S3 front, the node cache, the fetch engine, the FUSE front, the NFS front, the
//! Prepare provider. A deployment profile of section 21 is only a set of enabled components and
//! their configurations (the tests check the output of the NOW and LATER examples). A family
//! more than one component uses (`fdatasync_seconds`, the per-disk and per-drive families, the
//! read-reply bytes, the copy-generation mismatches) is in a shared group that is made once;
//! each component adds its own series to it.

use std::sync::{Arc, OnceLock};

use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};

use crate::family::seal::Seed;
use crate::family::{FamilyInfo, Registered};
use crate::groups::{
    Cache, Cold, CopyGeneration, Disks, Drives, Durability, Fetch, Fuse, Hot, Movement, Peer,
    Prepare, Process, ReadReply, S3Front,
};
use crate::labels::Disk;

/// The hot store's configuration, as its families need it (10.3, 10.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotStoreConfig {
    /// The hot device's drive id.
    pub device: DriveId,
    /// `hot.replicas`: `{copies}` runs from 0 to it.
    pub replicas: u8,
    /// The other storage nodes, the `{node}` of the replica families.
    pub peers: Vec<NodeId>,
}

/// The cold store's configuration, as its families need it (10.4, 10.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdStoreConfig {
    /// The node's hard drives.
    pub drives: Vec<DriveId>,
    /// The stripe codes the cold tier holds: the configured code, and during a conversion or a
    /// migration (10.5, 21.3) the code it moves from.
    pub codes: Vec<Code>,
}

/// The fetch engine's configuration, as its families need it (section 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchEngineConfig {
    /// Every drive of the cluster the engine reads from, hard drives and hot devices: the
    /// `{drive}` of its budget families.
    pub drives: Vec<DriveId>,
}

/// The families of the hot store.
#[derive(Clone)]
pub struct HotStoreMetrics {
    /// The hot tier.
    pub hot: Arc<Hot>,
    /// The per-drive families, with a series for the hot device.
    pub drives: Arc<Drives>,
    /// The per-disk gauges, with a series for the hot device.
    pub disks: Arc<Disks>,
    /// `fdatasync_seconds`.
    pub durability: Arc<Durability>,
}

/// The families of the cold store.
#[derive(Clone)]
pub struct ColdStoreMetrics {
    /// The cold tier.
    pub cold: Arc<Cold>,
    /// Destage, promotion and GC.
    pub movement: Arc<Movement>,
    /// The per-drive families, with a series for each hard drive.
    pub drives: Arc<Drives>,
    /// The per-disk gauges, with a series for each hard drive.
    pub disks: Arc<Disks>,
}

/// The families of the S3 front.
#[derive(Clone)]
pub struct S3FrontMetrics {
    /// PUTs, LIST and imports.
    pub front: Arc<S3Front>,
    /// `store_copy_generation_mismatch_total`.
    pub copy_generation: Arc<CopyGeneration>,
}

/// The families of the node cache.
#[derive(Clone)]
pub struct NodeCacheMetrics {
    /// Verification, the journal, holds, eviction and the cache device.
    pub cache: Arc<Cache>,
    /// The per-disk gauges, with a series for the cache device.
    pub disks: Arc<Disks>,
    /// `fdatasync_seconds`.
    pub durability: Arc<Durability>,
}

/// The families of the fetch engine.
#[derive(Clone)]
pub struct FetchEngineMetrics {
    /// The fetch engine and its budget.
    pub fetch: Arc<Fetch>,
    /// `store_copy_generation_mismatch_total`.
    pub copy_generation: Arc<CopyGeneration>,
}

/// The families of the FUSE front.
#[derive(Clone)]
pub struct FuseFrontMetrics {
    /// Requests, transport, parking and restarts.
    pub fuse: Arc<Fuse>,
    /// How read replies were sent.
    pub read_reply: Arc<ReadReply>,
}

/// One group's slot: made by the first component that registers it.
type Slot<G> = OnceLock<Arc<G>>;

/// The registry of one process. Each component registers its families with the method of its
/// name; registering again (a second component that shares a group, or the same component with
/// more drives) adds the series of the new configuration to the families already there.
/// [`Metrics::render`] writes every registered family.
pub struct Metrics {
    version: &'static str,
    process: Arc<Process>,
    fetch: Slot<Fetch>,
    prepare: Slot<Prepare>,
    cache: Slot<Cache>,
    fuse: Slot<Fuse>,
    read_reply: Slot<ReadReply>,
    durability: Slot<Durability>,
    disks: Slot<Disks>,
    copy_generation: Slot<CopyGeneration>,
    hot: Slot<Hot>,
    front: Slot<S3Front>,
    peer: Slot<Peer>,
    cold: Slot<Cold>,
    movement: Slot<Movement>,
    drives: Slot<Drives>,
}

/// The seed of a group with only closed labels, and the version.
fn closed(version: &'static str) -> Seed<'static> {
    Seed {
        drives: &[],
        nodes: &[],
        disks: &[],
        version,
        hot_replicas: 0,
        codes: &[],
    }
}

/// The group in `slot`, made now if no component registered it yet, with the series of `seed`.
fn register<G>(
    slot: &Slot<G>,
    new: fn() -> G,
    seed: &Seed<'_>,
    seed_group: fn(&G, &Seed<'_>),
) -> Arc<G> {
    let group = slot.get_or_init(|| Arc::new(new())).clone();
    seed_group(&group, seed);
    group
}

impl Metrics {
    /// The registry of a process running the release `version`, with only the families every
    /// process has: `tierstore_build_info` (at 1) and `blocking_pool_queue_len`.
    #[must_use]
    pub fn new(version: &'static str) -> Self {
        let process = Arc::new(Process::new());
        process.seed(&closed(version));
        for v in process.tierstore_build_info.labels() {
            process.tierstore_build_info.set(v, 1.0);
        }
        Self {
            version,
            process,
            fetch: OnceLock::new(),
            prepare: OnceLock::new(),
            cache: OnceLock::new(),
            fuse: OnceLock::new(),
            read_reply: OnceLock::new(),
            durability: OnceLock::new(),
            disks: OnceLock::new(),
            copy_generation: OnceLock::new(),
            hot: OnceLock::new(),
            front: OnceLock::new(),
            peer: OnceLock::new(),
            cold: OnceLock::new(),
            movement: OnceLock::new(),
            drives: OnceLock::new(),
        }
    }

    /// The families every process has.
    #[must_use]
    pub fn process(&self) -> Arc<Process> {
        self.process.clone()
    }

    fn closed(&self) -> Seed<'static> {
        closed(self.version)
    }

    fn durability(&self) -> Arc<Durability> {
        register(
            &self.durability,
            Durability::new,
            &self.closed(),
            Durability::seed,
        )
    }

    fn disks(&self, disks: &[Disk]) -> Arc<Disks> {
        let seed = Seed {
            disks,
            ..self.closed()
        };
        register(&self.disks, Disks::new, &seed, Disks::seed)
    }

    fn drives(&self, drives: &[DriveId]) -> Arc<Drives> {
        let seed = Seed {
            drives,
            ..self.closed()
        };
        register(&self.drives, Drives::new, &seed, Drives::seed)
    }

    fn copy_generation(&self) -> Arc<CopyGeneration> {
        let seed = self.closed();
        register(
            &self.copy_generation,
            CopyGeneration::new,
            &seed,
            CopyGeneration::seed,
        )
    }

    fn read_reply(&self) -> Arc<ReadReply> {
        register(
            &self.read_reply,
            ReadReply::new,
            &self.closed(),
            ReadReply::seed,
        )
    }

    /// Registers the hot store's families (10.3): the hot tier, and its device in the per-drive
    /// and per-disk families.
    pub fn hot_store(&self, config: &HotStoreConfig) -> HotStoreMetrics {
        let seed = Seed {
            nodes: &config.peers,
            hot_replicas: config.replicas,
            ..self.closed()
        };
        let device = [config.device];
        HotStoreMetrics {
            hot: register(&self.hot, Hot::new, &seed, Hot::seed),
            drives: self.drives(&device),
            disks: self.disks(&[Disk::Drive(config.device)]),
            durability: self.durability(),
        }
    }

    /// Registers the cold store's families (10.4, 10.5, 10.9, 10.12): the cold tier, destage,
    /// promotion and GC, and its drives in the per-drive and per-disk families.
    pub fn cold_store(&self, config: &ColdStoreConfig) -> ColdStoreMetrics {
        let seed = Seed {
            drives: &config.drives,
            codes: &config.codes,
            ..self.closed()
        };
        let disks: Vec<Disk> = config.drives.iter().copied().map(Disk::Drive).collect();
        ColdStoreMetrics {
            cold: register(&self.cold, Cold::new, &seed, Cold::seed),
            movement: register(&self.movement, Movement::new, &seed, Movement::seed),
            drives: self.drives(&config.drives),
            disks: self.disks(&disks),
        }
    }

    /// Registers the peer service's families (10.6, 10.8, 10.9).
    pub fn peer_service(&self) -> Arc<Peer> {
        register(&self.peer, Peer::new, &self.closed(), Peer::seed)
    }

    /// Registers the S3 front's families (10.2, 10.6, 10.10).
    pub fn s3_front(&self) -> S3FrontMetrics {
        S3FrontMetrics {
            front: register(&self.front, S3Front::new, &self.closed(), S3Front::seed),
            copy_generation: self.copy_generation(),
        }
    }

    /// Registers the node cache's families (4.7, 7, 8, 11), and its device in the per-disk
    /// gauges.
    pub fn node_cache(&self) -> NodeCacheMetrics {
        NodeCacheMetrics {
            cache: register(&self.cache, Cache::new, &self.closed(), Cache::seed),
            disks: self.disks(&[Disk::Cache]),
            durability: self.durability(),
        }
    }

    /// Registers the fetch engine's families (section 4).
    pub fn fetch_engine(&self, config: &FetchEngineConfig) -> FetchEngineMetrics {
        let seed = Seed {
            drives: &config.drives,
            ..self.closed()
        };
        FetchEngineMetrics {
            fetch: register(&self.fetch, Fetch::new, &seed, Fetch::seed),
            copy_generation: self.copy_generation(),
        }
    }

    /// Registers the FUSE front's families (5.2 to 5.5).
    pub fn fuse_front(&self) -> FuseFrontMetrics {
        FuseFrontMetrics {
            fuse: register(&self.fuse, Fuse::new, &self.closed(), Fuse::seed),
            read_reply: self.read_reply(),
        }
    }

    /// Registers the NFS front's families (5.6): how its read replies were sent.
    pub fn nfs_front(&self) -> Arc<ReadReply> {
        self.read_reply()
    }

    /// Registers the Prepare provider's families (sections 3, 6).
    pub fn prepare_provider(&self) -> Arc<Prepare> {
        register(&self.prepare, Prepare::new, &self.closed(), Prepare::seed)
    }

    /// Every registered family, in the order [`Metrics::render`] writes them.
    #[must_use]
    pub fn families(&self) -> Vec<FamilyInfo> {
        let mut out = Vec::new();
        self.each(&mut |family| out.push(family.info()));
        out
    }

    /// The text of every registered family in the Prometheus text format (0.0.4): per family a
    /// `# HELP` and a `# TYPE` line, then its series.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.each(&mut |family| family.render(&mut out));
        out
    }

    /// Calls `f` on every registered family, group by group in a fixed order.
    fn each(&self, f: &mut dyn FnMut(&dyn Registered)) {
        fn visit<G>(
            slot: &Slot<G>,
            families: fn(&G) -> Vec<&dyn Registered>,
            f: &mut dyn FnMut(&dyn Registered),
        ) {
            if let Some(group) = slot.get() {
                for family in families(group) {
                    f(family);
                }
            }
        }
        for family in self.process.families() {
            f(family);
        }
        visit(&self.fetch, Fetch::families, f);
        visit(&self.prepare, Prepare::families, f);
        visit(&self.cache, Cache::families, f);
        visit(&self.fuse, Fuse::families, f);
        visit(&self.read_reply, ReadReply::families, f);
        visit(&self.durability, Durability::families, f);
        visit(&self.disks, Disks::families, f);
        visit(&self.copy_generation, CopyGeneration::families, f);
        visit(&self.hot, Hot::families, f);
        visit(&self.front, S3Front::families, f);
        visit(&self.peer, Peer::families, f);
        visit(&self.cold, Cold::families, f);
        visit(&self.movement, Movement::families, f);
        visit(&self.drives, Drives::families, f);
    }
}
