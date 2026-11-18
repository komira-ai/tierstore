//! The label values of the families: closed sets, and drive and node ids. Never a pack, a key, a
//! job, a holder or a path.
//!
//! Where a frozen type already names the values (the class of a fetch, the drive scheduler's
//! classes, a FUSE opcode, a drive's state, a seal's reason, a list's refusal), the label is that
//! type or a closed mirror of it. A mirror converts with an exhaustive `From`, so a variant added
//! to the frozen type fails to compile here until it has a label.

use std::borrow::Cow;

use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::list::Refusal;
use tierstore_fetch_api::upstream;
use tierstore_fetch_api::verify::VerifyPriority;
use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_store_api::hot::{self, Unavailable};
use tierstore_store_api::proto::LocateAnswer;
use tierstore_store_api::sched::{DriveClass, Refused};
use tsfuse::Opcode;

use crate::family::seal::{Seed, Value};
use crate::family::{LabelValue, State};

/// A closed label: the enum, `ALL`, `as_str`, and the label traits, seeded with every value.
macro_rules! closed {
    (
        $(#[$meta:meta])*
        $name:ident = $label:literal {
            $( $(#[$vmeta:meta])* $variant:ident = $text:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// The value as the label renders it.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text, )+
                }
            }
        }

        impl Value for $name {
            fn seed(_: &Seed<'_>) -> Vec<Self> {
                <Self as State>::ALL.to_vec()
            }
        }

        impl LabelValue for $name {
            const NAME: &'static str = $label;

            fn text(&self) -> Cow<'static, str> {
                Cow::Borrowed(self.as_str())
            }
        }

        impl State for $name {
            const ALL: &'static [Self] = &[$( Self::$variant, )+];

            fn index(self) -> usize {
                self as usize
            }
        }
    };
}

closed! {
    /// The pack size class a part belongs to (4.4: the bands of the part-sizing table; 4.3: the
    /// controller's signals are normalized per size class).
    SizeClass = "size_class" {
        /// A pack of at most 32 MiB.
        Small = "small",
        /// A pack above 32 MiB and at most 512 MiB.
        Medium = "medium",
        /// A pack above 512 MiB.
        Large = "large",
    }
}

impl SizeClass {
    /// The size class of a pack of `bytes` bytes.
    #[must_use]
    pub const fn of_pack(bytes: u64) -> Self {
        const MIB: u64 = 1 << 20;
        if bytes <= 32 * MIB {
            Self::Small
        } else if bytes <= 512 * MIB {
            Self::Medium
        } else {
            Self::Large
        }
    }

    /// The size class whose index ([`State::index`]: 0 small, 1 medium, 2 large) is `index`,
    /// the `size_class` a budget observation carries.
    #[must_use]
    pub fn from_index(index: u8) -> Option<Self> {
        <Self as State>::ALL.get(usize::from(index)).copied()
    }
}

closed! {
    /// Why the controller last moved a permit count or the ceiling (4.3).
    BudgetReason = "reason" {
        /// Slow start: doubling toward the share, after a decrease took the ceiling below it.
        SlowStart = "slow_start",
        /// +1 per window while a backlog exists.
        Increase = "increase",
        /// A drive's first-byte time above twice its baseline for two windows.
        DriveFirstByte = "drive_first_byte",
        /// A drive's reported queue wait above twice its baseline for two windows.
        DriveQueueWait = "drive_queue_wait",
        /// A `503 Retry-After` from a drive.
        Drive503 = "drive_503",
        /// A storage node's network time above twice its baseline for two windows.
        NodeNetwork = "node_network",
        /// More than a third of a storage node's drives in a decrease state.
        NodeDrives = "node_drives",
        /// The client's network time above twice its baseline for two windows.
        ClientNetwork = "client_network",
        /// More than a third of all drives in a decrease state.
        ClientDrives = "client_drives",
        /// The cooldown window after a decrease.
        Cooldown = "cooldown",
        /// No move: no backlog and no decrease evidence.
        Steady = "steady",
    }
}

closed! {
    /// The kind of a GET (4.2: GETs are at most grid parts plus demand sub-ranges plus retries
    /// plus hedges).
    GetKind = "kind" {
        /// A whole part of the grid, or a piece of the ramp.
        GridPart = "grid_part",
        /// A demand sub-range: a run of absent cells split from a part that had not started.
        DemandSubRange = "demand_sub",
        /// A retry after a stall, a transport failure or a mismatch.
        Retry = "retry",
        /// A hedge to the other hot replica or a degraded read.
        Hedge = "hedge",
    }
}

closed! {
    /// Why a read waited for cells (5.1, 4.5, 4.6).
    ReadWaitCause = "cause" {
        /// The job launched before Prepare was done, and the read started a demand fetch.
        LaunchBeforePrepare = "launch_before_prepare",
        /// The cells' part was already being fetched.
        InFlight = "in_flight",
        /// The read started a demand fetch after Prepare (a cell evicted, or never planned).
        DemandFetch = "demand_fetch",
        /// The fetch of the cells failed over: a retry, a hedge, or another copy.
        Retry = "retry",
        /// The cells were `recheck` after a restart and waited for their hash (section 8).
        Recheck = "recheck",
    }
}

closed! {
    /// Why the node cache refused an admission (7: the refusal reasons `budget`, `enospc` and
    /// `oversize` are metered).
    AdmissionReason = "reason" {
        /// The cache's byte budget is held.
        Budget = "budget",
        /// The cache device has no space.
        Enospc = "enospc",
        /// The pack is larger than the cache may hold.
        Oversize = "oversize",
    }
}

closed! {
    /// The FUSE transport a mount runs (5.2, 5.3).
    TransportMode = "mode" {
        /// Reader threads on cloned `/dev/fuse` descriptors.
        DevFuse = "dev_fuse",
        /// io_uring queues, with a `/dev/fuse` reader beside them for INTERRUPT and FORGET.
        IoUring = "io_uring",
    }
}

closed! {
    /// A FUSE operation: every opcode `tsfuse` knows, and `other` for a code it does not.
    FuseOp = "op" {
        /// `FUSE_LOOKUP`.
        Lookup = "lookup",
        /// `FUSE_FORGET`.
        Forget = "forget",
        /// `FUSE_GETATTR`.
        Getattr = "getattr",
        /// `FUSE_SETATTR`.
        Setattr = "setattr",
        /// `FUSE_READLINK`.
        Readlink = "readlink",
        /// `FUSE_MKDIR`.
        Mkdir = "mkdir",
        /// `FUSE_UNLINK`.
        Unlink = "unlink",
        /// `FUSE_RENAME`.
        Rename = "rename",
        /// `FUSE_OPEN`.
        Open = "open",
        /// `FUSE_READ`.
        Read = "read",
        /// `FUSE_WRITE`.
        Write = "write",
        /// `FUSE_STATFS`.
        Statfs = "statfs",
        /// `FUSE_RELEASE`.
        Release = "release",
        /// `FUSE_FSYNC`.
        Fsync = "fsync",
        /// `FUSE_FLUSH`.
        Flush = "flush",
        /// `FUSE_INIT`.
        Init = "init",
        /// `FUSE_OPENDIR`.
        Opendir = "opendir",
        /// `FUSE_READDIR`.
        Readdir = "readdir",
        /// `FUSE_RELEASEDIR`.
        Releasedir = "releasedir",
        /// `FUSE_CREATE`.
        Create = "create",
        /// `FUSE_INTERRUPT`.
        Interrupt = "interrupt",
        /// `FUSE_DESTROY`.
        Destroy = "destroy",
        /// `FUSE_BATCH_FORGET`.
        BatchForget = "batch_forget",
        /// `FUSE_READDIRPLUS`.
        Readdirplus = "readdirplus",
        /// An opcode `tsfuse` does not know.
        Other = "other",
    }
}

impl FuseOp {
    /// The operation of the opcode number `code`; `other` for one `tsfuse` does not know.
    #[must_use]
    pub fn from_code(code: u32) -> Self {
        Opcode::from_code(code).map_or(Self::Other, Self::from)
    }
}

impl From<Opcode> for FuseOp {
    fn from(op: Opcode) -> Self {
        match op {
            Opcode::Lookup => Self::Lookup,
            Opcode::Forget => Self::Forget,
            Opcode::Getattr => Self::Getattr,
            Opcode::Setattr => Self::Setattr,
            Opcode::Readlink => Self::Readlink,
            Opcode::Mkdir => Self::Mkdir,
            Opcode::Unlink => Self::Unlink,
            Opcode::Rename => Self::Rename,
            Opcode::Open => Self::Open,
            Opcode::Read => Self::Read,
            Opcode::Write => Self::Write,
            Opcode::Statfs => Self::Statfs,
            Opcode::Release => Self::Release,
            Opcode::Fsync => Self::Fsync,
            Opcode::Flush => Self::Flush,
            Opcode::Init => Self::Init,
            Opcode::Opendir => Self::Opendir,
            Opcode::Readdir => Self::Readdir,
            Opcode::Releasedir => Self::Releasedir,
            Opcode::Create => Self::Create,
            Opcode::Interrupt => Self::Interrupt,
            Opcode::Destroy => Self::Destroy,
            Opcode::BatchForget => Self::BatchForget,
            Opcode::Readdirplus => Self::Readdirplus,
        }
    }
}

closed! {
    /// Why a list was refused (3.5): the values of [`Refusal::label`], without their payload.
    ListRefusal = "reason" {
        /// The object is a v1 list.
        V1List = "v1_list",
        /// The tree has no L2 sidecar.
        MissingSidecar = "missing_sidecar",
        /// The L2 header names a digest function this release does not know.
        UnknownCellDigest = "unknown_cell_digest",
        /// The L2 header names a version this release does not read.
        UnknownVersion = "unknown_version",
    }
}

impl From<Refusal> for ListRefusal {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::V1List => Self::V1List,
            Refusal::MissingSidecar => Self::MissingSidecar,
            Refusal::UnknownCellDigest(_) => Self::UnknownCellDigest,
            Refusal::UnknownVersion(_) => Self::UnknownVersion,
        }
    }
}

closed! {
    /// When the fill of a pack was found lost (4.6).
    PackLostPhase = "phase" {
        /// Before any job held the tree (Prepare, or the first `LOCATE`).
        BeforeJob = "before_job",
        /// While a job held the tree: its holders are failed with `tree-lost`.
        MidJob = "mid_job",
    }
}

closed! {
    /// Why a segment was sealed (10.3 step 6): the values of the frozen [`hot::SealReason`].
    SealReason = "reason" {
        /// It reached `hot.seal_bytes`, or a record did not fit.
        Size = "size",
        /// Past `hot.seal_max_age` above `hot.seal_min_bytes`, or past `destage.max_age`.
        Age = "age",
        /// The writer sealed early.
        Early = "early",
        /// A replica took over from a writer that stopped answering.
        TakeOver = "take_over",
    }
}

impl From<hot::SealReason> for SealReason {
    fn from(reason: hot::SealReason) -> Self {
        match reason {
            hot::SealReason::Size => Self::Size,
            hot::SealReason::Age => Self::Age,
            hot::SealReason::Early => Self::Early,
            hot::SealReason::TakeOver => Self::TakeOver,
        }
    }
}

closed! {
    /// A step of the majority seal (10.3): rounds prepared, proposals adopted, ballots rejected.
    /// A round counts once as `prepare` and once more if it adopts or is rejected.
    SealRound = "outcome" {
        /// A prepare round started.
        Prepare = "prepare",
        /// A proposal already accepted by a node was adopted.
        Adopted = "adopted",
        /// A ballot was rejected by a higher promise.
        Rejected = "rejected",
    }
}

closed! {
    /// What the hot tier reclaimed (10.3, 10.5).
    ReclaimKind = "kind" {
        /// A cache segment of promoted copies.
        CacheSegment = "cache_segment",
        /// A sealed segment whose blocks are destaged (clean).
        CleanSegment = "clean_segment",
        /// A large file whose block is destaged.
        CleanLargeFile = "clean_large_file",
    }
}

closed! {
    /// The answer to a PUT at the S3 front (10.2, 10.6).
    PutOutcome = "outcome" {
        /// 200: the key was created.
        Created = "created",
        /// 200: the key already held the same bytes.
        Identical = "identical",
        /// 412: the key holds other bytes.
        Conflict = "conflict",
        /// 503: fewer live hot devices than `min_acks`.
        TooFewCopies = "too_few_copies",
        /// 503: the all-node key check had no definitive answer.
        NotDefinitive = "not_definitive",
        /// 503: the dirty marks throttle PUTs.
        Throttled = "throttled",
        /// 503: this node is on the minority side of the view.
        Minority = "minority",
        /// 507: the hot tier is full.
        Full = "full",
    }
}

impl From<Unavailable> for PutOutcome {
    fn from(why: Unavailable) -> Self {
        match why {
            Unavailable::TooFewCopies => Self::TooFewCopies,
            Unavailable::NotDefinitive => Self::NotDefinitive,
            Unavailable::Throttled => Self::Throttled,
            Unavailable::Minority => Self::Minority,
        }
    }
}

closed! {
    /// Why a copy of a PUT was abandoned (10.2).
    AbandonReason = "reason" {
        /// The copy's trailer generation differed from the writer's.
        GenerationMismatch = "generation_mismatch",
        /// The replica lagged past `put_copy_stall`.
        ReplicaStall = "replica_stall",
    }
}

closed! {
    /// A node's answer to `LOCATE` (10.6): the variants of [`LocateAnswer`].
    LocateResult = "result" {
        /// In the hot tier.
        Hot = "hot",
        /// In the cold tier.
        Cold = "cold",
        /// Not stored: a definitive miss from this node.
        Absent = "absent",
        /// The node's index is not built: never a miss.
        Indexing = "indexing",
    }
}

impl From<&LocateAnswer> for LocateResult {
    fn from(answer: &LocateAnswer) -> Self {
        match answer {
            LocateAnswer::Hot { .. } => Self::Hot,
            LocateAnswer::Cold { .. } => Self::Cold,
            LocateAnswer::Absent => Self::Absent,
            LocateAnswer::Indexing => Self::Indexing,
        }
    }
}

closed! {
    /// A step of the index's recovery after a start (10.9).
    RecoveryPhase = "phase" {
        /// Reading the replay mark.
        ReplayMark = "replay_mark",
        /// Replaying the hot records and trailers after the mark.
        Replay = "replay",
        /// Rebuilding from block trailers, `blk/*.idx` and cold shard trailers.
        Rebuild = "rebuild",
        /// Pulling the tombstones of the other nodes.
        TombstonePull = "tombstone_pull",
    }
}

closed! {
    /// Whether a stripe holds all its data blocks (10.5: partial stripes after `partial_after`).
    StripeState = "state" {
        /// `k` data blocks.
        Full = "full",
        /// Fewer than `k` data blocks.
        Partial = "partial",
    }
}

closed! {
    /// Why a cold read was served degraded (10.4, 10.7, 10.8).
    DegradedCause = "cause" {
        /// The shard's drive is `stalled`.
        Stalled = "stalled",
        /// The shard's drive is `failed`.
        Failed = "failed",
        /// The shard's drive is `suspect` in the reader's view.
        Suspect = "suspect",
        /// The shard's node or drive is `down`.
        Down = "down",
        /// The shard's read failed with an I/O error.
        IoError = "io_error",
        /// The shard's bytes failed their crc.
        CrcMismatch = "crc_mismatch",
    }
}

closed! {
    /// What a cold rebuild wrote for (10.12, 21.3).
    RebuildKind = "kind" {
        /// A lost drive.
        Drive = "drive",
        /// A lost node.
        Node = "node",
        /// A shard a scrub found bad.
        ScrubRepair = "scrub_repair",
        /// A shard moved to satisfy the placement rule.
        PlacementMove = "placement_move",
        /// A stripe re-encoded off `drain` devices.
        Drain = "drain",
    }
}

closed! {
    /// How a destage of a stripe ended (10.5).
    DestageOutcome = "outcome" {
        /// The descriptor committed.
        Committed = "committed",
        /// The stripe was abandoned (its `.up` files unlinked) and is retried.
        Abandoned = "abandoned",
    }
}

closed! {
    /// The priority destage runs at (10.5: above `hot.dirty_urgent` it rises above `prepare`).
    DestagePriority = "priority" {
        /// The drive's `write` class.
        Normal = "normal",
        /// Above `prepare`, with partial stripes allowed.
        Urgent = "urgent",
    }
}

closed! {
    /// How a promotion ended (10.9).
    PromoteOutcome = "outcome" {
        /// Copied into a cache segment.
        Promoted = "promoted",
        /// Not copied: dirty bytes above the destage mark.
        SkippedPressure = "skipped_pressure",
        /// Not copied: the object is larger than a promotion takes.
        SkippedSize = "skipped_size",
        /// Not copied: another node or request promoted it first.
        LostRace = "lost_race",
    }
}

closed! {
    /// How a stripe-free round ended (10.5: committed, waited for a node, found live).
    FreeOutcome = "outcome" {
        /// A majority committed the free.
        Committed = "committed",
        /// It waited for a node that holds a shard.
        Waited = "waited",
        /// A block of the stripe was found live.
        FoundLive = "found_live",
    }
}

closed! {
    /// A drive's state (10.7, 10.8): the values of the frozen [`upstream::DriveState`].
    DriveState = "state" {
        /// Serving.
        Up = "up",
        /// The owner's index or drive list is not built.
        Indexing = "indexing",
        /// The owner's watchdog saw a call outstanding past `stall_after`.
        Stalled = "stalled",
        /// Failed pings or error responses.
        Suspect = "suspect",
        /// No ping answered for three intervals.
        Down = "down",
        /// `FAILED` written.
        Failed = "failed",
        /// The membership drains it.
        Drain = "drain",
        /// Not a candidate.
        Out = "out",
    }
}

impl From<upstream::DriveState> for DriveState {
    fn from(state: upstream::DriveState) -> Self {
        match state {
            upstream::DriveState::Up => Self::Up,
            upstream::DriveState::Indexing => Self::Indexing,
            upstream::DriveState::Stalled => Self::Stalled,
            upstream::DriveState::Suspect => Self::Suspect,
            upstream::DriveState::Down => Self::Down,
            upstream::DriveState::Failed => Self::Failed,
            upstream::DriveState::Drain => Self::Drain,
            upstream::DriveState::Out => Self::Out,
        }
    }
}

closed! {
    /// The direction of drive I/O.
    Dir = "dir" {
        /// Reads.
        Read = "read",
        /// Writes.
        Write = "write",
    }
}

/// Why a drive answered 503 (10.7, 10.8): its queue was full, or its state serves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Drive503 {
    /// The class's queue was at its bound.
    QueueFull,
    /// The drive's state.
    State(DriveState),
}

impl From<&Refused> for Drive503 {
    fn from(refused: &Refused) -> Self {
        match refused {
            Refused::QueueFull { .. } => Self::QueueFull,
            Refused::State(state) => Self::State((*state).into()),
        }
    }
}

impl Value for Drive503 {
    fn seed(_: &Seed<'_>) -> Vec<Self> {
        let states = <DriveState as State>::ALL.iter().map(|s| Self::State(*s));
        std::iter::once(Self::QueueFull).chain(states).collect()
    }
}

impl LabelValue for Drive503 {
    const NAME: &'static str = "reason";

    fn text(&self) -> Cow<'static, str> {
        match self {
            Self::QueueFull => Cow::Borrowed("queue_full"),
            Self::State(state) => Cow::Borrowed(state.as_str()),
        }
    }
}

/// A disk of the per-disk gauges (`r_await`, `aqu_sz`, `util`): the cache device of a build node,
/// or a drive (a hard drive or the hot device) of a storage node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Disk {
    /// The node cache's device (section 11).
    Cache,
    /// A drive, by its id.
    Drive(DriveId),
}

impl Value for Disk {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        seed.disks.to_vec()
    }
}

impl LabelValue for Disk {
    const NAME: &'static str = "disk";

    fn text(&self) -> Cow<'static, str> {
        match self {
            Self::Cache => Cow::Borrowed("cache"),
            Self::Drive(id) => Cow::Owned(id.to_string()),
        }
    }
}

/// The number of live copies of a hot block (`hot_blocks_by_copies{copies}`), 0 to
/// `hot.replicas`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Copies(pub u8);

impl Value for Copies {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        (0..=seed.hot_replicas).map(Self).collect()
    }
}

impl LabelValue for Copies {
    const NAME: &'static str = "copies";

    fn text(&self) -> Cow<'static, str> {
        Cow::Owned(self.0.to_string())
    }
}

/// How many more shards (or copies) a stripe can lose and stay readable (10.4): `0` is the last
/// readable margin; `unreadable` is below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Margin {
    /// Fewer than `k` shards (or no copy) left.
    Unreadable,
    /// Readable, able to lose this many more.
    Spare(u8),
}

impl Value for Margin {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        let most = seed.codes.iter().map(|c| spare(*c)).max().unwrap_or(0);
        std::iter::once(Self::Unreadable)
            .chain((0..=most).map(Self::Spare))
            .collect()
    }
}

/// The margin of a whole stripe of `code`: `m`, or `N - 1`.
fn spare(code: Code) -> u8 {
    match code {
        Code::Ec { m, .. } => m,
        Code::Replicas(n) => n.saturating_sub(1),
    }
}

impl LabelValue for Margin {
    const NAME: &'static str = "margin";

    fn text(&self) -> Cow<'static, str> {
        match self {
            Self::Unreadable => Cow::Borrowed("unreadable"),
            Self::Spare(n) => Cow::Owned(n.to_string()),
        }
    }
}

/// The code of a stripe (`cold_stripes{code}`): `k+m`, or `replicas-N`; the values of the
/// frozen [`Code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StripeCode {
    /// Reed-Solomon with `k` data and `m` parity shards.
    Ec {
        /// Data shards.
        k: u8,
        /// Parity shards.
        m: u8,
    },
    /// `N` whole copies.
    Replicas(u8),
}

impl From<Code> for StripeCode {
    fn from(code: Code) -> Self {
        match code {
            Code::Ec { k, m } => Self::Ec { k, m },
            Code::Replicas(n) => Self::Replicas(n),
        }
    }
}

impl Value for StripeCode {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        seed.codes.iter().copied().map(Self::from).collect()
    }
}

impl LabelValue for StripeCode {
    const NAME: &'static str = "code";

    fn text(&self) -> Cow<'static, str> {
        Cow::Owned(match *self {
            Self::Ec { k, m } => format!("{k}+{m}"),
            Self::Replicas(n) => format!("replicas-{n}"),
        })
    }
}

/// The release a process runs (`tierstore_build_info{version}`): one value per process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(&'static str);

impl Value for Version {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        vec![Self(seed.version)]
    }
}

impl LabelValue for Version {
    const NAME: &'static str = "version";

    fn text(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
}

impl Value for Class {
    fn seed(_: &Seed<'_>) -> Vec<Self> {
        vec![Self::Demand, Self::Prepare, Self::Background]
    }
}

impl LabelValue for Class {
    const NAME: &'static str = "class";

    fn text(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.header())
    }
}

impl Value for DriveClass {
    fn seed(_: &Seed<'_>) -> Vec<Self> {
        vec![Self::Demand, Self::Write, Self::Prepare, Self::Background]
    }
}

impl LabelValue for DriveClass {
    const NAME: &'static str = "class";

    fn text(&self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            Self::Demand => "demand",
            Self::Write => "write",
            Self::Prepare => "prepare",
            Self::Background => "background",
        })
    }
}

impl Value for VerifyPriority {
    fn seed(_: &Seed<'_>) -> Vec<Self> {
        vec![Self::Fill, Self::Recheck]
    }
}

impl LabelValue for VerifyPriority {
    const NAME: &'static str = "priority";

    fn text(&self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            Self::Fill => "fill",
            Self::Recheck => "recheck",
        })
    }
}

impl Value for DriveId {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        seed.drives.to_vec()
    }
}

impl LabelValue for DriveId {
    const NAME: &'static str = "drive";

    fn text(&self) -> Cow<'static, str> {
        Cow::Owned(self.to_string())
    }
}

impl Value for NodeId {
    fn seed(seed: &Seed<'_>) -> Vec<Self> {
        seed.nodes.to_vec()
    }
}

impl LabelValue for NodeId {
    const NAME: &'static str = "node";

    fn text(&self) -> Cow<'static, str> {
        Cow::Owned(self.to_string())
    }
}
