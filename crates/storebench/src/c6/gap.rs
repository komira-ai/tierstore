//! The gap: what silo's drives hold against what its objects should cost under erasure coding.
//!
//! Silo stores each object as `data + parity` shards, one on each drive of its erasure set. On
//! every drive an object is an `xl.meta` file and, unless it is small enough to be inlined into
//! `xl.meta`, a part file. [`Model::object`] is an *assumed* model of that cost, with its
//! constants named below; the measured side is the `du` reports of some of silo's drives, summed
//! and extrapolated to all of them.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::du::{DuReport, Entry, Filesystem};
use crate::listing::Object;

use super::ratio;

/// The model's assumption: the bytes of metadata in an `xl.meta` that holds a shard inline.
pub const META_BYTES: u64 = 1024;

/// The model's assumption: a shard of at most this many bytes is inlined into `xl.meta`.
pub const INLINE_MAX_SHARD: u64 = 128 * 1024;

/// The model's assumption: the file system allocates in blocks of this many bytes.
pub const FS_BLOCK: u64 = 4096;

/// The model's assumption: the bytes an `xl.meta` takes when it holds no shard (it is one file
/// system block).
pub const XL_META_FILE: u64 = 4096;

/// The model's assumption: an object is erasure-coded and checksummed in blocks of this many
/// bytes.
pub const ERASURE_BLOCK: u64 = 1 << 20;

/// The model's assumption: the bytes of bitrot checksum for each started erasure block.
pub const BITROT_BYTES: u64 = 32;

/// What a set of objects costs on the drives: allocated bytes and files, over all drives of the
/// erasure set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Raw {
    /// Allocated bytes.
    pub bytes: u64,
    /// Files (`xl.meta` and part files).
    pub files: u64,
}

impl Raw {
    /// Adds `other`.
    pub fn add(&mut self, other: Raw) {
        self.bytes += other.bytes;
        self.files += other.files;
    }
}

/// The erasure layout the model costs objects under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// The drives silo has (what a measured drive is extrapolated to).
    pub drives: u64,
    /// Data shards of an object.
    pub data: u64,
    /// Parity shards of an object.
    pub parity: u64,
}

impl Model {
    /// A model of `drives` drives with `data` and `parity` shards.
    ///
    /// # Errors
    /// [`Error::Usage`] when there are no drives or no data shards.
    pub fn new(drives: u64, data: u64, parity: u64) -> Result<Self, Error> {
        if drives == 0 || data == 0 {
            return Err(Error::Usage(
                "--drives and --data-shards must be at least 1".to_owned(),
            ));
        }
        Ok(Self {
            drives,
            data,
            parity,
        })
    }

    /// What an object of `size` bytes costs: per drive one `xl.meta`, and one part file unless
    /// the shard (`ceil(size / data)`) is at most [`INLINE_MAX_SHARD`], in which case `xl.meta`
    /// holds it. Bitrot is [`BITROT_BYTES`] for each started [`ERASURE_BLOCK`] of the object.
    #[must_use]
    pub fn object(&self, size: u64) -> Raw {
        let shard = size.div_ceil(self.data);
        let bitrot = size.div_ceil(ERASURE_BLOCK) * BITROT_BYTES;
        let (per_drive, files) = if shard <= INLINE_MAX_SHARD {
            ((META_BYTES + shard + bitrot).next_multiple_of(FS_BLOCK), 1)
        } else {
            (
                XL_META_FILE + (shard + bitrot).next_multiple_of(FS_BLOCK),
                2,
            )
        };
        let shards = self.data + self.parity;
        Raw {
            bytes: shards * per_drive,
            files: shards * files,
        }
    }

    /// What an object of `size` bytes costs when its `xl.meta` states its layout: its own data
    /// and parity shards, and inlined or not as stated (else as [`Model::object`]).
    #[must_use]
    pub fn object_as(&self, size: u64, facts: Option<&Erasure>) -> Raw {
        let Some(f) = facts else {
            return self.object(size);
        };
        let shard = size.div_ceil(f.data);
        let bitrot = size.div_ceil(ERASURE_BLOCK) * BITROT_BYTES;
        let (per_drive, files) = if f.inline {
            ((META_BYTES + shard + bitrot).next_multiple_of(FS_BLOCK), 1)
        } else {
            (
                XL_META_FILE + (shard + bitrot).next_multiple_of(FS_BLOCK),
                2,
            )
        };
        let shards = f.data + f.parity;
        Raw {
            bytes: shards * per_drive,
            files: shards * files,
        }
    }
}

/// The erasure facts of one object as its `xl.meta` states them (`storebench drive-list
/// --meta-out`): silo raises an object's parity when drives are offline at its write, so an
/// object of an 8 + 4 set may be 7 + 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Erasure {
    /// Data shards.
    pub data: u64,
    /// Parity shards.
    pub parity: u64,
    /// Whether the bytes are inlined in `xl.meta`.
    pub inline: bool,
}

/// The erasure facts of objects, by `(bucket, key)`.
pub type ErasureFacts = BTreeMap<(String, String), Erasure>;

/// Reads erasure-fact files (`bucket key data parity parts inline versions`, tab-separated).
///
/// # Errors
/// [`Error::Io`] when a file cannot be read; [`Error::Parse`] for a line that is not one.
pub fn read_erasure(paths: &[PathBuf]) -> Result<ErasureFacts, Error> {
    let mut facts = ErasureFacts::new();
    for path in paths {
        let file = std::fs::File::open(path).map_err(|e| Error::io("open", path, e))?;
        for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|e| Error::io("read", path, e))?;
            let f: Vec<&str> = line.split('\t').collect();
            let [bucket, key, data, parity, _parts, inline, _versions] = f.as_slice() else {
                return Err(Error::parse(
                    path,
                    i + 1,
                    format!("{} fields, not 7", f.len()),
                ));
            };
            let num = |v: &str| {
                v.parse::<u64>()
                    .map_err(|_| Error::parse(path, i + 1, format!("{v:?} is not a number")))
            };
            let erasure = Erasure {
                data: num(data)?,
                parity: num(parity)?,
                inline: num(inline)? == 1,
            };
            if erasure.data == 0 {
                return Err(Error::parse(path, i + 1, "0 data shards"));
            }
            facts.insert(((*bucket).to_owned(), (*key).to_owned()), erasure);
        }
    }
    Ok(facts)
}

/// Objects of one erasure layout.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// Data shards.
    pub data: u64,
    /// Parity shards.
    pub parity: u64,
    /// Objects written so.
    pub objects: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Of them, inlined in `xl.meta`.
    pub inline: u64,
}

/// One bucket's expected cost against what its directory holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GapBucket {
    /// The bucket.
    pub bucket: String,
    /// Its objects.
    pub objects: u64,
    /// Their bytes.
    pub logical: u64,
    /// What the model says they cost on all drives.
    pub expected: Raw,
    /// The allocated bytes of the bucket's directory on all drives, from the du reports
    /// (`None` without reports or when none has the directory).
    pub measured_allocated: Option<u64>,
}

/// What the du reports say, extrapolated to all drives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// The whole root, summed.
    pub total: Entry,
    /// The file systems, summed.
    pub filesystem: Filesystem,
    /// Allocated bytes of the directories named after the listed buckets.
    pub buckets_allocated: u64,
    /// Allocated bytes of every other top-level entry (`.minio.sys`, ...).
    pub others_allocated: u64,
    /// Every entry outside the bucket directories, at every depth walked.
    pub others: Vec<Entry>,
    /// `total.allocated / L_silo`.
    pub ratio_total: f64,
    /// `buckets_allocated / L_silo`.
    pub ratio_buckets: f64,
}

/// The gap between the objects and the drives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    /// The drives of the erasure set.
    pub drives: u64,
    /// Data shards of the model.
    pub data_shards: u64,
    /// Parity shards of the model.
    pub parity_shards: u64,
    /// How many du reports were given.
    pub du_files: usize,
    /// The listed objects' bytes (`L_silo`).
    pub l_silo: u64,
    /// What the model says every listed object costs on all drives.
    pub expected: Raw,
    /// `expected.bytes / L_silo`.
    pub expected_ratio: f64,
    /// Per bucket, in name order.
    pub buckets: Vec<GapBucket>,
    /// Objects whose layout their `xl.meta` stated (the others are costed at the model's).
    #[serde(default)]
    pub facts: u64,
    /// The objects with stated layouts, by layout (data, parity), in that order.
    #[serde(default)]
    pub layouts: Vec<Layout>,
    /// The drives, measured (`None` without du reports).
    pub measured: Option<Measured>,
}

/// `sum` over `n` drives scaled to `drives` drives.
#[must_use]
pub fn extrapolate(sum: u64, drives: u64, n: usize) -> u64 {
    let scaled = u128::from(sum) * u128::from(drives) / (n.max(1) as u128);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The entries of all reports added up by path (the root as `.`), unscaled.
fn merge(reports: &[DuReport]) -> BTreeMap<String, Entry> {
    let mut merged: BTreeMap<String, Entry> = BTreeMap::new();
    for report in reports {
        for e in std::iter::once(&report.total).chain(&report.entries) {
            let sum = merged.entry(e.path.clone()).or_insert_with(|| Entry {
                path: e.path.clone(),
                ..Entry::default()
            });
            sum.files += e.files;
            sum.dirs += e.dirs;
            sum.apparent += e.apparent;
            sum.allocated += e.allocated;
            sum.errors += e.errors;
        }
    }
    merged
}

/// An entry scaled from `n` drives to `drives`.
fn scale(e: &Entry, drives: u64, n: usize) -> Entry {
    let up = |v: u64| extrapolate(v, drives, n);
    Entry {
        path: e.path.clone(),
        files: up(e.files),
        dirs: up(e.dirs),
        apparent: up(e.apparent),
        allocated: up(e.allocated),
        errors: up(e.errors),
    }
}

/// The gap of the `listing` under `model`, against the du `reports`.
#[must_use]
pub fn analyze(listing: &[Object], reports: &[DuReport], model: Model) -> Gap {
    analyze_with(listing, reports, model, None)
}

/// As [`analyze`], costing each object with stated erasure `facts` at its own layout.
#[must_use]
pub fn analyze_with(
    listing: &[Object],
    reports: &[DuReport],
    model: Model,
    facts: Option<&ErasureFacts>,
) -> Gap {
    let mut per_bucket: BTreeMap<&str, GapBucket> = BTreeMap::new();
    let mut layouts: BTreeMap<(u64, u64), Layout> = BTreeMap::new();
    let mut stated = 0;
    let mut expected = Raw::default();
    let mut l_silo = 0;
    for o in listing {
        let fact = facts.and_then(|f| f.get(&(o.bucket.clone(), o.key.clone())));
        if let Some(f) = fact {
            stated += 1;
            let l = layouts.entry((f.data, f.parity)).or_insert_with(|| Layout {
                data: f.data,
                parity: f.parity,
                ..Layout::default()
            });
            l.objects += 1;
            l.bytes += o.size;
            l.inline += u64::from(f.inline);
        }
        let cost = model.object_as(o.size, fact);
        expected.add(cost);
        l_silo += o.size;
        let b = per_bucket
            .entry(o.bucket.as_str())
            .or_insert_with(|| GapBucket {
                bucket: o.bucket.clone(),
                objects: 0,
                logical: 0,
                expected: Raw::default(),
                measured_allocated: None,
            });
        b.objects += 1;
        b.logical += o.size;
        b.expected.add(cost);
    }
    let n = reports.len();
    let merged = merge(reports);
    let names: BTreeSet<&str> = per_bucket.keys().copied().collect();
    let mut buckets: Vec<GapBucket> = per_bucket.into_values().collect();
    for b in &mut buckets {
        b.measured_allocated = merged
            .get(&b.bucket)
            .map(|e| extrapolate(e.allocated, model.drives, n));
    }
    let measured = (n > 0).then(|| {
        let mut buckets_allocated = 0;
        let mut others_allocated = 0;
        let mut others = Vec::new();
        for (path, e) in merged.iter().filter(|(path, _)| *path != ".") {
            let first = path.split('/').next().unwrap_or(path);
            let scaled = scale(e, model.drives, n);
            match (names.contains(first), path.contains('/')) {
                (true, false) => buckets_allocated += scaled.allocated,
                (true, true) => {}
                (false, false) => {
                    others_allocated += scaled.allocated;
                    others.push(scaled);
                }
                (false, true) => others.push(scaled),
            }
        }
        let total = merged
            .get(".")
            .map(|e| scale(e, model.drives, n))
            .unwrap_or_default();
        let sum = |f: fn(&Filesystem) -> u64| {
            extrapolate(
                reports.iter().map(|r| f(&r.filesystem)).sum(),
                model.drives,
                n,
            )
        };
        Measured {
            ratio_total: ratio(total.allocated, l_silo),
            ratio_buckets: ratio(buckets_allocated, l_silo),
            total,
            filesystem: Filesystem {
                size: sum(|f| f.size),
                used: sum(|f| f.used),
                free: sum(|f| f.free),
            },
            buckets_allocated,
            others_allocated,
            others,
        }
    });
    Gap {
        drives: model.drives,
        data_shards: model.data,
        parity_shards: model.parity,
        du_files: n,
        l_silo,
        expected,
        expected_ratio: ratio(expected.bytes, l_silo),
        buckets,
        facts: stated,
        layouts: layouts.into_values().collect(),
        measured,
    }
}

#[cfg(test)]
#[path = "gap_tests.rs"]
mod tests;
