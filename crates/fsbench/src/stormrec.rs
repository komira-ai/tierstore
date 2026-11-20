//! The records of `fsbench storm` ([`crate::storm`]) and `fsbench storm-report`, the tables of a
//! measurement document made from them.
//!
//! A results file holds one [`StormRecord`] per line, one per run. The report groups the runs by
//! label, phase and probe, in the order they first appear, leaves out the first `discard` runs
//! of each label, and gives every figure as the median of the runs kept with their least and
//! most, `median (min-max)`, as `fsbench report` does. A figure a run did not measure is `-`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::hist::Histogram;
use crate::host::Environment;

/// The format of [`StormRecord`].
pub const FORMAT: u32 = 1;

/// The record of one run of the storm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StormRecord {
    /// [`FORMAT`].
    pub format: u32,
    /// The series' label (for example `uring-d5-park60`).
    pub label: String,
    /// The run, from 0.
    pub run: u32,
    /// What the session served on: `io_uring`, or `dev-fuse (<why not io_uring>)`.
    pub transport: String,
    /// The bytes the ring entries registered (`uring_registered_bytes`); `None` on `/dev/fuse`.
    pub uring_registered_bytes: Option<u64>,
    /// The entries of each queue; `None` on `/dev/fuse`.
    pub uring_depth: Option<u16>,
    /// The queues (possible CPUs); `None` on `/dev/fuse`.
    pub uring_queues: Option<u32>,
    /// The `/dev/fuse` readers (the mount's descriptor and its clones).
    pub readers: usize,
    /// How long a cold READ is parked, microseconds.
    pub park_us: u64,
    /// How the storm threads read: `buffered` or `direct`.
    pub read: String,
    /// The storm threads on each stormed CPU.
    pub per_cpu: u32,
    /// The CPU of the one-CPU storm.
    pub storm_cpu: usize,
    /// The CPU whose probe is not stormed in the one-CPU storm; `None` on a machine of one CPU.
    pub other_cpu: Option<usize>,
    /// What the operator recorded (`--setting KEY=VALUE`).
    pub settings: BTreeMap<String, String>,
    /// The machine and the mount.
    pub environment: Environment,
    /// The phases, in the order they ran.
    pub phases: Vec<PhaseRecord>,
}

/// One phase of a run: probes, alone or beside a storm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhaseRecord {
    /// `idle`, `one-cpu` or `every-cpu`.
    pub name: String,
    /// The storm threads (0 when idle).
    pub storm_threads: u32,
    /// The CPUs stormed.
    pub stormed_cpus: u32,
    /// How long the probes ran, nanoseconds.
    pub window_ns: u64,
    /// Cold reads completed by the storm threads while the probes ran.
    pub storm_reads: u64,
    /// Cold reads that failed, and storm threads that could not start (open, pin).
    pub storm_errors: u64,
    /// The same by step and error (`open: Input/output error (os error 5)`).
    pub storm_failures: BTreeMap<String, u64>,
    /// The probes.
    pub probes: Vec<ProbeRecord>,
    /// Ring entries holding a request not answered yet (`uring_entries_parked`), sampled.
    pub held: Sampled,
    /// Cold READs parked in the file system, sampled.
    pub parked: Sampled,
    /// Requests the kernel holds for the connection (`waiting`), sampled; `None` when the
    /// connection is not listed.
    pub waiting: Option<Sampled>,
}

/// One probe: a thread on one CPU that `stat`s the probe file over and over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRecord {
    /// `storm-cpu` (the CPU of the one-CPU storm) or `other-cpu`.
    pub role: String,
    /// The CPU.
    pub cpu: usize,
    /// Whether the phase stormed its CPU.
    pub stormed: bool,
    /// The `stat`s that failed.
    pub errors: u64,
    /// The latency of each `stat`, nanoseconds.
    pub stat: Histogram,
}

/// Samples of a count: their mean and largest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sampled {
    /// The samples taken.
    pub samples: u64,
    /// Their mean, 0 when none.
    pub mean: f64,
    /// The largest, 0 when none.
    pub max: u64,
}

impl Sampled {
    /// The samples of `values`.
    #[must_use]
    pub fn of(values: &[u64]) -> Self {
        let samples = values.len() as u64;
        let sum: u64 = values.iter().fold(0u64, |a, &v| a.saturating_add(v));
        Self {
            samples,
            mean: if samples == 0 {
                0.0
            } else {
                sum as f64 / samples as f64
            },
            max: values.iter().copied().max().unwrap_or(0),
        }
    }
}

/// The records of the results file at `path`, one per line.
///
/// # Errors
/// The file cannot be read, or a line is not a storm record (the error names it).
pub fn read(path: &Path) -> Result<Vec<StormRecord>, Error> {
    let text = std::fs::read_to_string(path).map_err(Error::at("read", path))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line).map_err(|e| Error::Parse {
                path: path.to_path_buf(),
                line: n + 1,
                why: format!("not a storm record: {e}"),
            })
        })
        .collect()
}

/// One row's runs: the phase and probe of each run kept, with the run's record.
type Row<'a> = Vec<(&'a StormRecord, &'a PhaseRecord, &'a ProbeRecord)>;

/// The key of a row: label, phase, probe role, stormed.
type Key<'a> = (&'a str, &'a str, &'a str, bool);

/// A figure of a row's run.
type Figure = fn(&StormRecord, &PhaseRecord, &ProbeRecord) -> Option<f64>;

/// The p99 of the probe of the same role in the run's idle phase, nanoseconds.
fn idle_p99(record: &StormRecord, role: &str) -> Option<f64> {
    let idle = record.phases.iter().find(|p| p.name == "idle")?;
    let probe = idle.probes.iter().find(|p| p.role == role)?;
    (probe.stat.count > 0).then(|| probe.stat.quantile(0.99) as f64)
}

fn per_s(count: u64, ns: u64) -> Option<f64> {
    (ns > 0).then(|| count as f64 * 1e9 / ns as f64)
}

/// The columns of the table.
const COLUMNS: &[(&str, Figure)] = &[
    ("stats/s", |_, phase, p| {
        per_s(p.stat.count, phase.window_ns)
    }),
    ("stat p50 us", |_, _, p| {
        Some(p.stat.quantile(0.5) as f64 / 1e3)
    }),
    ("stat p99 us", |_, _, p| {
        Some(p.stat.quantile(0.99) as f64 / 1e3)
    }),
    ("stat max ms", |_, _, p| Some(p.stat.max as f64 / 1e6)),
    ("p99 / idle p99", |r, _, p| {
        let idle = idle_p99(r, &p.role)?;
        (idle > 0.0).then(|| p.stat.quantile(0.99) as f64 / idle)
    }),
    ("stat errors", |_, _, p| Some(p.errors as f64)),
    ("storm reads/s", |_, phase, _| {
        per_s(phase.storm_reads, phase.window_ns)
    }),
    ("storm errors", |_, phase, _| {
        Some(phase.storm_errors as f64)
    }),
    ("held mean", |_, phase, _| Some(phase.held.mean)),
    ("held max", |_, phase, _| Some(phase.held.max as f64)),
    ("parked max", |_, phase, _| Some(phase.parked.max as f64)),
    ("waiting max", |_, phase, _| {
        phase.waiting.map(|w| w.max as f64)
    }),
];

/// The rows of `records`, the first `discard` runs of each label left out: (label, phase,
/// probe role, stormed) and its runs, in the order of first appearance.
fn rows(records: &[StormRecord], discard: u32) -> Vec<(Key<'_>, Row<'_>)> {
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    let mut rows: Vec<(Key<'_>, Row<'_>)> = Vec::new();
    for record in records {
        let n = seen.entry(record.label.as_str()).or_insert(0);
        *n += 1;
        if *n <= discard {
            continue;
        }
        for phase in &record.phases {
            for probe in &phase.probes {
                let key = (
                    record.label.as_str(),
                    phase.name.as_str(),
                    probe.role.as_str(),
                    probe.stormed,
                );
                match rows.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, runs)) => runs.push((record, phase, probe)),
                    None => rows.push((key, vec![(record, phase, probe)])),
                }
            }
        }
    }
    rows
}

/// The settings of each label: transport, depth, registered bytes, park, read, threads per CPU,
/// readers, runs kept, kernel and `fuse.enable_uring`, from the label's first run kept.
fn settings(out: &mut String, records: &[StormRecord], discard: u32) {
    let _ = writeln!(
        out,
        "| label | transport | depth | registered MiB | park ms | read | threads per CPU | readers | runs kept | kernel | enable_uring |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|");
    let mut labels: Vec<&str> = Vec::new();
    for record in records {
        if !labels.contains(&record.label.as_str()) {
            labels.push(&record.label);
        }
    }
    for label in labels {
        let runs: Vec<&StormRecord> = records
            .iter()
            .filter(|r| r.label == label)
            .skip(discard as usize)
            .collect();
        let Some(first) = runs.first() else {
            continue;
        };
        let opt = |v: Option<String>| v.unwrap_or_else(|| "-".to_owned());
        let _ = writeln!(
            out,
            "| {label} | {} | {} | {} | {} | {} | {} | {} | {} of {} | {} | {} |",
            first.transport,
            opt(first.uring_depth.map(|d| d.to_string())),
            opt(first
                .uring_registered_bytes
                .map(|b| crate::report::number(b as f64 / f64::from(1 << 20)))),
            crate::report::number(first.park_us as f64 / 1e3),
            first.read,
            first.per_cpu,
            first.readers,
            runs.len(),
            records.iter().filter(|r| r.label == label).count(),
            opt(first.environment.kernel.clone()),
            opt(first
                .environment
                .fuse_parameters
                .get("enable_uring")
                .cloned()),
        );
    }
}

/// The tables of `records`: the settings of each label, then a row per label, phase and probe,
/// leaving out the first `discard` runs of each label.
#[must_use]
pub fn render(records: &[StormRecord], discard: u32) -> String {
    let mut out = String::new();
    settings(&mut out, records, discard);
    let _ = writeln!(out);
    let names: Vec<&str> = COLUMNS.iter().map(|(name, _)| *name).collect();
    let _ = writeln!(
        out,
        "| label | phase | probe | stormed | {} |",
        names.join(" | ")
    );
    let _ = writeln!(out, "|---|---|---|---|{}", "---|".repeat(COLUMNS.len()));
    for ((label, phase, role, stormed), runs) in rows(records, discard) {
        let cells: Vec<String> = COLUMNS
            .iter()
            .map(|(_, figure)| {
                crate::report::cell(
                    runs.iter()
                        .filter_map(|(r, ph, p)| figure(r, ph, p))
                        .collect(),
                )
            })
            .collect();
        let stormed = if stormed { "yes" } else { "no" };
        let _ = writeln!(
            out,
            "| {label} | {phase} | {role} | {stormed} | {} |",
            cells.join(" | ")
        );
    }
    out
}

#[cfg(test)]
#[path = "stormrec_tests.rs"]
mod tests;
