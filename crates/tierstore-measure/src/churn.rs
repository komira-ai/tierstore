//! Arm C-5, steady-state churn: a dedicated xfs file system on the drive (a loop over an image,
//! so the drive's own file system keeps its headroom) is filled with packs to a band of fill
//! (say 84-86% or 89-91%) and then kept there for several capacities of writes: one writer adds
//! packs as the cold tier writes them, a light reader reads packs (most often the newest) and
//! stamps them, and whenever the fill passes the band's top the least recently read packs are
//! unlinked until it is under the band's bottom, as the tier's eviction does. A window per
//! `window` bytes written gives write MB/s, read MB/s, the fill and the evictions; the first
//! (filling) phase is the fresh-drive rate the churn is compared with.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::diskstats::{IoStat, Sampler, delta};
use crate::io::{AlignedBuf, MIB, Mode, fs_space, open_read, read_full_at};
use crate::report::{Table, num};
use crate::run::Runner;
use crate::stats::mb_per_s;
use crate::workload::Rng;
use crate::write::{DEFAULT, extents, set_extsize, write_pack};

/// The arm's settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// The mount point of the dedicated file system.
    pub dir: PathBuf,
    /// The block devices to sample (the loop and the drive under it).
    pub devs: Vec<String>,
    /// Fill (percent of the file system) under which eviction stops.
    pub lo: f64,
    /// Fill above which eviction starts.
    pub hi: f64,
    /// Pack sizes in MiB, drawn in turn by a seeded generator.
    pub sizes_mib: Vec<u64>,
    /// Bytes to write after the fill phase, as a multiple of the file system's size.
    pub turns: f64,
    /// Bytes per window.
    pub window: u64,
    /// The reader reads one 4 MiB piece every this long.
    pub read_every: Duration,
    /// `/proc/diskstats`.
    pub diskstats: PathBuf,
}

/// One window.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// `fill` or `churn`.
    pub phase: String,
    /// Bytes written so far, in GiB.
    pub written_gib: f64,
    /// Write MB/s over the window.
    pub write_mb_s: f64,
    /// Read MB/s over the window.
    pub read_mb_s: f64,
    /// Fill at the window's end, percent.
    pub fill: f64,
    /// Packs unlinked in the window.
    pub evicted: u64,
    /// Extents of the window's last pack.
    pub extents: usize,
    /// The devices' `iostat -x`.
    pub io: BTreeMap<String, IoStat>,
}

#[derive(Debug)]
struct Pack {
    name: String,
    bytes: u64,
    last_read: u64,
}

/// The packs on the file system, in write order, with their last-read stamps.
#[derive(Debug, Default)]
struct Store {
    packs: Vec<Pack>,
    tick: u64,
}

impl Store {
    fn add(&mut self, name: String, bytes: u64) {
        self.tick += 1;
        self.packs.push(Pack {
            name,
            bytes,
            last_read: self.tick,
        });
    }

    /// A pack to read: the newest with probability 1/2, the one before with 1/4, and so on.
    fn pick(&mut self, rng: &mut Rng) -> Option<(String, u64)> {
        let n = self.packs.len();
        if n == 0 {
            return None;
        }
        let back = usize::try_from(rng.next_u64().trailing_ones())
            .unwrap_or(0)
            .min(n - 1);
        self.tick += 1;
        let p = &mut self.packs[n - 1 - back];
        p.last_read = self.tick;
        Some((p.name.clone(), p.bytes))
    }

    /// Removes and returns the least recently read pack.
    fn evict(&mut self) -> Option<Pack> {
        let i = self
            .packs
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.last_read)
            .map(|(i, _)| i)?;
        Some(self.packs.remove(i))
    }
}

/// How full a file system is.
pub trait Space: Sync {
    /// Its size and the bytes in use, of the file system at `dir`.
    ///
    /// # Errors
    /// When it cannot be read.
    fn total_used(&self, dir: &Path) -> Result<(u64, u64), Error>;
}

/// The file system's own count (`statvfs`).
#[derive(Debug, Default, Clone, Copy)]
pub struct Statvfs;

impl Space for Statvfs {
    fn total_used(&self, dir: &Path) -> Result<(u64, u64), Error> {
        let (total, free) = fs_space(dir)?;
        Ok((total, total.saturating_sub(free)))
    }
}

/// The used fraction of the file system at `dir`, in percent.
fn fill(space: &dyn Space, dir: &Path) -> Result<f64, Error> {
    let (total, used) = space.total_used(dir)?;
    Ok(used as f64 * 100.0 / total.max(1) as f64)
}

fn subdir(dir: &Path, k: u64) -> PathBuf {
    dir.join(format!("{:02x}", k % 256))
}

/// Runs the arm.
///
/// # Errors
/// When a write, a read, an unlink or a sample fails, or the band is empty.
pub fn run(
    cfg: &Config,
    space: &dyn Space,
    runner: &dyn Runner,
    clock: &dyn Clock,
) -> Result<Vec<Row>, Error> {
    if !(cfg.lo < cfg.hi && cfg.hi < 100.0) || cfg.sizes_mib.is_empty() || cfg.window == 0 {
        return Err(Error::Invalid(format!(
            "band {}..{} must rise and stay under 100, with pack sizes and a window",
            cfg.lo, cfg.hi
        )));
    }
    for k in 0..256 {
        let d = subdir(&cfg.dir, k);
        std::fs::create_dir_all(&d).map_err(Error::io("mkdir", &d))?;
        set_extsize(runner, &d, 64)?;
    }
    let size = space.total_used(&cfg.dir)?.0;
    let store = Mutex::new(Store::default());
    let stop = AtomicBool::new(false);
    let read_bytes = AtomicU64::new(0);
    let sampler = Sampler::new(&cfg.diskstats, &cfg.devs);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| reader(cfg, &store, &stop, &read_bytes, clock));
        let r = writer(
            cfg,
            space,
            runner,
            clock,
            &sampler,
            &store,
            &read_bytes,
            size,
        );
        stop.store(true, Ordering::SeqCst);
        let rd = crate::run::joined(reader.join());
        let rows = r?;
        rd?;
        Ok(rows)
    })
}

fn reader(
    cfg: &Config,
    store: &Mutex<Store>,
    stop: &AtomicBool,
    read_bytes: &AtomicU64,
    clock: &dyn Clock,
) -> Result<(), Error> {
    let mut rng = Rng::new(99);
    let mut buf = AlignedBuf::new(4 << 20);
    while !stop.load(Ordering::SeqCst) {
        clock.sleep(cfg.read_every);
        let pick = store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pick(&mut rng);
        let Some((name, bytes)) = pick else { continue };
        let p = cfg.dir.join(&name);
        // A pack evicted between the pick and the open is skipped, as the tier would miss it.
        let Ok(f) = open_read(&p, Mode::Direct) else {
            continue;
        };
        let off = rng.below((bytes / (4 << 20)).max(1)) * (4 << 20);
        let n = read_full_at(&f, buf.as_mut_slice(), off).map_err(Error::io("read", &p))?;
        read_bytes.fetch_add(n as u64, Ordering::SeqCst);
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the writer's view of the arm's shared state"
)]
fn writer(
    cfg: &Config,
    space: &dyn Space,
    runner: &dyn Runner,
    clock: &dyn Clock,
    sampler: &Sampler,
    store: &Mutex<Store>,
    read_bytes: &AtomicU64,
    size: u64,
) -> Result<Vec<Row>, Error> {
    let mut rng = Rng::new(7);
    let mut rows = Vec::new();
    let (mut written, mut churned, mut k) = (0u64, 0u64, 0u64);
    let target = (cfg.turns * size as f64) as u64;
    let mut filling = true;
    while filling || churned < target {
        let was_filling = filling;
        let a = sampler.snapshot()?;
        let t0 = clock.now();
        let r0 = read_bytes.load(Ordering::SeqCst);
        let (mut wb, mut evicted, mut last) = (0u64, 0u64, String::new());
        while wb < cfg.window {
            let mib =
                cfg.sizes_mib[usize::try_from(rng.below(cfg.sizes_mib.len() as u64)).unwrap_or(0)];
            let dir = subdir(&cfg.dir, k);
            let name = format!("p{k}");
            write_pack(&dir, &name, mib * MIB, DEFAULT, k, clock)?;
            let rel = format!("{:02x}/{name}.pack", k % 256);
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .add(rel.clone(), mib * MIB);
            last = rel;
            wb += mib * MIB;
            k += 1;
            if fill(space, &cfg.dir)? > cfg.hi {
                filling = false;
                while fill(space, &cfg.dir)? > cfg.lo {
                    let gone = store
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .evict();
                    let Some(p) = gone else { break };
                    let path = cfg.dir.join(&p.name);
                    std::fs::remove_file(&path).map_err(Error::io("remove", &path))?;
                    evicted += 1;
                }
            }
        }
        let el = (clock.now() - t0).as_secs_f64();
        let b = sampler.snapshot()?;
        written += wb;
        let phase = if was_filling { "fill" } else { "churn" };
        if !was_filling {
            churned += wb;
        }
        // A window writes at least one pack (the window is not empty), so `last` names one.
        let ext = extents(runner, &cfg.dir.join(&last))?;
        rows.push(Row {
            phase: phase.into(),
            written_gib: written as f64 / (1u64 << 30) as f64,
            write_mb_s: mb_per_s(wb, el),
            read_mb_s: mb_per_s(read_bytes.load(Ordering::SeqCst) - r0, el),
            fill: fill(space, &cfg.dir)?,
            evicted,
            extents: ext,
            io: delta(&a, &b, el),
        });
    }
    Ok(rows)
}

/// Mean write MB/s of the fill phase and of each turn of churn (a turn = `size` bytes).
#[must_use]
pub fn summary(rows: &[Row], window_gib: f64, turn_gib: f64) -> Vec<(String, f64)> {
    let mean = |v: &[f64]| v.iter().fold(0.0, |a, b| a + b) / v.len() as f64;
    let mut out = Vec::new();
    let fill: Vec<f64> = rows
        .iter()
        .filter(|r| r.phase == "fill")
        .map(|r| r.write_mb_s)
        .collect();
    if !fill.is_empty() {
        out.push(("fill (fresh)".to_string(), mean(&fill)));
    }
    let churn: Vec<&Row> = rows.iter().filter(|r| r.phase == "churn").collect();
    let per_turn = ((turn_gib / window_gib).ceil() as usize).max(1);
    for (i, c) in churn.chunks(per_turn).enumerate() {
        let v: Vec<f64> = c.iter().map(|r| r.write_mb_s).collect();
        out.push((format!("churn turn {}", i + 1), mean(&v)));
    }
    out
}

/// The arm's tables: the windows and the per-turn summary.
#[must_use]
pub fn tables(label: &str, rows: &[Row], window_gib: f64, turn_gib: f64) -> Vec<Table> {
    let mut t = Table::new(
        &format!("C-5, steady-state churn: {label}"),
        &[
            "phase",
            "written GiB",
            "write MB/s",
            "read MB/s",
            "fill %",
            "evicted",
            "extents",
            "util % (per device)",
        ],
    );
    for r in rows {
        let util: Vec<String> =
            r.io.iter()
                .map(|(k, v)| format!("{k}:{}", num(v.util, 0)))
                .collect();
        t.row(vec![
            r.phase.clone(),
            num(r.written_gib, 1),
            num(r.write_mb_s, 1),
            num(r.read_mb_s, 1),
            num(r.fill, 1),
            r.evicted.to_string(),
            r.extents.to_string(),
            util.join(" "),
        ]);
    }
    let mut s = Table::new(
        &format!("C-5, churn summary: {label}"),
        &["phase", "write MB/s", "over fresh"],
    );
    let sum = summary(rows, window_gib, turn_gib);
    let fresh = sum
        .first()
        .filter(|(n, _)| n.starts_with("fill"))
        .map(|(_, v)| *v);
    for (name, v) in &sum {
        s.row(vec![
            name.clone(),
            num(*v, 1),
            fresh.map_or("-".into(), |f| num(v / f, 2)),
        ]);
    }
    vec![t, s]
}

#[cfg(test)]
mod tests;
