//! C-10, garbage collection replayed offline (design section 16 C).
//!
//! The objects of the listing arrive when silo's listing says they were written. The CAS
//! service's garbage collection (minimum TTL 7 days, action TTL 30 days) had not run when the
//! listing was taken (silo saw only some 1,800 deletes), so when they die is **synthetic**; each
//! scenario states its rule:
//!
//! | scenario | an object dies at |
//! |---|---|
//! | `min-ttl` | arrival + 7 days (`min_ttl`), every object |
//! | `ttl-by-kind` | arrival + 30 days (`action_ttl`) for a task output (a key with `cas/b/`), arrival + 7 days for the rest |
//! | `footer-dead` | arrival for the keys of `--dead-keys` (packs dead at arrival by the footer rule, from the C-6 arm), as `ttl-by-kind` for the rest; run only with `--dead-keys` |
//!
//! Each scenario is replayed under two groupings of the objects into expiry classes:
//! `arrival` (every object is class 0: blocks fill in arrival order) and `expiry` (the class is
//! the week the object dies in, modulo 8, as the CAS service's `x-ts-expiry` hint would say it),
//! and under the two profiles of design section 21 ([`Policy::now`] and [`Policy::later`]), with
//! NOW also in its first state before erasure coding ([`Policy::now_replicas`]: two copies).
//! The open segment an object is appended to is the first 8 bytes of blake3 of its key (little
//! endian) modulo the number of open segments per class, with the class: an equal share of the
//! keys to each open segment, the hash standing in for the rendezvous rank's choice of replica
//! set and writer (design 10.2, 10.3). That number is the nodes when every node holds a copy
//! (LATER: 3) and the unordered replica sets otherwise (NOW: C(6,2) = 15).
//!
//! The model itself (sealing, destage, stripes, frees, the rewrite cost) is in the module `sim`.

mod sim;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use sim::{
    BlockSizes, DAY_MS, HOUR_MS, Item, Policy, Replay, Rewrite, Sample, Snapshot, StripeView,
    deciles, expiry_class, ratio, replay, rewrite_costs,
};

use crate::Error;
use crate::listing::{self, Object};
use crate::table::{self, Table};

/// The shortest life of an object: `min_ttl`, 7 days.
pub const MIN_TTL_MS: u64 = 7 * DAY_MS;
/// The life of a task output: `action_ttl`, 30 days.
pub const ACTION_TTL_MS: u64 = 30 * DAY_MS;

/// The arguments of `storebench c10`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The listing of silo's objects (the file `list` wrote).
    #[arg(long)]
    pub listing: PathBuf,
    /// Packs dead at arrival by the footer rule, one `bucket TAB key` per line; adds the
    /// `footer-dead` scenario.
    #[arg(long)]
    pub dead_keys: Option<PathBuf>,
    /// Where the summary is written, as JSON.
    #[arg(long)]
    pub out: PathBuf,
}

/// When the objects die.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// Every object dies 7 days after it arrived.
    MinTtl,
    /// Task outputs die 30 days after arrival, the rest 7 days.
    TtlByKind,
    /// Dead keys die on arrival, the rest as [`Scenario::TtlByKind`].
    FooterDead,
}

impl Scenario {
    /// The scenario's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Scenario::MinTtl => "min-ttl",
            Scenario::TtlByKind => "ttl-by-kind",
            Scenario::FooterDead => "footer-dead",
        }
    }

    /// When `a` dies, in milliseconds since the epoch.
    #[must_use]
    pub fn death_ms(self, a: &Arrival) -> u64 {
        let by_kind = if a.task_output {
            ACTION_TTL_MS
        } else {
            MIN_TTL_MS
        };
        let life = match self {
            Scenario::MinTtl => MIN_TTL_MS,
            Scenario::TtlByKind => by_kind,
            Scenario::FooterDead if a.dead_on_arrival => 0,
            Scenario::FooterDead => by_kind,
        };
        a.arrival_ms + life
    }
}

/// How the objects are grouped into expiry classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Grouping {
    /// One class: blocks fill in arrival order.
    Arrival,
    /// The week of the object's death, modulo the classes.
    Expiry,
}

impl Grouping {
    /// The grouping's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Grouping::Arrival => "arrival",
            Grouping::Expiry => "expiry",
        }
    }

    /// The class of an object that dies at `death_ms`.
    #[must_use]
    pub fn class(self, death_ms: u64, classes_max: u64) -> u64 {
        match self {
            Grouping::Arrival => 0,
            Grouping::Expiry => expiry_class(death_ms, classes_max),
        }
    }
}

/// An object of the listing as the replay needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrival {
    /// When it was written, in milliseconds since the epoch.
    pub arrival_ms: u64,
    /// Its size in bytes (not zero).
    pub size: u64,
    /// The hash that picks its writer ([`writer_hash`]).
    pub writer_hash: u64,
    /// Whether it is a task output (its key has `cas/b/`).
    pub task_output: bool,
    /// Whether the footer rule found it dead at arrival.
    pub dead_on_arrival: bool,
}

/// The first 8 bytes of blake3 of `key`, little endian: the open segment is this modulo the open segments per class
/// (`Policy::open_segments`).
#[must_use]
pub fn writer_hash(key: &str) -> u64 {
    let [a, b, c, d, e, f, g, h, ..] = *blake3::hash(key.as_bytes()).as_bytes();
    u64::from_le_bytes([a, b, c, d, e, f, g, h])
}

/// The packs of the dead-keys file `path`: `(bucket, key)` of each line.
///
/// # Errors
/// [`Error::Io`] when the file cannot be read; [`Error::Parse`] for a line without a tab.
pub fn read_dead_keys(path: &Path) -> Result<HashSet<(String, String)>, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io("read", path, e))?;
    let mut keys = HashSet::new();
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.is_empty()) {
        let Some((bucket, key)) = line.split_once('\t') else {
            return Err(Error::parse(path, i + 1, "not `bucket TAB key`"));
        };
        keys.insert((bucket.to_owned(), key.to_owned()));
    }
    Ok(keys)
}

/// The objects of `objects` that hold bytes, as arrivals; those in `dead` are dead on arrival.
#[must_use]
pub fn arrivals(objects: &[Object], dead: &HashSet<(String, String)>) -> Vec<Arrival> {
    objects
        .iter()
        .filter(|o| o.size > 0)
        .map(|o| Arrival {
            arrival_ms: o.modified_ms,
            size: o.size,
            writer_hash: writer_hash(&o.key),
            task_output: o.key.contains("cas/b/"),
            dead_on_arrival: dead.contains(&(o.bucket.clone(), o.key.clone())),
        })
        .collect()
}

/// The replay's items for `arrivals` under `scenario` and `grouping`.
#[must_use]
pub fn items(
    arrivals: &[Arrival],
    scenario: Scenario,
    grouping: Grouping,
    classes_max: u64,
) -> Vec<Item> {
    arrivals
        .iter()
        .map(|a| {
            let death_ms = scenario.death_ms(a);
            Item {
                arrival_ms: a.arrival_ms,
                size: a.size,
                death_ms,
                writer_hash: a.writer_hash,
                class: grouping.class(death_ms, classes_max),
            }
        })
        .collect()
}

/// One replay: a profile, a scenario and a grouping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// The profile's name.
    pub profile: String,
    /// The scenario's name.
    pub scenario: String,
    /// The grouping's name.
    pub grouping: String,
    /// What the replay gave.
    pub replay: Replay,
}

impl Run {
    /// `profile/scenario/grouping`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}/{}", self.profile, self.scenario, self.grouping)
    }
}

/// The summary `storebench c10` writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Objects with bytes that were replayed.
    pub objects: u64,
    /// Of them, the ones the footer rule found dead on arrival.
    pub dead_on_arrival: u64,
    /// The first arrival, in milliseconds since the epoch.
    pub first_arrival_ms: u64,
    /// The last arrival, in milliseconds since the epoch.
    pub last_arrival_ms: u64,
    /// The profiles, with every constant of the model.
    pub profiles: Vec<Policy>,
    /// One replay per profile, scenario and grouping.
    pub runs: Vec<Run>,
}

/// Replays `arrivals` under every profile, scenario and grouping (`footer_dead` adds the
/// scenario of that name).
#[must_use]
pub fn summarize(arrivals: &[Arrival], footer_dead: bool) -> Summary {
    let mut scenarios = vec![Scenario::MinTtl, Scenario::TtlByKind];
    if footer_dead {
        scenarios.push(Scenario::FooterDead);
    }
    let profiles = vec![Policy::now(), Policy::now_replicas(), Policy::later()];
    let mut runs = Vec::new();
    for policy in &profiles {
        for &scenario in &scenarios {
            for grouping in [Grouping::Arrival, Grouping::Expiry] {
                let its = items(arrivals, scenario, grouping, policy.classes_max);
                runs.push(Run {
                    profile: policy.name.clone(),
                    scenario: scenario.name().to_owned(),
                    grouping: grouping.name().to_owned(),
                    replay: replay(policy, its),
                });
            }
        }
    }
    Summary {
        objects: arrivals.len() as u64,
        dead_on_arrival: arrivals.iter().filter(|a| a.dead_on_arrival).count() as u64,
        first_arrival_ms: arrivals.iter().map(|a| a.arrival_ms).min().unwrap_or(0),
        last_arrival_ms: arrivals.iter().map(|a| a.arrival_ms).max().unwrap_or(0),
        profiles,
        runs,
    }
}

/// The two moments of a replay that are tabulated, with their labels.
fn moments(run: &Run) -> [(&'static str, &Snapshot); 2] {
    [
        ("end of data", &run.replay.end_of_data),
        ("end of replay", &run.replay.end_of_replay),
    ]
}

/// A row that starts with the run's three names and the moment.
fn head(run: &Run, at: &str) -> Vec<String> {
    vec![
        run.profile.clone(),
        run.scenario.clone(),
        run.grouping.clone(),
        at.to_owned(),
    ]
}

/// The tables of the measurement document, titled `C-10, ...`.
#[must_use]
pub fn tables(summary: &Summary) -> Vec<Table> {
    let mut cold = Table::new(
        "C-10, cold tier at the end of the data and at the end of the replay",
        &[
            "profile",
            "deaths",
            "grouping",
            "at",
            "cold raw",
            "live in cold",
            "cold_fill_efficiency %",
            "stripes",
            "freed whole",
            "partial",
            "dropped hot",
            "compacted hot",
            "bytes compacted",
        ],
    );
    let mut sizes = Table::new(
        "C-10, sealed blocks by size so far",
        &[
            "profile",
            "deaths",
            "grouping",
            "at",
            "under seal_min_bytes",
            "seal_min_bytes up to full",
            "full",
        ],
    );
    let tenths: Vec<String> = (0..10)
        .map(|i| format!("{}-{}%", i * 10, i * 10 + 10))
        .collect();
    let mut columns = vec!["profile", "deaths", "grouping", "at"];
    columns.extend(tenths.iter().map(String::as_str));
    let mut live = Table::new("C-10, stripes not freed by live fraction", &columns);
    let mut rewrite = Table::new(
        "C-10, rewrite cost below each rewrite_below",
        &[
            "profile",
            "deaths",
            "grouping",
            "at",
            "rewrite_below",
            "stripes",
            "rewrite bytes",
            "raw freed",
            "written per raw byte freed",
            "written per logical byte freed",
        ],
    );
    for run in &summary.runs {
        for (at, snap) in moments(run) {
            let s = &snap.sample;
            let mut row = head(run, at);
            row.extend([
                table::bytes(s.cold_raw_bytes),
                table::bytes(s.live_bytes),
                table::num(s.cold_fill_efficiency * 100.0, 1),
                table::count(s.stripes_live),
                table::count(s.stripes_freed),
                table::count(s.partial_stripes),
                table::count(s.blocks_dropped_hot),
                table::count(s.blocks_compacted_hot),
                table::bytes(s.compacted_bytes),
            ]);
            cold.row(row);
            let mut row = head(run, at);
            let b = &snap.sealed_blocks;
            row.extend([
                table::count(b.under_seal_min),
                table::count(b.partly_full),
                table::count(b.full),
            ]);
            sizes.row(row);
            let mut row = head(run, at);
            row.extend(snap.live_fraction_deciles.iter().map(|&n| table::count(n)));
            live.row(row);
            for r in &snap.rewrite {
                let mut row = head(run, at);
                row.extend([
                    table::num(r.below, 2),
                    table::count(r.stripes),
                    table::bytes(r.rewrite_bytes),
                    table::bytes(r.raw_freed),
                    table::num(r.per_raw_byte_freed, 2),
                    table::num(r.per_logical_byte_freed, 2),
                ]);
                rewrite.row(row);
            }
        }
    }
    vec![cold, weekly(summary), sizes, live, rewrite]
}

/// The cold_fill_efficiency (percent) of every run on every seventh day.
fn weekly(summary: &Summary) -> Table {
    let labels: Vec<String> = summary.runs.iter().map(Run::label).collect();
    let mut columns = vec!["day"];
    columns.extend(labels.iter().map(String::as_str));
    let mut t = Table::new("C-10, cold_fill_efficiency % by week", &columns);
    let days = summary
        .runs
        .iter()
        .map(|r| r.replay.samples.len())
        .max()
        .unwrap_or(0);
    for day in (0..days).step_by(7) {
        let mut row = vec![day.to_string()];
        row.extend(summary.runs.iter().map(|r| {
            r.replay.samples.get(day).map_or_else(String::new, |s| {
                table::num(s.cold_fill_efficiency * 100.0, 1)
            })
        }));
        t.row(row);
    }
    t
}

/// Runs the arm: reads the listing (and the dead keys), replays, writes the JSON summary to
/// `args.out` and the tables to `stdout`.
///
/// # Errors
/// [`Error::Io`] or [`Error::Parse`] for a file that cannot be read or is malformed or an
/// output that cannot be written; [`Error::Usage`] when the listing has no object with bytes.
pub fn run(args: &Args, stdout: &mut dyn Write) -> Result<(), Error> {
    let objects = listing::read(&args.listing)?;
    let dead = match &args.dead_keys {
        Some(path) => read_dead_keys(path)?,
        None => HashSet::new(),
    };
    let arrivals = arrivals(&objects, &dead);
    if arrivals.is_empty() {
        return Err(Error::Usage(format!(
            "{} has no object with bytes to replay",
            args.listing.display()
        )));
    }
    let summary = summarize(&arrivals, args.dead_keys.is_some());
    serde_json::to_string_pretty(&summary)
        .map_err(std::io::Error::from)
        .and_then(|json| std::fs::write(&args.out, json))
        .map_err(|e| Error::io("write", &args.out, e))?;
    let say = |e| Error::io("write", "standard output", e);
    for t in tables(&summary) {
        writeln!(stdout, "{}", t.markdown()).map_err(say)?;
    }
    writeln!(
        stdout,
        "{} objects replayed in {} runs; summary in {}",
        summary.objects,
        summary.runs.len(),
        args.out.display()
    )
    .map_err(say)
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
