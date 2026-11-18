//! Arm C-5, pack writes on an HDD as the cold tier would write them: each pack a new file
//! written sequentially with `O_DIRECT` 4 MiB writes at depth 2 under a temporary name, then
//! `fsync`, a rename to its final name and an `fsync` of the directory. Cells: pack size x
//! writers per drive x the 64 MiB xfs extent-size hint x `fallocate` of the whole length; the
//! extent count of every file. Then the sustained curve (a window per 10 GiB over 200 GiB) and
//! the same after deleting every third file. Decides the extent hint, `fill_limit` (with the
//! churn arm) and the per-epoch-directory fallback.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::diskstats::{IoStat, Sampler};
use crate::io::{AlignedBuf, MIB, Mode, create, fill, fs_space, preallocate, write_full_at};
use crate::report::{Table, num};
use crate::run::{Runner, measure};
use crate::stats::{Summary, mb_per_s, summarize};

/// How a pack is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct How {
    /// Bytes per write.
    pub chunk: usize,
    /// Writes in flight.
    pub depth: usize,
    /// Whether the whole length is allocated first.
    pub prealloc: bool,
}

/// The cold tier's default: 4 MiB writes, depth 2, the length allocated first.
pub const DEFAULT: How = How {
    chunk: 4 << 20,
    depth: 2,
    prealloc: true,
};

/// Writes one pack of `size` bytes (a multiple of 4 KiB) as `dir/name.pack`; returns the
/// seconds it took, the commit included.
///
/// # Errors
/// When a step fails or the size is not aligned.
pub fn write_pack(
    dir: &Path,
    name: &str,
    size: u64,
    how: How,
    seed: u64,
    clock: &dyn Clock,
) -> Result<f64, Error> {
    if !size.is_multiple_of(4096)
        || !how.chunk.is_multiple_of(4096)
        || how.chunk == 0
        || how.depth == 0
    {
        return Err(Error::Invalid(format!(
            "pack size {size} and chunk {} must be multiples of 4096, depth at least 1",
            how.chunk
        )));
    }
    let t0 = clock.now();
    let up = dir.join(format!(".{name}.up"));
    let f = create(&up, Mode::Direct)?;
    if how.prealloc && size > 0 {
        preallocate(&f, size, &up)?;
    }
    let chunks = size.div_ceil(how.chunk as u64);
    std::thread::scope(|scope| {
        let hs: Vec<_> = (0..how.depth as u64)
            .map(|k| {
                let (f, up) = (&f, &up);
                scope.spawn(move || -> Result<(), Error> {
                    let mut buf = AlignedBuf::new(how.chunk);
                    let mut i = k;
                    while i < chunks {
                        let off = i * how.chunk as u64;
                        let n = usize::try_from((size - off).min(how.chunk as u64)).unwrap_or(0);
                        fill(&mut buf.as_mut_slice()[..n], seed ^ off);
                        write_full_at(f, &buf.as_slice()[..n], off)
                            .map_err(Error::io("write", up))?;
                        i += how.depth as u64;
                    }
                    Ok(())
                })
            })
            .collect();
        hs.into_iter().try_for_each(|h| {
            h.join()
                .unwrap_or_else(|_| Err(Error::Invalid("a writer panicked".into())))
        })
    })?;
    f.sync_all().map_err(Error::io("fsync", &up))?;
    let fin = dir.join(format!("{name}.pack"));
    std::fs::rename(&up, &fin).map_err(Error::io("rename", &up))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(Error::io("fsync", dir))?;
    Ok((clock.now() - t0).as_secs_f64())
}

/// The number of data extents of `path`, from `xfs_io -r -c fiemap` (holes not counted).
///
/// # Errors
/// When `xfs_io` fails.
pub fn extents(run: &dyn Runner, path: &Path) -> Result<usize, Error> {
    let out = run.run(&["xfs_io", "-r", "-c", "fiemap", &path.to_string_lossy()])?;
    Ok(out
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.split_once(": [")
                .is_some_and(|(n, _)| n.parse::<u64>().is_ok())
                && !t.ends_with("hole")
        })
        .count())
}

/// Sets the xfs extent-size hint of `dir` (inherited by files made in it) to `mib` MiB.
///
/// # Errors
/// When `xfs_io` fails.
pub fn set_extsize(run: &dyn Runner, dir: &Path, mib: u64) -> Result<(), Error> {
    run.run(&[
        "xfs_io",
        "-c",
        &format!("extsize {mib}m"),
        &dir.to_string_lossy(),
    ])
    .map(|_| ())
}

/// The arm's settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// The scratch directory on the drive.
    pub dir: PathBuf,
    /// The drive's block device.
    pub dev: String,
    /// Pack sizes in MiB.
    pub sizes_mib: Vec<u64>,
    /// Writers per drive.
    pub writers: Vec<usize>,
    /// Measured time per cell (each writer writes packs back to back until it ends).
    pub measure: Duration,
    /// Bytes of the sustained phase.
    pub sustained: u64,
    /// Bytes per pack of the sustained and rewrite phases (1 GiB in a run).
    pub pack: u64,
    /// Bytes per window of the sustained curve.
    pub window: u64,
    /// Cap of the rewrite phase after the deletes (the plan: 400 GiB).
    pub rewrite_cap: u64,
    /// The free fraction of the drive's file system that must remain after the arm's largest
    /// use, else the arm refuses to start (it never fills a drive silo uses).
    pub keep_free: f64,
    /// `/proc/diskstats`.
    pub diskstats: PathBuf,
}

/// One cell of pack writes.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// `cells`, `sustained`, `rewrite`.
    pub phase: String,
    /// The cell's parameters as `name=value` words.
    pub params: String,
    /// Aggregate MB/s.
    pub mb_s: f64,
    /// Seconds per pack.
    pub pack_secs: Option<Summary>,
    /// Extents per file.
    pub extents: Option<Summary>,
    /// The drive's `iostat -x`.
    pub io: Option<IoStat>,
}

fn rm_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// The cells: size x writers x extent hint x `fallocate`.
///
/// # Errors
/// When a write fails.
pub fn cells(cfg: &Config, run: &dyn Runner, clock: &dyn Clock) -> Result<Vec<Row>, Error> {
    let sampler = Sampler::new(&cfg.diskstats, std::slice::from_ref(&cfg.dev));
    let mut rows = Vec::new();
    for &mib in &cfg.sizes_mib {
        for &writers in &cfg.writers {
            for hint in [false, true] {
                for prealloc in [false, true] {
                    let dir = cfg.dir.join("c5-cell");
                    rm_dir(&dir);
                    std::fs::create_dir_all(&dir).map_err(Error::io("mkdir", &dir))?;
                    if hint {
                        set_extsize(run, &dir, 64)?;
                    }
                    let how = How {
                        prealloc,
                        ..DEFAULT
                    };
                    let ((bytes, secs, names), io) = measure(
                        clock,
                        Some(&sampler),
                        Duration::ZERO,
                        cfg.measure,
                        |span| {
                            std::thread::scope(|scope| {
                                let hs: Vec<_> = (0..writers)
                                    .map(|w| {
                                        let dir = &dir;
                                        scope.spawn(move || -> Result<(u64, Vec<f64>, Vec<String>), Error> {
                                            let (mut b, mut s, mut n) = (0u64, Vec::new(), Vec::new());
                                            let mut k = 0;
                                            while clock.now() < span.end {
                                                let name = format!("w{w}-{k}");
                                                s.push(write_pack(dir, &name, mib * MIB, how, k, clock)?);
                                                b += mib * MIB;
                                                n.push(name);
                                                k += 1;
                                            }
                                            Ok((b, s, n))
                                        })
                                    })
                                    .collect();
                                let (mut b, mut s, mut n) = (0, Vec::new(), Vec::new());
                                for h in hs {
                                    let (b1, s1, n1) = h.join().unwrap_or_else(|_| {
                                        Err(Error::Invalid("a writer panicked".into()))
                                    })?;
                                    b += b1;
                                    s.extend(s1);
                                    n.extend(n1);
                                }
                                Ok((b, s, n))
                            })
                        },
                    )?;
                    let mut ext = Vec::new();
                    for n in &names {
                        ext.push(extents(run, &dir.join(format!("{n}.pack")))? as f64);
                    }
                    let total: f64 = secs.iter().fold(0.0, |a, b| a + b) / writers as f64;
                    rows.push(Row {
                        phase: "cells".into(),
                        params: format!(
                            "size={mib}MiB writers={writers} extsize64m={hint} fallocate={prealloc}"
                        ),
                        mb_s: mb_per_s(bytes, total),
                        pack_secs: summarize(&secs),
                        extents: summarize(&ext),
                        io: io.get(&cfg.dev).copied(),
                    });
                    rm_dir(&dir);
                }
            }
        }
    }
    Ok(rows)
}

/// Writes packs of `size` bytes into `dir` until `total` bytes, a row per `window` bytes;
/// returns the rows and the packs' names in order.
#[expect(
    clippy::too_many_arguments,
    reason = "a phase of the arm and where it writes"
)]
fn curve(
    cfg: &Config,
    phase: &str,
    dir: &Path,
    first: usize,
    size: u64,
    total: u64,
    run: &dyn Runner,
    clock: &dyn Clock,
) -> Result<(Vec<Row>, Vec<String>), Error> {
    let sampler = Sampler::new(&cfg.diskstats, std::slice::from_ref(&cfg.dev));
    let (mut rows, mut names) = (Vec::new(), Vec::new());
    let mut written = 0;
    let mut k = first;
    while written < total {
        let a = sampler.snapshot()?;
        let t0 = clock.now();
        let (mut wb, mut secs, mut ext) = (0, Vec::new(), Vec::new());
        while wb < cfg.window && written + wb < total {
            let name = format!("p{k}");
            secs.push(write_pack(dir, &name, size, DEFAULT, k as u64, clock)?);
            ext.push(extents(run, &dir.join(format!("{name}.pack")))? as f64);
            names.push(name);
            wb += size;
            k += 1;
        }
        let el = (clock.now() - t0).as_secs_f64();
        let b = sampler.snapshot()?;
        written += wb;
        rows.push(Row {
            phase: phase.into(),
            params: format!("written={}GiB", written >> 30),
            mb_s: mb_per_s(wb, el),
            pack_secs: summarize(&secs),
            extents: summarize(&ext),
            io: crate::diskstats::delta(&a, &b, el).get(&cfg.dev).copied(),
        });
    }
    Ok((rows, names))
}

/// The sustained curve, then every third pack deleted and as much written again (up to the
/// smaller of 60% of the free space and `rewrite_cap`), then everything removed.
///
/// # Errors
/// When the drive would keep less than `keep_free` free, or a step fails.
pub fn sustained(cfg: &Config, run: &dyn Runner, clock: &dyn Clock) -> Result<Vec<Row>, Error> {
    let (total, free) = fs_space(&cfg.dir)?;
    let rewrite = cfg
        .rewrite_cap
        .min(free.saturating_sub(cfg.sustained) * 6 / 10);
    let after = free.saturating_sub(cfg.sustained + rewrite);
    if (after as f64) < cfg.keep_free * total as f64 {
        return Err(Error::Invalid(format!(
            "the arm would leave {} GiB of {} GiB free, under the {}% it must keep",
            after >> 30,
            total >> 30,
            cfg.keep_free * 100.0
        )));
    }
    let dir = cfg.dir.join("c5-sustained");
    rm_dir(&dir);
    std::fs::create_dir_all(&dir).map_err(Error::io("mkdir", &dir))?;
    set_extsize(run, &dir, 64)?;
    let size = cfg.pack;
    let (mut rows, names) = curve(cfg, "sustained", &dir, 0, size, cfg.sustained, run, clock)?;
    for n in names.iter().step_by(3) {
        let p = dir.join(format!("{n}.pack"));
        std::fs::remove_file(&p).map_err(Error::io("remove", &p))?;
    }
    let (more, _) = curve(cfg, "rewrite", &dir, names.len(), size, rewrite, run, clock)?;
    rows.extend(more);
    rm_dir(&dir);
    Ok(rows)
}

/// The rewrite phase's mean MB/s over the sustained phase's: under 0.5 calls for the
/// per-epoch-directory fallback.
#[must_use]
pub fn ratio(rows: &[Row]) -> Option<f64> {
    let mean = |phase: &str| {
        let v: Vec<f64> = rows
            .iter()
            .filter(|r| r.phase == phase)
            .map(|r| r.mb_s)
            .collect();
        (!v.is_empty()).then(|| v.iter().fold(0.0, |a, b| a + b) / v.len() as f64)
    };
    Some(mean("rewrite")? / mean("sustained")?)
}

/// The arm's tables.
#[must_use]
pub fn tables(rows: &[Row]) -> Vec<Table> {
    let mut out = Vec::new();
    for phase in ["cells", "sustained", "rewrite"] {
        let mut t = Table::new(
            &format!("C-5, pack writes: {phase}"),
            &[
                "params",
                "MB/s",
                "pack s p50",
                "pack s max",
                "extents p50",
                "extents max",
                "wareq-sz KiB",
                "util %",
            ],
        );
        for r in rows.iter().filter(|r| r.phase == phase) {
            t.row(vec![
                r.params.clone(),
                num(r.mb_s, 1),
                r.pack_secs.map_or("-".into(), |s| num(s.p50, 2)),
                r.pack_secs.map_or("-".into(), |s| num(s.max, 2)),
                r.extents.map_or("-".into(), |s| num(s.p50, 0)),
                r.extents.map_or("-".into(), |s| num(s.max, 0)),
                r.io.map_or("-".into(), |s| num(s.wareq_kb, 0)),
                r.io.map_or("-".into(), |s| num(s.util, 1)),
            ]);
        }
        if !t.rows().is_empty() {
            out.push(t);
        }
    }
    if let Some(x) = ratio(rows) {
        let mut t = Table::new("C-5, rewrite after deletes over fresh", &["ratio"]);
        t.row(vec![num(x, 2)]);
        out.push(t);
    }
    out
}

#[cfg(test)]
mod tests;
