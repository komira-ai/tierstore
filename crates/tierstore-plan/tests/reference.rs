//! The planner against a reference written cell by cell, over generated inputs: random packs (with
//! short last cells, packs on both sides of 64 MiB, and every `pack_flags` byte, shared or not,
//! with bits the reader does not know), needed ranges, media, gap rules from 0 to 6 cells, and
//! backlogs on both sides of the bound.
//!
//! The reference states section 6 directly: a needed range's rank is its place among the ranges of
//! shared packs (`pack_flags` bit 1) in tree order, or, after all of them, its place among the
//! other ranges in tree order (6.1); a cell is needed when a range covers it; its rank is the
//! lowest of the needed ranges covering it; a stretch of at most
//! `gap_cells` absent cells between two needed cells is read, at the rank of the needed cell
//! before it; the pack is whole at the coverage thresholds unless the backlog downgrades it, and
//! every unneeded cell of a whole pack takes the rank of the needed cell before it, or after it at
//! the head.
//!
//! Catches any difference between the plan and the reference in which cells are ensured, at which
//! rank, and in each pack's counts and decision; and any ensure list that is not one ensure per
//! (rank, pack), in rank then pack order, with ascending runs that never touch.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use std::collections::BTreeMap;

use support::{Case, FakeList, MIB, pack, per_cell};
use tierstore_fetch_api::ids::ByteRange;
use tierstore_plan::config::{DriveModel, Medium, PlanConfig};
use tierstore_plan::plan::{Backlog, Fetch, Needed, Plan};

/// A small deterministic generator (a 64-bit LCG): the cases are the same on every run.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Generated {
    list: FakeList,
    needed: Vec<Needed>,
    media: BTreeMap<u32, Medium>,
    backlog: Backlog,
    config: PlanConfig,
}

fn generate(seed: u64) -> Generated {
    let mut g = Gen(seed);
    let packs = 1 + g.below(4);
    let list = FakeList {
        packs: (0..packs)
            .map(|i| {
                // 1 to 100 cells, the last one short half of the time.
                let cells = 1 + g.below(100);
                let short = if g.below(2) == 0 { g.below(MIB) } else { 0 };
                let mut record = pack(&format!("k{i}"), cells * MIB - short);
                // Any flag byte: bit 1 half of the time, beside bits that do not order.
                record.pack_flags = u8::try_from(g.below(256)).unwrap();
                record
            })
            .collect(),
    };
    let needed = (0..g.below(8))
        .map(|_| {
            let p = g.below(packs);
            let size = list.packs[p as usize].pack_size;
            let offset = g.below(size);
            let len = g.below((size - offset).min(12 * MIB) + 1);
            Needed {
                pack: u32::try_from(p).unwrap(),
                range: ByteRange { offset, len },
            }
        })
        .collect();
    let media = (0..packs)
        .filter_map(|p| match g.below(3) {
            0 => Some((u32::try_from(p).unwrap(), Medium::Hot)),
            1 => Some((u32::try_from(p).unwrap(), Medium::Cold)),
            _ => None,
        })
        .collect();
    let backlog = Backlog {
        queued_bytes: g.below(5_000),
        bytes_per_s: g.below(2_000),
    };
    // Gap rules of 0 to 6 cells on both media.
    let model = |g: &mut Gen| DriveModel {
        positioning_us: u32::try_from(g.below(6_000)).unwrap(),
        media_mb_per_s: 1_000,
    };
    let config = PlanConfig {
        hot: model(&mut g),
        cold: model(&mut g),
        ..PlanConfig::default()
    };
    Generated {
        list,
        needed,
        media,
        backlog,
        config,
    }
}

/// What the reference says of one pack.
struct RefPack {
    ranks: Vec<Option<u32>>,
    needed: u32,
    merged: u32,
    fetch: Fetch,
}

fn cell_range(range: ByteRange, cell_count: u32) -> std::ops::Range<u32> {
    if range.len == 0 {
        return 0..0;
    }
    let Some(end) = range.offset.checked_add(range.len) else {
        return 0..0;
    };
    let first = (range.offset / MIB).min(u64::from(cell_count));
    let last = ((end - 1) / MIB + 1).min(u64::from(cell_count));
    u32::try_from(first).unwrap()..u32::try_from(last).unwrap()
}

/// The rank of each needed range: the ranges of shared packs in tree order, then the others.
fn range_ranks(case: &Generated) -> Vec<u32> {
    let shared = |n: &Needed| case.list.packs[n.pack as usize].pack_flags & 2 != 0;
    let order: Vec<usize> = (0..case.needed.len())
        .filter(|&i| shared(&case.needed[i]))
        .chain((0..case.needed.len()).filter(|&i| !shared(&case.needed[i])))
        .collect();
    let mut ranks = vec![0; case.needed.len()];
    for (rank, i) in order.into_iter().enumerate() {
        ranks[i] = u32::try_from(rank).unwrap();
    }
    ranks
}

fn reference(case: &Generated) -> BTreeMap<u32, RefPack> {
    let range_rank = range_ranks(case);
    let mut out = BTreeMap::new();
    for (index, record) in case.list.packs.iter().enumerate() {
        let index = u32::try_from(index).unwrap();
        let count = record.cell_count as usize;
        let mut needed: Vec<Option<u32>> = vec![None; count];
        for (i, n) in case.needed.iter().enumerate() {
            if n.pack != index {
                continue;
            }
            for c in cell_range(n.range, record.cell_count) {
                let r = range_rank[i];
                let slot = &mut needed[c as usize];
                *slot = Some(slot.map_or(r, |old| old.min(r)));
            }
        }
        let needed_cells = u32::try_from(needed.iter().flatten().count()).unwrap();
        if needed_cells == 0 {
            continue;
        }
        let medium = case.media.get(&index).copied().unwrap_or(Medium::Cold);
        let gap = case.config.gap_cells(medium, 20) as usize;
        // Read through each stretch of at most `gap` absent cells between two needed cells.
        let mut runs = needed.clone();
        let mut last_needed: Option<usize> = None;
        for c in 0..count {
            if needed[c].is_some() {
                if let Some(prev) = last_needed
                    && c - prev - 1 <= gap
                {
                    runs[prev + 1..c].fill(needed[prev]);
                }
                last_needed = Some(c);
            }
        }
        let merged = u32::try_from(runs.iter().flatten().count()).unwrap();
        let cov = |pct: u8| u64::from(merged) * 100 >= u64::from(pct) * count as u64;
        let wants_whole = cov(case.config.whole_cov_percent)
            || (record.pack_size <= case.config.small_pack_bytes
                && cov(case.config.small_whole_cov_percent));
        let downgrade = case.backlog.exceeds(case.config.backlog_ms);
        let (fetch, ranks) = if !wants_whole {
            (Fetch::Runs, runs)
        } else if downgrade {
            (Fetch::RunsUnderBacklog, runs)
        } else {
            let first = needed.iter().flatten().next().copied();
            let mut whole = vec![None; count];
            let mut before: Option<u32> = None;
            for c in 0..count {
                if let Some(r) = needed[c] {
                    before = Some(r);
                }
                whole[c] = before.or(first);
            }
            (Fetch::Whole, whole)
        };
        out.insert(
            index,
            RefPack {
                ranks,
                needed: needed_cells,
                merged,
                fetch,
            },
        );
    }
    out
}

fn check_ensure_shape(plan: &Plan, seed: u64) {
    let keys: Vec<(u32, u32)> = plan.ensures.iter().map(|e| (e.rank.0, e.pack)).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        keys, sorted,
        "seed {seed}: one ensure per (rank, pack), in order"
    );
    for e in &plan.ensures {
        assert!(!e.runs.is_empty(), "seed {seed}");
        for pair in e.runs.windows(2) {
            assert!(
                pair[0].first + pair[0].count < pair[1].first,
                "seed {seed}: runs ascend and never touch: {:?}",
                e.runs
            );
        }
        assert!(e.runs.iter().all(|r| r.count > 0), "seed {seed}");
    }
}

#[test]
fn the_plan_equals_the_cell_by_cell_reference() {
    let mut whole = 0;
    let mut downgraded = 0;
    let mut runs = 0;
    let mut reordered = 0;
    for seed in 0..2_000 {
        let case = generate(seed);
        // A case where shared packs first moves a range ahead of an earlier one.
        let ranks = range_ranks(&case);
        reordered += usize::from(ranks.iter().enumerate().any(|(i, &r)| r as usize != i));
        let plan = Case {
            list: &case.list,
            needed: case.needed.clone(),
            media: case.media.clone(),
            backlog: case.backlog,
            config: case.config,
        }
        .plan();
        check_ensure_shape(&plan, seed);
        let want = reference(&case);

        let mut want_cells = BTreeMap::new();
        for (&p, r) in &want {
            for (c, rank) in r.ranks.iter().enumerate() {
                if let Some(rank) = rank {
                    want_cells.insert((p, u32::try_from(c).unwrap()), *rank);
                }
            }
        }
        assert_eq!(per_cell(&plan), want_cells, "seed {seed}");

        let got: Vec<(u32, u32, u32, u32, Fetch)> = plan
            .packs
            .iter()
            .map(|p| {
                (
                    p.pack,
                    p.needed_cells,
                    p.merged_cells,
                    p.fetch_cells,
                    p.fetch,
                )
            })
            .collect();
        let expected: Vec<(u32, u32, u32, u32, Fetch)> = want
            .iter()
            .map(|(&p, r)| {
                let fetched = u32::try_from(r.ranks.iter().flatten().count()).unwrap();
                (p, r.needed, r.merged, fetched, r.fetch)
            })
            .collect();
        assert_eq!(got, expected, "seed {seed}");
        for p in &plan.packs {
            match p.fetch {
                Fetch::Whole => whole += 1,
                Fetch::RunsUnderBacklog => downgraded += 1,
                Fetch::Runs => runs += 1,
            }
        }
    }
    // The generated cases reach every decision often, and reorder ranges by shared packs often,
    // so the comparison covers each.
    assert!(
        whole > 100 && downgraded > 100 && runs > 100 && reordered > 100,
        "{whole} {downgraded} {runs} {reordered}"
    );
}
