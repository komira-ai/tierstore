//! `plan`: the needed ranges of a tree, in tree order, and a first-touch profile, to the cell runs
//! Prepare ensures and their ranks (section 6).

use std::collections::{BTreeMap, BTreeSet};

use tierstore_fetch_api::ids::{ByteRange, CellRun, PackKey, Rank};
use tierstore_fetch_api::list::{ListL2, PackRecord};

use crate::config::{Medium, PlanConfig};

/// A byte range of a pack of the list that the job needs (a span of the list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Needed {
    /// The pack's index in the list.
    pub pack: u32,
    /// The bytes.
    pub range: ByteRange,
}

/// One first touch of a profile: a byte range of the pack with this key, read before every later
/// touch of the profile. A profile is a hint recorded by an earlier run, so a touch names its pack
/// by key, not by an index of some list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Touch {
    /// The pack's object key.
    pub key: PackKey,
    /// The bytes touched.
    pub range: ByteRange,
}

/// The node's queued backlog against what its budget moves, for the downgrade of section 6
/// step 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Backlog {
    /// Bytes queued for fetching on this node, every class.
    pub queued_bytes: u64,
    /// The budget's throughput, in bytes per second.
    pub bytes_per_s: u64,
}

impl Backlog {
    /// Whether the queued bytes take more than `ms` milliseconds at the budget's throughput. No
    /// throughput with bytes queued is an unbounded backlog.
    #[must_use]
    pub fn exceeds(self, ms: u64) -> bool {
        u128::from(self.queued_bytes) * 1_000 > u128::from(self.bytes_per_s) * u128::from(ms)
    }
}

/// What the planner reads.
#[derive(Clone, Copy)]
pub struct PlanInput<'a> {
    /// The tree's list.
    pub list: &'a dyn ListL2,
    /// The ranges the job needs, in tree order.
    pub needed: &'a [Needed],
    /// The profile's first touches, in first-touch order; empty when there is no profile.
    pub profile: &'a [Touch],
    /// The medium of the packs whose location is known; any other pack is [`Medium::Cold`].
    pub media: &'a BTreeMap<u32, Medium>,
    /// The node's backlog.
    pub backlog: Backlog,
}

impl std::fmt::Debug for PlanInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanInput")
            .field("needed", &self.needed)
            .field("profile", &self.profile)
            .field("media", &self.media)
            .field("backlog", &self.backlog)
            .finish_non_exhaustive()
    }
}

/// One `Engine::ensure` of class `prepare`: runs of one pack at one rank.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ensure {
    /// The pack's index in the list.
    pub pack: u32,
    /// The runs, ascending and separated by at least one cell not in this ensure.
    pub runs: Vec<CellRun>,
    /// The first-touch rank of the runs' needed cells.
    pub rank: Rank,
}

/// How a pack is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fetch {
    /// Its needed runs, merged across gaps of at most the gap rule.
    Runs,
    /// Every cell of the pack.
    Whole,
    /// Its merged runs, because the backlog downgraded a pack the coverage made whole.
    RunsUnderBacklog,
}

/// What the plan does with one pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackPlan {
    /// The pack's index in the list.
    pub pack: u32,
    /// Its cells.
    pub cell_count: u32,
    /// The medium its gap rule came from.
    pub medium: Medium,
    /// Its gap rule, in cells.
    pub gap_cells: u32,
    /// The cells the needed ranges cover.
    pub needed_cells: u32,
    /// The cells of the needed runs once merged across gaps: `cov` is this over `cell_count`.
    pub merged_cells: u32,
    /// The cells the plan ensures.
    pub fetch_cells: u32,
    /// How.
    pub fetch: Fetch,
}

/// The plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// The ensures, by rank, then pack index, then first cell.
    pub ensures: Vec<Ensure>,
    /// One entry per pack with a needed cell, by pack index.
    pub packs: Vec<PackPlan>,
}

/// Why a plan cannot be made: the needed ranges and the list disagree. A profile never makes a
/// plan fail: a touch of a pack or cell the job does not need is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A needed range names a pack the list does not have.
    #[error("needed range names pack {pack}, which the list does not have")]
    NoSuchPack {
        /// The pack index.
        pack: u32,
    },
    /// A needed range ends past its pack's size, or past 2^64.
    #[error("needed range {offset}+{len} is outside pack {pack}")]
    OutsidePack {
        /// The pack index.
        pack: u32,
        /// The range's first byte.
        offset: u64,
        /// The range's length.
        len: u64,
    },
    /// The pack's record cannot hold the range's cells: a cell shift of 64 or more, or fewer
    /// cells than its size needs.
    #[error("pack {pack}'s record does not cover its own bytes in cells")]
    BadPackRecord {
        /// The pack index.
        pack: u32,
    },
}

/// Cells `[start, end)` of one pack at one rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    start: u32,
    end: u32,
    rank: u32,
}

impl Segment {
    fn cells(self) -> u32 {
        self.end - self.start
    }
}

/// One pack with a needed cell: its record and the cells the job needs, at their tree ranks.
struct PackWork {
    record: PackRecord,
    needed: Vec<Segment>,
}

/// The rank of the `n`th first touch. Ranks past `u32::MAX` share the last one, which only
/// ties their order.
fn rank_at(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The cells `[start, end)` of `record` that `range` covers, or why the record cannot hold
/// them. `range` is not empty.
fn needed_cells(record: &PackRecord, pack: u32, range: ByteRange) -> Result<(u32, u32), PlanError> {
    if range.end().is_none_or(|end| end > record.pack_size) {
        return Err(PlanError::OutsidePack {
            pack,
            offset: range.offset,
            len: range.len,
        });
    }
    let run = range
        .cells(record.cell_shift)
        .filter(|run| u64::from(run.first) + u64::from(run.count) <= u64::from(record.cell_count))
        .ok_or(PlanError::BadPackRecord { pack })?;
    Ok((run.first, run.first + run.count))
}

/// The cells of `record` that a profile's `range` covers, clipped to the pack; `None` when it
/// covers none.
fn touched_cells(record: &PackRecord, range: ByteRange) -> Option<(u32, u32)> {
    let run = range.cells(record.cell_shift)?;
    let end = run.first.saturating_add(run.count).min(record.cell_count);
    (run.first < end).then_some((run.first, end))
}

/// Appends `seg` to `out`, joining it to the last segment when they touch at the same rank.
fn push(out: &mut Vec<Segment>, seg: Segment) {
    match out.last_mut() {
        Some(last) if last.end == seg.start && last.rank == seg.rank => last.end = seg.end,
        _ => out.push(seg),
    }
}

/// The needed cells of a pack as ascending segments, each cell at its rank: the lowest rank of
/// every needed range and profile touch that covers it. Cells no needed range covers are left out,
/// whatever the profile touched.
fn ranked(needed: &[Segment], touched: &[Segment]) -> Vec<Segment> {
    // (cell, opens, is_needed, (rank, interval)): each interval opens at its start and closes at
    // its end. The interval's number makes every open entry distinct, even where ranks tie.
    let mut events: Vec<(u32, bool, bool, (u32, usize))> = Vec::new();
    let all = needed
        .iter()
        .map(|s| (s, true))
        .chain(touched.iter().map(|s| (s, false)));
    for (id, (s, is_needed)) in all.enumerate() {
        events.push((s.start, true, is_needed, (s.rank, id)));
        events.push((s.end, false, is_needed, (s.rank, id)));
    }
    events.sort_unstable();
    let mut out = Vec::new();
    let mut open_needed = 0u32;
    let mut open: BTreeSet<(u32, usize)> = BTreeSet::new();
    let mut at = 0u32;
    let mut events = events.into_iter().peekable();
    while let Some(&(cell, ..)) = events.peek() {
        if let Some(&(rank, _)) = open.first().filter(|_| open_needed > 0) {
            push(
                &mut out,
                Segment {
                    start: at,
                    end: cell,
                    rank,
                },
            );
        }
        while let Some((_, opens, is_needed, entry)) = events.next_if(|e| e.0 == cell) {
            if opens {
                open_needed += u32::from(is_needed);
                open.insert(entry);
            } else {
                open_needed -= u32::from(is_needed);
                open.remove(&entry);
            }
        }
        at = cell;
    }
    out
}

/// `segments` with every gap of at most `gap` cells read through: a gap's cells take the rank of
/// the segment before them, which the same read continues.
fn merged(segments: &[Segment], gap: u32) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for &seg in segments {
        if let Some(last) = out.last_mut().filter(|last| seg.start - last.end <= gap) {
            last.end = seg.start;
        }
        push(&mut out, seg);
    }
    out
}

/// `segments` grown to the whole pack of `cell_count` cells: the cells before the first take its
/// rank, and every other cell the rank of the segment before it.
fn whole(segments: &[Segment], cell_count: u32) -> Vec<Segment> {
    let ends = segments
        .iter()
        .skip(1)
        .map(|s| s.start)
        .chain(std::iter::once(cell_count));
    let mut out = Vec::with_capacity(segments.len());
    for (i, (&seg, end)) in segments.iter().zip(ends).enumerate() {
        let start = if i == 0 { 0 } else { seg.start };
        push(
            &mut out,
            Segment {
                start,
                end,
                rank: seg.rank,
            },
        );
    }
    out
}

fn total(segments: &[Segment]) -> u32 {
    segments.iter().map(|s| s.cells()).sum()
}

/// Whether `part` of `whole` is at least `percent`%.
fn at_least(part: u32, whole: u32, percent: u8) -> bool {
    u64::from(part) * 100 >= u64::from(percent) * u64::from(whole)
}

/// The plan of a tree's Prepare (section 6): which cells of which packs to ensure, at which rank.
///
/// A cell is needed when a needed range covers it. Its rank is the index of the first profile
/// touch covering it, else the number of touches plus the index of the first needed range covering
/// it. A pack's needed runs are merged across gaps of at most its medium's gap rule; the pack is
/// fetched whole when the merged cells reach the coverage of [`PlanConfig`] and the backlog does
/// not exceed its bound. Fetched cells no needed range covers take the rank of the nearest needed
/// cell before them in the pack, or after them when none is before.
///
/// # Errors
/// [`PlanError`] when a needed range names a pack the list does not have, lies outside its pack,
/// or its pack's record cannot hold its cells.
pub fn plan(input: &PlanInput<'_>, config: &PlanConfig) -> Result<Plan, PlanError> {
    let mut work: BTreeMap<u32, PackWork> = BTreeMap::new();
    let first_tree_rank = input.profile.len();
    for (i, need) in input.needed.iter().enumerate() {
        let record = input
            .list
            .pack_record(need.pack)
            .ok_or(PlanError::NoSuchPack { pack: need.pack })?;
        if need.range.len == 0 {
            continue;
        }
        let (start, end) = needed_cells(&record, need.pack, need.range)?;
        let entry = work.entry(need.pack).or_insert_with(|| PackWork {
            record,
            needed: Vec::new(),
        });
        entry.needed.push(Segment {
            start,
            end,
            rank: rank_at(first_tree_rank.saturating_add(i)),
        });
    }

    let mut by_key: BTreeMap<PackKey, Vec<u32>> = BTreeMap::new();
    for (&pack, w) in &work {
        by_key.entry(w.record.key.clone()).or_default().push(pack);
    }
    // The touches of each needed pack, at their profile ranks.
    let mut touches: BTreeMap<u32, Vec<(ByteRange, u32)>> = BTreeMap::new();
    for (j, touch) in input.profile.iter().enumerate() {
        for &pack in by_key.get(&touch.key).into_iter().flatten() {
            touches
                .entry(pack)
                .or_default()
                .push((touch.range, rank_at(j)));
        }
    }

    let downgrade = input.backlog.exceeds(config.backlog_ms);
    let mut ensures: BTreeMap<(u32, u32), Vec<CellRun>> = BTreeMap::new();
    let mut packs = Vec::with_capacity(work.len());
    for (&pack, w) in &work {
        let medium = input.media.get(&pack).copied().unwrap_or(Medium::Cold);
        let gap_cells = config.gap_cells(medium, w.record.cell_shift);
        let touched: Vec<Segment> = touches
            .remove(&pack)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(range, rank)| {
                touched_cells(&w.record, range).map(|(start, end)| Segment { start, end, rank })
            })
            .collect();
        let needed = ranked(&w.needed, &touched);
        let runs = merged(&needed, gap_cells);
        let merged_cells = total(&runs);
        let cell_count = w.record.cell_count;
        let wants_whole = at_least(merged_cells, cell_count, config.whole_cov_percent)
            || (w.record.pack_size <= config.small_pack_bytes
                && at_least(merged_cells, cell_count, config.small_whole_cov_percent));
        let (fetch, segments) = match (wants_whole, downgrade) {
            (false, _) => (Fetch::Runs, runs),
            (true, true) => (Fetch::RunsUnderBacklog, runs),
            (true, false) => (Fetch::Whole, whole(&runs, cell_count)),
        };
        for seg in &segments {
            ensures.entry((seg.rank, pack)).or_default().push(CellRun {
                first: seg.start,
                count: seg.cells(),
            });
        }
        packs.push(PackPlan {
            pack,
            cell_count,
            medium,
            gap_cells,
            needed_cells: total(&needed),
            merged_cells,
            fetch_cells: total(&segments),
            fetch,
        });
    }

    Ok(Plan {
        ensures: ensures
            .into_iter()
            .map(|((rank, pack), runs)| Ensure {
                pack,
                runs,
                rank: Rank(rank),
            })
            .collect(),
        packs,
    })
}
