//! A list of pack records for the planner's tests, and helpers to state inputs and read plans
//! cell by cell.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the support module"
)]

use std::collections::BTreeMap;

use tierstore_fetch_api::ids::{ByteRange, PackKey, PackRoot};
use tierstore_fetch_api::list::{CellDigest, ListL2, NodeIndex, PackRecord};
use tierstore_plan::config::{Medium, PlanConfig};
use tierstore_plan::plan::{Backlog, Needed, Plan, PlanError, PlanInput, Touch, plan};

/// One MiB, the cell of every pack here unless a test says otherwise.
pub const MIB: u64 = 1 << 20;

/// A list that has pack records and nothing else: the planner reads no digest, root or directory.
pub struct FakeList {
    pub packs: Vec<PackRecord>,
}

impl ListL2 for FakeList {
    fn pack_count(&self) -> u32 {
        u32::try_from(self.packs.len()).unwrap()
    }

    fn pack_record(&self, pack: u32) -> Option<PackRecord> {
        self.packs.get(pack as usize).cloned()
    }

    fn cell_digest(&self, _pack: u32, _cell: u32) -> Option<CellDigest> {
        None
    }

    fn pack_root(&self, _pack: u32) -> Option<PackRoot> {
        None
    }

    fn dir_lookup(&self, _dir: NodeIndex, _name: &[u8]) -> Option<NodeIndex> {
        None
    }
}

/// A pack record of `size` bytes in 1 MiB cells.
pub fn pack(key: &str, size: u64) -> PackRecord {
    PackRecord {
        key: PackKey(key.to_owned()),
        pack_size: size,
        cell_first: 0,
        cell_count: u32::try_from(size.div_ceil(MIB)).unwrap(),
        cell_shift: 20,
        pack_flags: 0,
    }
}

/// A list of packs `p0`, `p1`, ... of these sizes in MiB.
pub fn list_mib(sizes: &[u64]) -> FakeList {
    FakeList {
        packs: sizes
            .iter()
            .enumerate()
            .map(|(i, &mib)| pack(&format!("p{i}"), mib * MIB))
            .collect(),
    }
}

/// Bytes `[offset, offset + len)` of `pack`.
pub fn bytes(pack: u32, offset: u64, len: u64) -> Needed {
    Needed {
        pack,
        range: ByteRange { offset, len },
    }
}

/// Cells `[first, first + count)` of `pack`, whole.
pub fn cells(pack: u32, first: u64, count: u64) -> Needed {
    bytes(pack, first * MIB, count * MIB)
}

/// A profile touch of cells `[first, first + count)` of the pack keyed `key`.
pub fn touch(key: &str, first: u64, count: u64) -> Touch {
    Touch {
        key: PackKey(key.to_owned()),
        range: ByteRange {
            offset: first * MIB,
            len: count * MIB,
        },
    }
}

/// The inputs of one plan.
pub struct Case<'a> {
    pub list: &'a FakeList,
    pub needed: Vec<Needed>,
    pub profile: Vec<Touch>,
    pub media: BTreeMap<u32, Medium>,
    pub backlog: Backlog,
    pub config: PlanConfig,
}

impl<'a> Case<'a> {
    /// No profile, every pack cold, no backlog, the default configuration.
    pub fn new(list: &'a FakeList, needed: Vec<Needed>) -> Self {
        Self {
            list,
            needed,
            profile: Vec::new(),
            media: BTreeMap::new(),
            backlog: Backlog::default(),
            config: PlanConfig::default(),
        }
    }

    pub fn try_plan(&self) -> Result<Plan, PlanError> {
        plan(
            &PlanInput {
                list: self.list,
                needed: &self.needed,
                profile: &self.profile,
                media: &self.media,
                backlog: self.backlog,
            },
            &self.config,
        )
    }

    pub fn plan(&self) -> Plan {
        self.try_plan().unwrap()
    }
}

/// Every cell the plan ensures, as `(pack, cell) -> rank`; fails the test if a cell is ensured
/// twice.
pub fn per_cell(plan: &Plan) -> BTreeMap<(u32, u32), u32> {
    let mut out = BTreeMap::new();
    for e in &plan.ensures {
        for run in &e.runs {
            for cell in run.cells() {
                assert!(
                    out.insert((e.pack, cell), e.rank.0).is_none(),
                    "cell {cell} of pack {} ensured twice",
                    e.pack
                );
            }
        }
    }
    out
}

/// One ensure as `(pack, rank, [(first, count)])`.
pub type Shape = (u32, u32, Vec<(u32, u32)>);

/// The ensures, in plan order.
pub fn shape(plan: &Plan) -> Vec<Shape> {
    plan.ensures
        .iter()
        .map(|e| {
            (
                e.pack,
                e.rank.0,
                e.runs.iter().map(|r| (r.first, r.count)).collect(),
            )
        })
        .collect()
}
