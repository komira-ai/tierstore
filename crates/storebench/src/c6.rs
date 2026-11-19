//! `c6`: silo's objects (measurement C-6 of the storage-tier design, section 16 C).
//!
//! Reads the listing (every object of silo's buckets), optionally the footers file (every pack's
//! blobs) and the `du` reports of some of silo's drives, and writes a summary of
//!
//! - the objects by kind and size class, by bucket and by deployment prefix, and `L_silo`;
//! - the multipart objects and the ingest per day;
//! - `L_live`, by the CAS service's first-location rule ([`live`]);
//! - the tree-list check of the footers file;
//! - `large_min`: what falls on each side of 256 KiB, 1 MiB, 4 MiB and 16 MiB;
//! - first-fit-decreasing pack assembly of the objects above `large_min` ([`pack`]);
//! - the gap between the drives and the objects ([`gap`]).
//!
//! Every list of the summary is in a fixed order, so equal inputs give equal JSON.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::du::{DuReport, read_json, write_json};
use crate::kinds::{Kind, classify, prefix};
use crate::listing::{self, Object};

pub mod gap;
pub mod live;
pub mod pack;
mod tables;

pub use tables::tables;

use gap::{Gap, Model};
use live::{DeadPack, Footers, Live, TreeCheck};
use pack::Packing;

/// The upper bounds (inclusive) of the size classes; a sixth class holds what is larger.
pub const CLASS_BOUNDS: [u64; 5] = [64 << 10, 1 << 20, 4 << 20, 16 << 20, 256 << 20];

/// The size classes' names, in the order of the classes.
pub const CLASS_NAMES: [&str; 6] = [
    "<= 64 KiB",
    "<= 1 MiB",
    "<= 4 MiB",
    "<= 16 MiB",
    "<= 256 MiB",
    "> 256 MiB",
];

/// The `large_min` candidates: an object of at most this many bytes would be a segment record,
/// a larger one a whole file.
pub const LARGE_MINS: [u64; 4] = [256 << 10, 1 << 20, 4 << 20, 16 << 20];

/// Milliseconds in a day (days are counted from the Unix epoch, in UTC).
pub const DAY_MS: u64 = 86_400_000;

/// How many days at the end of the span count as the recent ingest.
pub const RECENT_DAYS: u64 = 7;

/// The arguments of `storebench c6`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The listing of silo's objects (`storebench list` or `storebench drive-list`).
    #[arg(long)]
    pub listing: PathBuf,
    /// A footers file of the packs (`storebench footers`); repeat for several scans, which are
    /// merged ([`live::Footers::merge`]). Without one `L_live` is absent.
    #[arg(long)]
    pub footers: Vec<PathBuf>,
    /// The report of `storebench du` on one silo drive; repeat for several drives.
    #[arg(long)]
    pub du: Vec<PathBuf>,
    /// The erasure facts of the objects (`storebench drive-list --meta-out`); repeat for
    /// several. An object with facts is costed at its own layout in the gap.
    #[arg(long)]
    pub erasure: Vec<PathBuf>,
    /// The drives of silo's erasure set (what the du reports are extrapolated to).
    #[arg(long, default_value_t = 12)]
    pub drives: u64,
    /// The data shards of an object.
    #[arg(long, default_value_t = 8)]
    pub data_shards: u64,
    /// The parity shards of an object.
    #[arg(long, default_value_t = 4)]
    pub parity_shards: u64,
    /// Where the keys of the packs found dead are written, `bucket TAB key` per line (needs
    /// `--footers`); the input of the garbage-collection replay.
    #[arg(long)]
    pub dead_out: Option<PathBuf>,
    /// Where the summary (JSON) is written.
    #[arg(long)]
    pub out: PathBuf,
}

/// A count of objects and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    /// Objects.
    pub objects: u64,
    /// Their bytes.
    pub bytes: u64,
}

impl Count {
    /// Adds one object of `size` bytes.
    pub fn add(&mut self, size: u64) {
        self.objects += 1;
        self.bytes += size;
    }
}

/// `part / whole` (`0.0` when `whole` is 0).
#[must_use]
pub fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    part as f64 / whole as f64
}

/// A count under a name (a bucket, a prefix).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    /// The bucket, or `bucket/prefix`.
    pub name: String,
    /// What it holds.
    pub count: Count,
}

/// The objects of one kind, in all and by size class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindSizes {
    /// The kind.
    pub kind: Kind,
    /// Its objects.
    pub total: Count,
    /// Its objects in each size class (`CLASS_NAMES` order).
    pub classes: Vec<Count>,
}

/// The objects written on one day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Day {
    /// The date (`2026-11-18`, UTC).
    pub date: String,
    /// What was written.
    pub count: Count,
}

/// When silo's objects were written.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ingest {
    /// The oldest object's time (milliseconds since the epoch).
    pub first_ms: Option<u64>,
    /// The newest object's time.
    pub last_ms: Option<u64>,
    /// Per day of the span with objects, in date order.
    pub per_day: Vec<Day>,
    /// The same for the last `RECENT_DAYS` days of the span.
    pub recent_days: Vec<Day>,
    /// The total of `recent_days`.
    pub recent_total: Count,
}

/// One day's objects on each side of a `large_min`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeMinDay {
    /// The date.
    pub date: String,
    /// Objects of at most `large_min` bytes.
    pub at_or_below: Count,
    /// Larger objects.
    pub above: Count,
}

/// What a `large_min` candidate does to the objects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeMin {
    /// The candidate.
    pub threshold: u64,
    /// Objects of at most it (segment records).
    pub at_or_below: Count,
    /// Larger objects (whole files).
    pub above: Count,
    /// Per day of the span with objects.
    pub per_day: Vec<LargeMinDay>,
}

/// The measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Listed objects.
    pub objects: u64,
    /// `L_silo`: the bytes of every listed object.
    pub l_silo: u64,
    /// By kind, in `Kind::ALL` order.
    pub kinds: Vec<KindSizes>,
    /// By bucket, in name order.
    pub buckets: Vec<Named>,
    /// By bucket and deployment prefix ([`label_prefixes`]): the keys with no prefix first, as
    /// `bucket/`, then each prefix as `bucket/deployment N`, by bytes.
    pub prefixes: Vec<Named>,
    /// Objects written by a multipart upload.
    pub multipart: Count,
    /// When they were written.
    pub ingest: Ingest,
    /// `L_live` and what it is made of.
    pub live: Live,
    /// The tree-list check of the footers file.
    pub trees: TreeCheck,
    /// One entry per `LARGE_MINS`.
    pub large_min: Vec<LargeMin>,
    /// First-fit-decreasing assembly of the objects above `large_min`.
    pub packing: Packing,
    /// The gap between drives and objects.
    pub gap: Gap,
}

/// The size class (index into `CLASS_NAMES`) of an object of `size` bytes.
#[must_use]
pub fn size_class(size: u64) -> usize {
    CLASS_BOUNDS.partition_point(|bound| *bound < size)
}

/// The date of the day `day` since the Unix epoch, as `YYYY-MM-DD` (proleptic Gregorian, UTC).
#[must_use]
pub fn date(day: u64) -> String {
    let z = day + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let d = day_of_year - (153 * mp + 2) / 5 + 1;
    let m = (mp + 2) % 12 + 1;
    let y = year_of_era + era * 400 + u64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The index of `kind` in `Kind::ALL`.
fn kind_index(kind: Kind) -> usize {
    match kind {
        Kind::Pack => 0,
        Kind::LargeBlob => 1,
        Kind::TreeList => 2,
        Kind::Other => 3,
    }
}

/// Days with counts as dated entries.
fn days<'a>(counts: impl Iterator<Item = (&'a u64, &'a Count)>) -> Vec<Day> {
    counts
        .map(|(day, count)| Day {
            date: date(*day),
            count: *count,
        })
        .collect()
}

/// Summarises `listing`, with the footers and du reports when there are any; also the packs
/// found dead, in the order they were walked.
#[must_use]
pub fn summarize(
    listing: &[Object],
    footers: Option<&Footers>,
    reports: &[DuReport],
    model: Model,
) -> (Summary, Vec<DeadPack>) {
    summarize_with(listing, footers, reports, model, None)
}

/// As [`summarize`], costing the objects with stated erasure `facts` at their own layout.
#[must_use]
pub fn summarize_with(
    listing: &[Object],
    footers: Option<&Footers>,
    reports: &[DuReport],
    model: Model,
    facts: Option<&gap::ErasureFacts>,
) -> (Summary, Vec<DeadPack>) {
    let mut kinds: Vec<KindSizes> = Kind::ALL
        .iter()
        .map(|kind| KindSizes {
            kind: *kind,
            total: Count::default(),
            classes: vec![Count::default(); CLASS_NAMES.len()],
        })
        .collect();
    let mut buckets: BTreeMap<&str, Count> = BTreeMap::new();
    let mut prefixes: BTreeMap<(&str, &str), Count> = BTreeMap::new();
    let mut multipart = Count::default();
    let mut per_day: BTreeMap<u64, Count> = BTreeMap::new();
    let mut split = [[Count::default(); 2]; LARGE_MINS.len()];
    let mut split_days: [BTreeMap<u64, [Count; 2]>; LARGE_MINS.len()] = Default::default();
    let (mut first, mut last): (Option<u64>, Option<u64>) = (None, None);
    let mut l_silo = 0;
    let mut arrivals: Vec<&Object> = Vec::new();
    for o in listing {
        let kind = &mut kinds[kind_index(classify(&o.key, o.size))];
        kind.total.add(o.size);
        kind.classes[size_class(o.size)].add(o.size);
        buckets.entry(o.bucket.as_str()).or_default().add(o.size);
        prefixes
            .entry((o.bucket.as_str(), prefix(&o.key)))
            .or_default()
            .add(o.size);
        if o.multipart() {
            multipart.add(o.size);
        }
        let day = o.modified_ms / DAY_MS;
        per_day.entry(day).or_default().add(o.size);
        first = Some(first.map_or(o.modified_ms, |f| f.min(o.modified_ms)));
        last = Some(last.map_or(o.modified_ms, |l| l.max(o.modified_ms)));
        for (i, threshold) in LARGE_MINS.iter().enumerate() {
            let side = usize::from(o.size > *threshold);
            split[i][side].add(o.size);
            split_days[i].entry(day).or_default()[side].add(o.size);
        }
        if o.size > pack::LARGE_MIN {
            arrivals.push(o);
        }
        l_silo += o.size;
    }
    arrivals.sort_by(|a, b| {
        (a.modified_ms, &a.key, &a.bucket).cmp(&(b.modified_ms, &b.key, &b.bucket))
    });
    let sizes: Vec<u64> = arrivals.iter().map(|o| o.size).collect();
    let recent_from = last.map_or(0, |ms| (ms / DAY_MS).saturating_sub(RECENT_DAYS - 1));
    let recent = per_day.range(recent_from..);
    let recent_total = recent.clone().fold(Count::default(), |mut sum, (_, c)| {
        sum.objects += c.objects;
        sum.bytes += c.bytes;
        sum
    });
    let ingest = Ingest {
        first_ms: first,
        last_ms: last,
        per_day: days(per_day.iter()),
        recent_days: days(recent),
        recent_total,
    };
    let large_min = LARGE_MINS
        .iter()
        .zip(split)
        .zip(split_days)
        .map(|((threshold, [at_or_below, above]), by_day)| LargeMin {
            threshold: *threshold,
            at_or_below,
            above,
            per_day: by_day
                .into_iter()
                .map(|(day, [at_or_below, above])| LargeMinDay {
                    date: date(day),
                    at_or_below,
                    above,
                })
                .collect(),
        })
        .collect();
    let (live, dead) = live::analyze(listing, footers);
    let named = |name: String, count: Count| Named { name, count };
    let summary = Summary {
        objects: listing.len() as u64,
        l_silo,
        kinds,
        buckets: buckets
            .into_iter()
            .map(|(name, count)| named(name.to_owned(), count))
            .collect(),
        prefixes: label_prefixes(prefixes),
        multipart,
        ingest,
        live,
        trees: live::tree_check(footers),
        large_min,
        packing: pack::packing(&sizes),
        gap: gap::analyze_with(listing, reports, model, facts),
    };
    (summary, dead)
}

/// Writes the dead packs to `path`, `bucket TAB key` per line.
///
/// # Errors
/// [`Error::Io`] when the file cannot be written.
pub fn write_dead(path: &Path, dead: &[DeadPack]) -> Result<(), Error> {
    let mut text = String::new();
    for pack in dead {
        text.push_str(&pack.bucket);
        text.push('\t');
        text.push_str(&pack.key);
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|e| Error::io("write", path, e))
}

/// The prefixes as named counts: the keys of a bucket with no deployment prefix (the CAS service's
/// own layout, `cas/...`) as `bucket/`, in bucket order; then every other prefix, most bytes
/// first (ties by name), as `bucket/deployment N` from 1. A deployment's prefix is the name its
/// operator chose, which a published summary does not carry; the count is what C-6 needs.
#[must_use]
pub fn label_prefixes(prefixes: BTreeMap<(&str, &str), Count>) -> Vec<Named> {
    let (roots, mut others): (Vec<_>, Vec<_>) =
        prefixes.into_iter().partition(|((_, p), _)| p.is_empty());
    others.sort_by(|(a, ca), (b, cb)| cb.bytes.cmp(&ca.bytes).then(a.cmp(b)));
    roots
        .into_iter()
        .map(|((bucket, _), count)| Named {
            name: format!("{bucket}/"),
            count,
        })
        .chain(
            others
                .into_iter()
                .enumerate()
                .map(|(i, ((bucket, _), count))| Named {
                    name: format!("{bucket}/deployment {}", i + 1),
                    count,
                }),
        )
        .collect()
}

/// Runs `storebench c6`: reads the inputs, writes the summary (and the dead packs) and prints
/// the tables of the measurement document.
///
/// # Errors
/// [`Error::Usage`] for a model without drives or data shards and for `--dead-out` without
/// `--footers`; [`Error::Io`] and [`Error::Parse`] for an input that cannot be read and an
/// output that cannot be written.
pub fn run(args: &Args, stdout: &mut dyn Write) -> Result<(), Error> {
    let model = Model::new(args.drives, args.data_shards, args.parity_shards)?;
    if args.dead_out.is_some() && args.footers.is_empty() {
        return Err(Error::Usage(
            "--dead-out needs --footers: dead packs are found from the footers".to_owned(),
        ));
    }
    let objects = listing::read(&args.listing)?;
    let footers = live::read_all_footers(&args.footers)?;
    let reports = args
        .du
        .iter()
        .map(|path| read_json::<DuReport>(path))
        .collect::<Result<Vec<_>, _>>()?;
    let facts = if args.erasure.is_empty() {
        None
    } else {
        Some(gap::read_erasure(&args.erasure)?)
    };
    let (summary, dead) =
        summarize_with(&objects, footers.as_ref(), &reports, model, facts.as_ref());
    if let Some(path) = &args.dead_out {
        write_dead(path, &dead)?;
    }
    write_json(&args.out, &summary)?;
    for table in tables(&summary) {
        writeln!(stdout, "{}", table.markdown())
            .map_err(|e| Error::io("write", "standard output", e))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "c6_tests.rs"]
mod tests;
