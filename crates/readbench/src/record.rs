//! The records the harness writes, one JSON object per line, and reads back for the report:
//! a client's run over one tree ([`ClientRun`]), a demand read ([`DemandRead`]), one interval of
//! one drive ([`DiskSample`]) and the change of one series of a node's metrics page over a
//! window ([`MetricDelta`]). [`Summary`] is the percentiles every table prints.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Error;

/// The count, median, 99th percentile and largest of a set of values, by nearest rank.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// How many values.
    pub n: u64,
    /// The median (0 for no value).
    pub p50: u64,
    /// The 99th percentile (0 for no value).
    pub p99: u64,
    /// The largest (0 for no value).
    pub max: u64,
}

impl Summary {
    /// The summary of `values` (sorted here; the order given does not matter).
    #[must_use]
    pub fn of(values: &[u64]) -> Self {
        let mut v = values.to_vec();
        v.sort_unstable();
        let n = v.len() as u64;
        if v.is_empty() {
            return Self::default();
        }
        Self {
            n,
            p50: nearest_rank(&v, 50),
            p99: nearest_rank(&v, 99),
            max: v[v.len() - 1],
        }
    }
}

/// The `pct`-th percentile of sorted, non-empty `v` by nearest rank: the value at
/// `ceil(pct / 100 x n)`, counted from 1.
#[must_use]
pub fn nearest_rank(v: &[u64], pct: u64) -> u64 {
    let n = v.len() as u64;
    let rank = (pct * n).div_ceil(100).max(1);
    v[usize::try_from(rank - 1)
        .unwrap_or(usize::MAX)
        .min(v.len() - 1)]
}

/// How a client fetched its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The read path of the redesign: the planner's ensures through the fetch engine.
    Engine,
    /// The shipped daemon's way: one ranged read per file, in `fill_chunk` pieces, through the
    /// filer.
    Shipped,
}

/// One client's fetch of one tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRun {
    /// The name of the run (the same for every client of one load).
    pub run: String,
    /// How the tree was fetched.
    pub mode: Mode,
    /// The tree: the hex digest of its root manifest.
    pub tree: String,
    /// This client's index in the run, from 0.
    pub client: u32,
    /// The clients of the run, on every node.
    pub clients: u32,
    /// The node the client ran on, as the command line named it.
    pub node: String,
    /// The tree's directory manifests read (one GET each).
    pub manifests: u64,
    /// The tree's files with bytes.
    pub files: u64,
    /// The packs the tree's files are in.
    pub packs: u64,
    /// The bytes of the tree's files: what a job that reads every file needs.
    pub bytes_needed: u64,
    /// The bytes the client fetched from the store.
    pub bytes_fetched: u64,
    /// The GETs of pack bytes the client sent, by kind (`grid_part`, `demand_sub`, `retry`,
    /// `hedge` for the engine; `chunk` for the shipped way).
    pub gets: BTreeMap<String, u64>,
    /// Microseconds from the client's first request for pack bytes to the last byte of the
    /// tree present in its cache: the tree-ready time.
    pub tree_ready_us: u64,
    /// The first-byte times of its GETs, in microseconds.
    pub first_byte_us: Summary,
    /// The whole times of its GETs, in microseconds.
    pub get_us: Summary,
    /// GETs that failed (and were retried by the engine, or failed the run).
    pub get_errors: u64,
    /// The configuration of the read path the client ran with, as `name = value`.
    pub config: BTreeMap<String, String>,
}

impl ClientRun {
    /// Bytes fetched over bytes needed, in thousandths (0 when nothing was needed).
    #[must_use]
    pub fn amplification_milli(&self) -> u64 {
        (self.bytes_fetched.saturating_mul(1000))
            .checked_div(self.bytes_needed)
            .unwrap_or(0)
    }

    /// Every GET of pack bytes, of every kind.
    #[must_use]
    pub fn total_gets(&self) -> u64 {
        self.gets.values().sum()
    }
}

/// One demand read: a span of a file read by a job before Prepare fetched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandRead {
    /// The name of the run.
    pub run: String,
    /// The node the probe ran on.
    pub node: String,
    /// The clients fetching trees at the same time on every node (0 for an idle store).
    pub load_clients: u32,
    /// The bytes read.
    pub len: u64,
    /// Microseconds from the `wait` to its answer: the read's wait for its cells.
    pub wait_us: u64,
    /// Whether the cells came (`ready`) or the read failed (`eio`, `lost`).
    pub outcome: String,
}

/// One interval of one block device, from two readings of `/proc/diskstats`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskSample {
    /// The name of the run.
    pub run: String,
    /// The node, as the command line named it.
    pub node: String,
    /// The device (`sda`, `loop2`).
    pub device: String,
    /// The end of the interval, in milliseconds since the sampler started.
    pub t_ms: u64,
    /// Reads completed in the interval.
    pub reads: u64,
    /// Kilobytes read per second.
    pub rkb_s: f64,
    /// Mean milliseconds a read took, queue included (`r_await`); 0 with no read.
    pub r_await_ms: f64,
    /// Mean requests in the device's queue (`aqu-sz`).
    pub aqu_sz: f64,
    /// Percent of the interval the device was busy (`util`).
    pub util_pct: f64,
}

/// The change of one series of a node's metrics page between two readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDelta {
    /// The name of the run.
    pub run: String,
    /// The node, as the command line named it.
    pub node: String,
    /// The series' name.
    pub name: String,
    /// Its labels, as written on the page (`{op="get"}`), empty for none.
    pub labels: String,
    /// The value at the second reading less the value at the first.
    pub delta: f64,
    /// The milliseconds between the readings.
    pub window_ms: u64,
}

/// Appends `record` as one line of JSON to `out`.
///
/// # Errors
/// Writing to `out` failed (named as `path`).
pub fn write_line<T: Serialize>(out: &mut dyn Write, path: &Path, record: &T) -> Result<(), Error> {
    let mut line = serde_json::to_vec(record)
        .map_err(|e| Error::parse(path.display().to_string(), e.to_string()))?;
    line.push(b'\n');
    out.write_all(&line)
        .map_err(|e| Error::io("write", path, e))
}

/// Every record of a results file of one kind, one per line; blank lines are skipped.
///
/// # Errors
/// The file cannot be read, or a line is not a record of this kind (named by file and line).
pub fn read_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, Error> {
    let file = std::fs::File::open(path).map_err(|e| Error::io("open", path, e))?;
    let mut out = Vec::new();
    for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| Error::io("read", path, e))?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line)
            .map_err(|e| Error::parse(format!("{}:{}", path.display(), i + 1), e.to_string()))?;
        out.push(record);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
