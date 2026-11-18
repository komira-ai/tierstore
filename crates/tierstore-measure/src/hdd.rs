//! Arm A, raw HDD reads: request size x streams x read mode (`O_DIRECT`, buffered with
//! `read_ahead_kb` 8192, buffered with 512) x adjacent or scattered, on one drive and then on
//! both. Each stream has a region of its own (a file); it decides `pos_ms`, `media_MBps`,
//! `gap_cells` and whether the cold tier reads with `O_DIRECT` or buffered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::blockdev::{QueueSetting, Restore};
use crate::clock::Clock;
use crate::diskstats::{IoStat, Sampler};
use crate::guard::{Guard, Seen, guarded};
use crate::io::{MIB, Mode, drop_cache, write_data_file};
use crate::report::{Out, Table, num};
use crate::run::measure;
use crate::stats::{Summary, mb_per_s, summarize};
use crate::workload::{Pattern, ReadSpec, Stream, run_reads};

/// How a cell reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReadMode {
    /// `O_DIRECT`.
    Direct,
    /// Buffered, with the device's `read_ahead_kb` set to this.
    Readahead(u32),
}

impl ReadMode {
    /// The name used in tables: `direct`, `ra8192`.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Direct => "direct".into(),
            Self::Readahead(kb) => format!("ra{kb}"),
        }
    }

    /// Parses a name of [`ReadMode::name`].
    ///
    /// # Errors
    /// When it is neither `direct` nor `ra<kb>`.
    pub fn parse(s: &str) -> Result<Self, Error> {
        if s == "direct" {
            return Ok(Self::Direct);
        }
        s.strip_prefix("ra")
            .and_then(|kb| kb.parse().ok())
            .map(Self::Readahead)
            .ok_or_else(|| Error::Invalid(format!("read mode {s}: want direct or ra<kb>")))
    }
}

/// One drive of the arm: its scratch directory and its block device.
#[derive(Debug, Clone)]
pub struct Drive {
    /// The scratch directory on the drive's file system.
    pub dir: PathBuf,
    /// Its block device name (`sdb`).
    pub dev: String,
}

/// The arm's grid.
#[derive(Debug, Clone)]
pub struct Config {
    /// The drives; "one" uses the first, "both" all of them.
    pub drives: Vec<Drive>,
    /// Which drive sets to run: `one`, `both`.
    pub sets: Vec<String>,
    /// Request sizes in MiB.
    pub sizes_mib: Vec<u64>,
    /// Streams per drive.
    pub streams: Vec<usize>,
    /// Read modes.
    pub modes: Vec<ReadMode>,
    /// Patterns.
    pub patterns: Vec<Pattern>,
    /// Region files per drive (at least the largest stream count).
    pub files: usize,
    /// Size of each region file in bytes.
    pub file_size: u64,
    /// Warm-up per cell.
    pub warmup: Duration,
    /// Measured time per cell.
    pub measure: Duration,
    /// The sysfs root (`/sys`).
    pub sys: PathBuf,
}

/// The region files of one drive.
#[must_use]
pub fn region_files(dir: &Path, n: usize) -> Vec<PathBuf> {
    (0..n).map(|i| dir.join(format!("a-{i}.dat"))).collect()
}

/// Writes the region files of every drive (each drive on a thread of its own); a file that is
/// already there with the right size is kept.
///
/// # Errors
/// When a file cannot be written.
pub fn prepare(cfg: &Config) -> Result<(), Error> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = cfg
            .drives
            .iter()
            .map(|d| {
                scope.spawn(move || -> Result<(), Error> {
                    std::fs::create_dir_all(&d.dir).map_err(Error::io("mkdir", &d.dir))?;
                    for (i, p) in region_files(&d.dir, cfg.files).iter().enumerate() {
                        match std::fs::metadata(p) {
                            Ok(m) if m.len() == cfg.file_size => continue,
                            Ok(_) => std::fs::remove_file(p).map_err(Error::io("remove", p))?,
                            Err(_) => {}
                        }
                        write_data_file(p, cfg.file_size, 4 << 20, i as u64)?;
                    }
                    Ok(())
                })
            })
            .collect();
        handles
            .into_iter()
            .try_for_each(|h| crate::run::joined(h.join()))
    })
}

/// One cell's result.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Row {
    /// `one` or `both`.
    pub set: String,
    /// Request size in MiB.
    pub req_mib: u64,
    /// Streams per drive.
    pub streams: usize,
    /// Read mode name.
    pub mode: String,
    /// Pattern name.
    pub pattern: String,
    /// Aggregate MB/s.
    pub mb_s: f64,
    /// Per-request latency in ms.
    pub lat: Option<Summary>,
    /// Per-drive `iostat -x` over the window.
    pub io: BTreeMap<String, IoStat>,
    /// Foreign work the cell saw.
    pub seen: Seen,
}

fn drives_of<'a>(cfg: &'a Config, set: &str) -> Result<&'a [Drive], Error> {
    match set {
        "one" => cfg
            .drives
            .get(..1)
            .ok_or_else(|| Error::Invalid("no drive".into())),
        "both" => Ok(&cfg.drives),
        other => Err(Error::Invalid(format!(
            "drive set {other}: want one or both"
        ))),
    }
}

fn streams_for(drives: &[Drive], n: usize, files: usize, pattern: Pattern) -> Vec<Stream> {
    let mut out = Vec::new();
    for (di, d) in drives.iter().enumerate() {
        let all = region_files(&d.dir, files);
        for s in 0..n {
            out.push(Stream {
                files: match pattern {
                    Pattern::Adjacent => vec![all[s % files].clone()],
                    Pattern::Scattered => all.clone(),
                },
                pattern,
                seed: (di * 1000 + s) as u64 + 1,
            });
        }
    }
    out
}

/// Runs one cell.
///
/// # Errors
/// When a file cannot be read, a setting cannot be changed, or the counters cannot be read.
#[expect(
    clippy::too_many_arguments,
    reason = "the coordinates of one cell of the grid"
)]
pub fn cell(
    cfg: &Config,
    clock: &dyn Clock,
    set: &str,
    req_mib: u64,
    streams: usize,
    mode: ReadMode,
    pattern: Pattern,
    diskstats: &Path,
) -> Result<Row, Error> {
    let drives = drives_of(cfg, set)?;
    let devs: Vec<String> = drives.iter().map(|d| d.dev.clone()).collect();
    let mut guards: Vec<Restore> = Vec::new();
    let io_mode = match mode {
        ReadMode::Direct => Mode::Direct,
        ReadMode::Readahead(kb) => {
            for d in &devs {
                guards.push(QueueSetting::new(&cfg.sys, d, "read_ahead_kb").set(&kb.to_string())?);
            }
            Mode::Buffered
        }
    };
    let all: Vec<PathBuf> = drives
        .iter()
        .flat_map(|d| region_files(&d.dir, cfg.files))
        .collect();
    drop_cache(&all)?;
    let spec = ReadSpec {
        streams: streams_for(drives, streams, cfg.files, pattern),
        req: usize::try_from(req_mib * MIB).unwrap_or(usize::MAX),
        mode: io_mode,
    };
    let sampler = Sampler::new(diskstats, &devs);
    let (r, io) = measure(clock, Some(&sampler), cfg.warmup, cfg.measure, |span| {
        run_reads(&spec, clock, span)
    })?;
    for g in guards {
        g.restore()?;
    }
    Ok(Row {
        set: set.to_string(),
        req_mib,
        streams,
        mode: mode.name(),
        pattern: pattern.name().to_string(),
        mb_s: mb_per_s(r.bytes, r.secs),
        lat: summarize(&r.lat_ms),
        io,
        seen: Seen::default(),
    })
}

/// The constants arm A decides, from its rows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Derived {
    /// One stream, adjacent, `O_DIRECT`: the best MB/s of one drive (`media_MBps`).
    pub media_mb_s: f64,
    /// One stream, scattered, `O_DIRECT`, 1 MiB: median latency less the transfer time of 1 MiB
    /// at the media rate (`pos_ms`).
    pub pos_ms: f64,
    /// `ceil(pos_ms x media_MBps / 1.048576 / 1000)`: cells a gap may hold and still be read
    /// through (`gap_cells`).
    pub gap_cells: u64,
}

/// Derives the constants, or `None` when the rows lack the cells they come from.
#[must_use]
pub fn derive(rows: &[Row]) -> Option<Derived> {
    let one = |r: &&Row| r.set == "one" && r.streams == 1 && r.mode == "direct";
    let media = rows
        .iter()
        .filter(one)
        .filter(|r| r.pattern == "adjacent")
        .map(|r| r.mb_s)
        .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))?;
    let scattered = rows
        .iter()
        .filter(one)
        .find(|r| r.pattern == "scattered" && r.req_mib == 1)?;
    let p50 = scattered.lat?.p50;
    let pos = (p50 - 1.048_576 / media * 1000.0).max(0.0);
    let gap = (pos * media / 1.048_576 / 1000.0).ceil() as u64;
    Some(Derived {
        media_mb_s: media,
        pos_ms: pos,
        gap_cells: gap,
    })
}

/// The arm's Markdown tables: one per drive set, and the derived constants.
#[must_use]
pub fn tables(rows: &[Row]) -> Vec<Table> {
    let mut out = Vec::new();
    for set in ["one", "both"] {
        let mut t = Table::new(
            &format!("A, raw HDD reads, drives: {set}"),
            &[
                "req MiB",
                "streams/drive",
                "mode",
                "pattern",
                "MB/s",
                "p50 ms",
                "p99 ms",
                "rareq-sz KiB",
                "r_await ms",
                "aqu-sz",
                "util %",
                "foreign MB/s",
                "load",
            ],
        );
        for r in rows.iter().filter(|r| r.set == set) {
            let io: Vec<&IoStat> = r.io.values().collect();
            let avg = |f: fn(&IoStat) -> f64| {
                if io.is_empty() {
                    0.0
                } else {
                    io.iter().map(|s| f(s)).fold(0.0, |a, b| a + b) / io.len() as f64
                }
            };
            t.row(vec![
                r.req_mib.to_string(),
                r.streams.to_string(),
                r.mode.clone(),
                r.pattern.clone(),
                num(r.mb_s, 1),
                r.lat.map_or("-".into(), |l| num(l.p50, 1)),
                r.lat.map_or("-".into(), |l| num(l.p99, 1)),
                num(avg(|s| s.rareq_kb), 0),
                num(avg(|s| s.r_await), 1),
                num(avg(|s| s.aqu_sz), 2),
                num(avg(|s| s.util), 1),
                num(r.seen.foreign_mb_s, 1),
                num(r.seen.load1, 0),
            ]);
        }
        if !t.rows().is_empty() {
            out.push(t);
        }
    }
    if let Some(d) = derive(rows) {
        let mut t = Table::new("A, derived", &["media MB/s", "pos ms", "gap cells"]);
        t.row(vec![
            num(d.media_mb_s, 1),
            num(d.pos_ms, 1),
            d.gap_cells.to_string(),
        ]);
        out.push(t);
    }
    out
}

/// Runs the whole grid, recording each cell as it ends, and writes the tables.
///
/// # Errors
/// When a cell fails.
pub fn run(
    cfg: &Config,
    clock: &dyn Clock,
    diskstats: &Path,
    out: &Out,
    guard: Option<&Guard>,
) -> Result<Vec<Row>, Error> {
    // A run that stopped is resumed: the cells already in the output are kept, not run again.
    let done: Vec<Row> = std::fs::read_to_string(out.path("jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let mut rows = Vec::new();
    for set in &cfg.sets {
        for &size in &cfg.sizes_mib {
            for &streams in &cfg.streams {
                for &mode in &cfg.modes {
                    for &pattern in &cfg.patterns {
                        let same = |r: &&Row| {
                            (
                                r.set.as_str(),
                                r.req_mib,
                                r.streams,
                                r.mode.as_str(),
                                r.pattern.as_str(),
                            ) == (
                                set.as_str(),
                                size,
                                streams,
                                mode.name().as_str(),
                                pattern.name(),
                            )
                        };
                        if let Some(r) = done.iter().find(same) {
                            rows.push(r.clone());
                            continue;
                        }
                        let (mut r, seen) = guarded(guard, clock, || {
                            cell(cfg, clock, set, size, streams, mode, pattern, diskstats)
                        })?;
                        r.seen = seen;
                        out.record(&r)?;
                        rows.push(r);
                    }
                }
            }
        }
    }
    out.tables(&tables(&rows))?;
    if let Some(d) = derive(&rows) {
        out.record(&serde_json::json!({ "derived": d }))?;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
