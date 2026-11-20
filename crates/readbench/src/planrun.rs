//! What the Prepare planner would fetch for many trees, without fetching anything: each tree's
//! manifests are walked and its packs' sizes read from the filer, a list is made with
//! placeholder cell digests (the planner reads only the pack records and the ranges, never a
//! digest), and `tierstore_plan::plan::plan` runs on it for each variant of the planner's
//! constants with every pack on a cold drive and no backlog. One [`PlanRecord`] comes out per
//! tree and variant.

use std::collections::BTreeMap;

use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use tierstore::TreeDigest;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_fetch_api::list::ListL2;
use tierstore_list::{PackCells, PackInfo, TreeListL2, sidecar_from_v2};
use tierstore_plan::config::PlanConfig;
use tierstore_plan::plan::{Backlog, Fetch, Needed, PlanInput, plan};

use crate::Error;
use crate::manifest::{encode_v2, walk};
use crate::record::PlanRecord;
use crate::replay::{fail, pack_keys, run_bytes};
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

/// The size of every pack of `list`, from the filer's entries.
async fn pack_sizes(
    fetch: &FilerFetch,
    keys: Vec<String>,
    parallel: usize,
) -> Result<BTreeMap<String, u64>, Error> {
    stream::iter(keys)
        .map(|key| async move {
            match fetch.entry(&key).await? {
                Some(entry) => Ok((key, entry.size)),
                None => Err(Error::parse(key, "the filer has no such pack")),
            }
        })
        .buffer_unordered(parallel.max(1))
        .try_collect()
        .await
}

/// A pack's record for the sidecar: its size and one zeroed digest for each cell.
fn placeholder(sizes: &BTreeMap<String, u64>, key: &str) -> Option<PackInfo> {
    sizes.get(key).map(|size| PackInfo {
        cells: PackCells {
            size: *size,
            digests: (0..size.div_ceil(MIB)).map(|_| [0; 32]).collect(),
        },
        ..PackInfo::default()
    })
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
    let sizes = pack_sizes(fetch, pack_keys(&walked.list), parallel).await?;
    let v2 = encode_v2(&walked.list)?;
    let id = TreeDigest::of(&v2);
    let sidecar = sidecar_from_v2(&v2, |key| placeholder(&sizes, key)).map_err(fail("sidecar"))?;
    let l2 = TreeListL2::open_sidecar(Some(sidecar), id.as_bytes()).map_err(fail("sidecar"))?;
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
