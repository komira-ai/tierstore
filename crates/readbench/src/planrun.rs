//! What the Prepare planner would fetch for many trees, without fetching anything: each tree's
//! manifests are walked and its packs' sizes read from the filer, a list is made with
//! placeholder cell digests (the planner reads only the pack records and the ranges, never a
//! digest), and `tierstore_plan::plan::plan` runs on it for each variant of the planner's
//! constants with every pack on a cold drive and no backlog. One [`PlanRecord`] comes out per
//! tree and variant.

use std::collections::BTreeMap;

use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::list::ListL2;
use tierstore_plan::config::PlanConfig;
use tierstore_plan::plan::{Backlog, Fetch, Needed, PlanInput, plan};

use crate::Error;
use crate::manifest::walk;
use crate::record::PlanRecord;
use crate::replay::{fail, placeholder_list, run_bytes};
use crate::swfs::FilerFetch;

/// One MiB, the size of a cell.
const MIB: u64 = 1 << 20;

/// A named set of the planner's constants.
#[derive(Debug, Clone)]
pub struct Variant {
    /// The name the records carry.
    pub name: String,
    /// The constants.
    pub config: PlanConfig,
}

impl Variant {
    /// The variant `design`: the planner's own defaults.
    #[must_use]
    pub fn design() -> Self {
        Self {
            name: "design".to_owned(),
            config: PlanConfig::default(),
        }
    }

    /// Reads `NAME=WHOLE_COV,SMALL_MIB,SMALL_COV`: the whole-pack coverage in percent, the
    /// largest pack of the lower coverage in MiB, and that lower coverage in percent. The other
    /// constants are the defaults'.
    ///
    /// # Errors
    /// [`Error::Usage`] when the text is not of that form.
    pub fn parse(arg: &str) -> Result<Self, Error> {
        let bad = |why: &str| Error::Usage(format!("--variant {arg:?}: {why}"));
        let (name, values) = arg
            .split_once('=')
            .filter(|(name, _)| !name.is_empty())
            .ok_or_else(|| bad("expected NAME=WHOLE_COV,SMALL_MIB,SMALL_COV"))?;
        let parts: Vec<&str> = values.split(',').collect();
        let [whole, small_mib, small_cov] = parts[..] else {
            return Err(bad("expected three numbers after the name"));
        };
        let number = |text: &str| text.parse::<u64>().map_err(|_| bad("not a number"));
        let percent = |text: &str| {
            number(text).and_then(|n| {
                u8::try_from(n)
                    .ok()
                    .filter(|p| *p <= 100)
                    .ok_or_else(|| bad("a coverage is from 0 to 100"))
            })
        };
        let config = PlanConfig {
            whole_cov_percent: percent(whole)?,
            small_pack_bytes: number(small_mib)?.saturating_mul(MIB),
            small_whole_cov_percent: percent(small_cov)?,
            ..PlanConfig::default()
        };
        Ok(Self {
            name: name.to_owned(),
            config,
        })
    }
}

/// The trees of a `--trees-file`: one per line, blank lines left out.
#[must_use]
pub fn parse_trees(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Plans the tree `tree` under each of `variants`, one record each.
///
/// # Errors
/// A manifest or a pack is missing or unreadable, or the list or the plan is refused.
pub async fn plan_tree(
    fetch: &FilerFetch,
    run: &str,
    tree: &str,
    variants: &[Variant],
    walk_parallel: usize,
    parallel: usize,
) -> Result<Vec<PlanRecord>, Error> {
    let walked = walk(tree, fetch, walk_parallel).await?;
    let l2 = placeholder_list(fetch, &walked, parallel).await?.l2;
    let needed: Vec<Needed> = l2
        .ranges()
        .map(|r| Needed {
            pack: r.pack,
            range: ByteRange {
                offset: r.offset,
                len: r.len,
            },
        })
        .collect();
    let list: &dyn ListL2 = &l2;
    let media = BTreeMap::new();
    let input = PlanInput {
        list,
        needed: &needed,
        profile: &[],
        media: &media,
        backlog: Backlog::default(),
    };
    let mut records = Vec::new();
    for variant in variants {
        let planned = plan(&input, &variant.config).map_err(fail("plan"))?;
        let mut bytes_planned = 0;
        for ensure in &planned.ensures {
            let size = list.pack_record(ensure.pack).map_or(0, |r| r.pack_size);
            bytes_planned += ensure
                .runs
                .iter()
                .map(|run| run_bytes(*run, size).len)
                .sum::<u64>();
        }
        let whole = planned
            .packs
            .iter()
            .filter(|p| p.fetch == Fetch::Whole)
            .count() as u64;
        records.push(PlanRecord {
            run: run.to_owned(),
            tree: tree.to_owned(),
            variant: variant.name.clone(),
            files: walked.files,
            packs: walked.packs,
            bytes_needed: walked.bytes,
            bytes_planned,
            packs_whole: whole,
            packs_runs: planned.packs.len() as u64 - whole,
            ensures: planned.ensures.len() as u64,
        });
    }
    Ok(records)
}

/// The records of the trees that were planned and the trees that were not, each with why.
#[derive(Debug, Default)]
pub struct Planned {
    /// One record per planned tree and variant, in the order of the trees and variants.
    pub records: Vec<PlanRecord>,
    /// The trees that failed, in order, with their errors.
    pub failed: Vec<(String, Error)>,
}

/// Plans every tree of `trees` one after the other; a tree that fails is listed in
/// [`Planned::failed`] and the rest go on.
pub async fn plan_trees(
    fetch: &FilerFetch,
    run: &str,
    trees: &[String],
    variants: &[Variant],
    walk_parallel: usize,
    parallel: usize,
) -> Planned {
    let mut out = Planned::default();
    for tree in trees {
        match plan_tree(fetch, run, tree, variants, walk_parallel, parallel).await {
            Ok(records) => out.records.extend(records),
            Err(e) => out.failed.push((tree.clone(), e)),
        }
    }
    out
}

#[cfg(test)]
#[path = "planrun_tests.rs"]
mod tests;
