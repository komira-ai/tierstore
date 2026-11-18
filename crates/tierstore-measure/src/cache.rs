//! Arm B, the cache device, run on each target directory: the loop device the cache uses
//! today, a directory of the host's root file system, and a loop device with `direct-io=on`.
//!
//! Tests: sequential writes (block x jobs x buffered with `fdatasync` per 32 MiB or `O_DIRECT`);
//! `fdatasync` latency of 1 MiB per entry file committed every second, idle and beside writers;
//! a mixed load of 70% 128 KiB random reads and 30% 1 MiB sequential writes; hole punching of
//! 16 MiB groups and where the space goes; and the `POSIX_FADV_DONTNEED` policies on a
//! read-after-serve pattern. It decides the cache-device gate, the dirty cap, and the DONTNEED
//! default.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;
use crate::diskstats::{IoStat, Sampler};
use crate::guard::{Guard, Seen, guarded};
use crate::io::{
    AlignedBuf, MIB, Mode, allocated, create, drop_pages, fill, fs_space, open_read, punch,
    read_full_at, write_data_file, write_full_at,
};
use crate::report::{Table, num};
use crate::run::{Runner, Span, measure};
use crate::stats::{Summary, mb_per_s, summarize};
use crate::workload::Rng;

/// One target of the arm.
#[derive(Debug, Clone)]
pub struct Target {
    /// Its label in tables: `loop`, `rootfs`, `loop-dio`.
    pub label: String,
    /// The scratch directory on it.
    pub dir: PathBuf,
    /// The block devices to sample (the loop and the disk under it).
    pub devs: Vec<String>,
}

/// How a write test writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteMode {
    /// Buffered, with an `fdatasync` after every 32 MiB.
    BufferedSync,
    /// `O_DIRECT`.
    Direct,
}

impl WriteMode {
    /// The name used in tables.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::BufferedSync => "buffered+fdatasync/32MiB",
            Self::Direct => "direct",
        }
    }
}

/// A row of any of the arm's tests.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    /// The target's label.
    pub target: String,
    /// `write`, `fdatasync`, `mixed`, `punch`, `dontneed`.
    pub test: String,
    /// The test's parameters, as `name=value` words.
    pub params: String,
    /// MB/s of the test's main stream (writes, or reads for `dontneed`).
    pub mb_s: f64,
    /// Latency of the test's operations in ms (writes, `fdatasync`s, or reads).
    pub lat: Option<Summary>,
    /// Further numbers, by name.
    pub extra: BTreeMap<String, f64>,
    /// Per-device `iostat -x` over the window.
    pub io: BTreeMap<String, IoStat>,
    /// Foreign work the test saw.
    pub seen: Seen,
}

const SYNC_EVERY: u64 = 32 * MIB;

/// A sequential writer of one new file until `span` ends; returns the bytes and latencies of
/// the writes inside the span.
fn writer(
    path: &Path,
    block: usize,
    mode: WriteMode,
    clock: &dyn Clock,
    span: Span,
) -> Result<(u64, Vec<f64>), Error> {
    let io_mode = match mode {
        WriteMode::Direct => Mode::Direct,
        WriteMode::BufferedSync => Mode::Buffered,
    };
    let f = create(path, io_mode)?;
    let mut buf = AlignedBuf::new(block);
    fill(buf.as_mut_slice(), block as u64);
    let (mut off, mut bytes, mut lat) = (0u64, 0u64, Vec::new());
    loop {
        let t0 = clock.now();
        if t0 >= span.end {
            break;
        }
        write_full_at(&f, buf.as_slice(), off).map_err(Error::io("write", path))?;
        off += block as u64;
        if mode == WriteMode::BufferedSync && off % SYNC_EVERY == 0 {
            rustix::fs::fdatasync(&f).map_err(|e| Error::io("fdatasync", path)(e.into()))?;
        }
        let t1 = clock.now();
        if span.holds(t0, t1) {
            bytes += block as u64;
            lat.push((t1 - t0).as_secs_f64() * 1000.0);
        }
    }
    Ok((bytes, lat))
}

fn remove_all(paths: &[PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

/// `jobs` concurrent writers of `block`-byte writes; the files are removed afterwards.
fn writers(
    dir: &Path,
    tag: &str,
    jobs: usize,
    block: usize,
    mode: WriteMode,
    clock: &dyn Clock,
    span: Span,
) -> Result<(u64, Vec<f64>), Error> {
    let paths: Vec<PathBuf> = (0..jobs)
        .map(|j| dir.join(format!("b-{tag}-{j}")))
        .collect();
    remove_all(&paths);
    let r = std::thread::scope(|scope| {
        let hs: Vec<_> = paths
            .iter()
            .map(|p| scope.spawn(move || writer(p, block, mode, clock, span)))
            .collect();
        let mut bytes = 0;
        let mut lat = Vec::new();
        for h in hs {
            let (b, l) = crate::run::joined(h.join())?;
            bytes += b;
            lat.extend(l);
        }
        Ok((bytes, lat))
    });
    remove_all(&paths);
    r
}

/// The arm's settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// The targets, run one after another.
    pub targets: Vec<Target>,
    /// Write block sizes in KiB.
    pub blocks_kib: Vec<usize>,
    /// Concurrent writers.
    pub jobs: Vec<usize>,
    /// Write modes.
    pub modes: Vec<WriteMode>,
    /// Warm-up per cell.
    pub warmup: Duration,
    /// Measured time per cell.
    pub measure: Duration,
    /// Entry files of the `fdatasync` test.
    pub sync_files: usize,
    /// Commits of the `fdatasync` test (one a second).
    pub sync_commits: usize,
    /// Background writers beside the loaded `fdatasync` test.
    pub sync_background: usize,
    /// Bytes of the read set of the mixed test (split in four files).
    pub mixed_read_set: u64,
    /// Bytes of the file of the punch test (a multiple of 32 MiB).
    pub punch_bytes: u64,
    /// Bytes written by each `DONTNEED` policy run.
    pub dontneed_bytes: u64,
    /// The page-cache cap of the cap-driven `DONTNEED` policy.
    pub dontneed_cap: u64,
    /// `/proc/diskstats`.
    pub diskstats: PathBuf,
    /// The tests to run: `write`, `fdatasync`, `mixed`, `punch`, `dontneed`.
    pub tests: Vec<String>,
    /// `/proc/meminfo`.
    pub meminfo: PathBuf,
}

fn row(target: &Target, test: &str, params: String) -> Row {
    Row {
        target: target.label.clone(),
        test: test.into(),
        params,
        mb_s: 0.0,
        lat: None,
        extra: BTreeMap::new(),
        io: BTreeMap::new(),
        seen: Seen::default(),
    }
}

/// The write grid on one target.
///
/// # Errors
/// When a write fails.
pub fn write_grid(cfg: &Config, t: &Target, clock: &dyn Clock) -> Result<Vec<Row>, Error> {
    let sampler = Sampler::new(&cfg.diskstats, &t.devs);
    let mut rows = Vec::new();
    for &kib in &cfg.blocks_kib {
        for &jobs in &cfg.jobs {
            for &mode in &cfg.modes {
                let ((bytes, lat), io) =
                    measure(clock, Some(&sampler), cfg.warmup, cfg.measure, |span| {
                        writers(&t.dir, "w", jobs, kib << 10, mode, clock, span)
                    })?;
                let mut r = row(
                    t,
                    "write",
                    format!("block={kib}KiB jobs={jobs} mode={}", mode.name()),
                );
                r.mb_s = mb_per_s(bytes, cfg.measure.as_secs_f64());
                r.lat = summarize(&lat);
                r.io = io;
                rows.push(r);
            }
        }
    }
    Ok(rows)
}

/// `fdatasync` latency: every second, 1 MiB is written to each of `files` entry files and each
/// is `fdatasync`ed; beside `background` sequential writers when that is not 0.
///
/// # Errors
/// When a write or a sync fails.
pub fn fsync_test(
    cfg: &Config,
    t: &Target,
    background: usize,
    clock: &dyn Clock,
) -> Result<Row, Error> {
    let paths: Vec<PathBuf> = (0..cfg.sync_files)
        .map(|i| t.dir.join(format!("b-entry-{i}")))
        .collect();
    remove_all(&paths);
    let files = paths
        .iter()
        .map(|p| create(p, Mode::Buffered))
        .collect::<Result<Vec<File>, Error>>()?;
    let mut buf = vec![0u8; usize::try_from(MIB).unwrap_or(1 << 20)];
    fill(&mut buf, 7);
    let period = Duration::from_secs(1);
    let total = period * u32::try_from(cfg.sync_commits).unwrap_or(u32::MAX);
    let sampler = Sampler::new(&cfg.diskstats, &t.devs);
    let lat = Mutex::new(Vec::new());
    let ((), io) = measure(clock, Some(&sampler), Duration::ZERO, total, |span| {
        std::thread::scope(|scope| {
            let bg = (background > 0).then(|| {
                scope.spawn(|| {
                    writers(
                        &t.dir,
                        "bg",
                        background,
                        1 << 20,
                        WriteMode::BufferedSync,
                        clock,
                        span,
                    )
                })
            });
            let mut off = 0u64;
            for k in 0..cfg.sync_commits {
                let due = span.from + period * u32::try_from(k).unwrap_or(0);
                while clock.now() < due {
                    clock.sleep(Duration::from_millis(5));
                }
                for (f, p) in files.iter().zip(&paths) {
                    write_full_at(f, &buf, off).map_err(Error::io("write", p))?;
                }
                for (f, p) in files.iter().zip(&paths) {
                    let t0 = clock.now();
                    rustix::fs::fdatasync(f).map_err(|e| Error::io("fdatasync", p)(e.into()))?;
                    let ms = (clock.now() - t0).as_secs_f64() * 1000.0;
                    lat.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(ms);
                }
                off += MIB;
            }
            match bg {
                Some(h) => crate::run::joined(h.join()).map(|_| ()),
                None => Ok(()),
            }
        })
    })?;
    remove_all(&paths);
    let lat = lat
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = row(
        t,
        "fdatasync",
        format!(
            "files={} batch=1MiB every=1s commits={} background={background}",
            cfg.sync_files, cfg.sync_commits
        ),
    );
    out.lat = summarize(&lat);
    out.io = io;
    Ok(out)
}

/// The mixed load: 8 jobs, each operation a 128 KiB `O_DIRECT` random read of the read set with
/// probability 0.7, else a 1 MiB buffered sequential write (`fdatasync` per 32 MiB).
///
/// # Errors
/// When a read or a write fails.
pub fn mixed_test(cfg: &Config, t: &Target, clock: &dyn Clock) -> Result<Row, Error> {
    let set: Vec<PathBuf> = (0..4).map(|i| t.dir.join(format!("b-read-{i}"))).collect();
    let each = (cfg.mixed_read_set / 4).max(1 << 20) / (1 << 20) * (1 << 20);
    for (i, p) in set.iter().enumerate() {
        if std::fs::metadata(p).map(|m| m.len()).ok() != Some(each) {
            let _ = std::fs::remove_file(p);
            write_data_file(p, each, 4 << 20, i as u64)?;
        }
    }
    let sampler = Sampler::new(&cfg.diskstats, &t.devs);
    let jobs = 8usize;
    let ((rd, wr, lat), io) = measure(clock, Some(&sampler), cfg.warmup, cfg.measure, |span| {
        std::thread::scope(|scope| {
            let hs: Vec<_> = (0..jobs)
                .map(|j| {
                    let set = &set;
                    scope.spawn(move || mixed_job(j, set, each, &t.dir, clock, span))
                })
                .collect();
            let (mut rd, mut wr, mut lat) = (0u64, 0u64, Vec::new());
            for h in hs {
                let (r, w, l) = crate::run::joined(h.join())?;
                rd += r;
                wr += w;
                lat.extend(l);
            }
            Ok((rd, wr, lat))
        })
    })?;
    let secs = cfg.measure.as_secs_f64();
    let mut out = row(
        t,
        "mixed",
        "jobs=8 read=70%x128KiB-random-direct write=30%x1MiB-seq".into(),
    );
    out.mb_s = mb_per_s(rd, secs);
    out.lat = summarize(&lat);
    out.extra.insert("write_mb_s".into(), mb_per_s(wr, secs));
    out.io = io;
    Ok(out)
}

fn mixed_job(
    j: usize,
    set: &[PathBuf],
    each: u64,
    dir: &Path,
    clock: &dyn Clock,
    span: Span,
) -> Result<(u64, u64, Vec<f64>), Error> {
    let readers = set
        .iter()
        .map(|p| open_read(p, Mode::Direct))
        .collect::<Result<Vec<File>, Error>>()?;
    let wpath = dir.join(format!("b-mixed-w-{j}"));
    let _ = std::fs::remove_file(&wpath);
    let w = create(&wpath, Mode::Buffered)?;
    let mut rng = Rng::new(j as u64 + 11);
    let mut rbuf = AlignedBuf::new(128 << 10);
    let mut wbuf = vec![0u8; 1 << 20];
    fill(&mut wbuf, j as u64);
    let (mut rd, mut wr, mut woff, mut lat) = (0u64, 0u64, 0u64, Vec::new());
    loop {
        let t0 = clock.now();
        if t0 >= span.end {
            break;
        }
        if rng.below(10) < 7 {
            let fi = usize::try_from(rng.below(readers.len() as u64)).unwrap_or(0);
            let off = rng.below(each / (128 << 10)) * (128 << 10);
            read_full_at(&readers[fi], rbuf.as_mut_slice(), off)
                .map_err(Error::io("read", &set[fi]))?;
            let t1 = clock.now();
            if span.holds(t0, t1) {
                rd += 128 << 10;
                lat.push((t1 - t0).as_secs_f64() * 1000.0);
            }
        } else {
            write_full_at(&w, &wbuf, woff).map_err(Error::io("write", &wpath))?;
            woff += 1 << 20;
            if woff % SYNC_EVERY == 0 {
                rustix::fs::fdatasync(&w).map_err(|e| Error::io("fdatasync", &wpath)(e.into()))?;
            }
            if span.holds(t0, clock.now()) {
                wr += 1 << 20;
            }
        }
    }
    let _ = std::fs::remove_file(&wpath);
    Ok((rd, wr, lat))
}

/// Hole punching: a file of `punch_bytes`, then every other 16 MiB group punched; reports the
/// file's allocated bytes, the file system's free bytes, and (for a loop device) the image's
/// allocated bytes, before and after, and after `fstrim` when `fstrim` names the mount point.
///
/// # Errors
/// When a step fails.
pub fn punch_test(
    cfg: &Config,
    t: &Target,
    image: Option<&Path>,
    fstrim: Option<(&dyn Runner, &Path)>,
) -> Result<Row, Error> {
    let p = t.dir.join("b-punch");
    let _ = std::fs::remove_file(&p);
    write_data_file(&p, cfg.punch_bytes, 4 << 20, 3)?;
    let img = |i: Option<&Path>| i.map(allocated).transpose();
    let (file0, free0, img0) = (allocated(&p)?, fs_space(&t.dir)?.1, img(image)?);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .map_err(Error::io("open", &p))?;
    let group = 16 * MIB;
    let mut off = 0;
    while off < cfg.punch_bytes {
        punch(&f, off, group.min(cfg.punch_bytes - off), &p)?;
        off += 2 * group;
    }
    f.sync_all().map_err(Error::io("fsync", &p))?;
    let (file1, free1, img1) = (allocated(&p)?, fs_space(&t.dir)?.1, img(image)?);
    let mut out = row(
        t,
        "punch",
        format!("file={}MiB group=16MiB every-other", cfg.punch_bytes / MIB),
    );
    let mut put = |k: &str, v: u64| {
        out.extra.insert(k.into(), v as f64 / MIB as f64);
    };
    put("file_mib_before", file0);
    put("file_mib_after", file1);
    put("fs_freed_mib", free1.saturating_sub(free0));
    if let (Some(a), Some(b)) = (img0, img1) {
        put("image_freed_mib", a.saturating_sub(b));
        if let Some((run, mnt)) = fstrim {
            run.run(&["fstrim", &mnt.to_string_lossy()])?;
            put(
                "image_freed_after_fstrim_mib",
                a.saturating_sub(img(image)?.unwrap_or(a)),
            );
        }
    }
    drop(f);
    let _ = std::fs::remove_file(&p);
    Ok(out)
}

/// When pages are dropped after a cell is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Policy {
    /// Never.
    Never,
    /// Right after each commit.
    AtCommit,
    /// When the written-and-not-dropped bytes exceed the cap, oldest commits first.
    Cap,
}

/// One `DONTNEED` policy on a read-after-serve pattern: `dontneed_bytes` written in 1 MiB cells,
/// committed (`fdatasync`) per 32 MiB, pages dropped per the policy; then every cell read back.
/// `cached` reads the bytes in the page cache of the whole machine (`Cached:` of `/proc/meminfo`
/// in a run); the row reports how much it grew over the writes.
///
/// # Errors
/// When a step fails.
pub fn dontneed_test(
    cfg: &Config,
    t: &Target,
    policy: Policy,
    cached: &dyn Fn() -> Result<u64, Error>,
    clock: &dyn Clock,
) -> Result<Row, Error> {
    let p = t.dir.join("b-dontneed");
    let _ = std::fs::remove_file(&p);
    let before = cached()?;
    let f = create(&p, Mode::Buffered)?;
    let mut cell = vec![0u8; 1 << 20];
    let (mut off, mut kept_from) = (0u64, 0u64);
    let t0 = clock.now();
    while off < cfg.dontneed_bytes {
        fill(&mut cell, off);
        write_full_at(&f, &cell, off).map_err(Error::io("write", &p))?;
        off += MIB;
        if off % SYNC_EVERY == 0 || off >= cfg.dontneed_bytes {
            rustix::fs::fdatasync(&f).map_err(|e| Error::io("fdatasync", &p)(e.into()))?;
            match policy {
                Policy::Never => {}
                Policy::AtCommit => {
                    drop_pages(&f, kept_from, off - kept_from, &p)?;
                    kept_from = off;
                }
                Policy::Cap => {
                    while off - kept_from > cfg.dontneed_cap {
                        let n = SYNC_EVERY.min(off - kept_from);
                        drop_pages(&f, kept_from, n, &p)?;
                        kept_from += n;
                    }
                }
            }
        }
    }
    let write_secs = (clock.now() - t0).as_secs_f64();
    let grew = cached()?.saturating_sub(before);
    let r = File::open(&p).map_err(Error::io("open", &p))?;
    let (mut lat, mut roff) = (Vec::new(), 0u64);
    let r0 = clock.now();
    while roff < off {
        let a = clock.now();
        read_full_at(&r, &mut cell, roff).map_err(Error::io("read", &p))?;
        lat.push((clock.now() - a).as_secs_f64() * 1000.0);
        roff += MIB;
    }
    let read_secs = (clock.now() - r0).as_secs_f64();
    let mut out = row(
        t,
        "dontneed",
        format!(
            "policy={policy:?} bytes={}MiB cap={}MiB",
            cfg.dontneed_bytes / MIB,
            cfg.dontneed_cap / MIB
        ),
    );
    out.mb_s = mb_per_s(off, read_secs);
    out.lat = summarize(&lat);
    out.extra
        .insert("write_mb_s".into(), mb_per_s(off, write_secs));
    out.extra
        .insert("cache_grew_mib".into(), grew as f64 / MIB as f64);
    drop((f, r));
    let _ = std::fs::remove_file(&p);
    Ok(out)
}

/// The `Cached:` bytes of a `/proc/meminfo` at `path`.
///
/// # Errors
/// When it cannot be read or has no such line.
pub fn cached_bytes(path: &Path) -> Result<u64, Error> {
    let text = std::fs::read_to_string(path).map_err(Error::io("read", path))?;
    text.lines()
        .find_map(|l| l.strip_prefix("Cached:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .ok_or_else(|| Error::Invalid(format!("{} has no Cached: line", path.display())))
}

/// What a target needs beyond its directory: the loop image behind it (for the punch test) and
/// its mount point when `fstrim` may run on it (only a loop the arm made itself).
#[derive(Debug, Clone, Default)]
pub struct Extras {
    /// The image file of a loop target.
    pub image: Option<PathBuf>,
    /// The mount point to `fstrim` after punching.
    pub fstrim: Option<PathBuf>,
}

/// One test of a target, run under the guard.
type Test<'a> = Box<dyn FnMut() -> Result<Vec<Row>, Error> + 'a>;

/// Every test on every target, recording each row as it ends.
///
/// # Errors
/// When a test fails.
pub fn run(
    cfg: &Config,
    extras: &[Extras],
    runner: &dyn Runner,
    clock: &dyn Clock,
    out: &crate::report::Out,
    guard: Option<&Guard>,
) -> Result<Vec<Row>, Error> {
    let mut rows = Vec::new();
    let none = Extras::default();
    for (i, t) in cfg.targets.iter().enumerate() {
        let x = extras.get(i).unwrap_or(&none);
        let trim = x.fstrim.as_deref().map(|m| (runner, m));
        let resident = || cached_bytes(&cfg.meminfo);
        let mut tests: Vec<(&str, Test<'_>)> = Vec::new();
        for kib in &cfg.blocks_kib {
            for jobs in &cfg.jobs {
                for mode in &cfg.modes {
                    let one = Config {
                        blocks_kib: vec![*kib],
                        jobs: vec![*jobs],
                        modes: vec![*mode],
                        ..cfg.clone()
                    };
                    tests.push(("write", Box::new(move || write_grid(&one, t, clock))));
                }
            }
        }
        let bg = cfg.sync_background;
        tests.push((
            "fdatasync",
            Box::new(|| fsync_test(cfg, t, 0, clock).map(|r| vec![r])),
        ));
        tests.push((
            "fdatasync",
            Box::new(move || fsync_test(cfg, t, bg, clock).map(|r| vec![r])),
        ));
        tests.push((
            "mixed",
            Box::new(|| mixed_test(cfg, t, clock).map(|r| vec![r])),
        ));
        let image = x.image.as_deref();
        tests.push((
            "punch",
            Box::new(move || punch_test(cfg, t, image, trim).map(|r| vec![r])),
        ));
        for policy in [Policy::Never, Policy::AtCommit, Policy::Cap] {
            let resident = &resident;
            let run = move || dontneed_test(cfg, t, policy, resident, clock).map(|r| vec![r]);
            tests.push(("dontneed", Box::new(run)));
        }
        let chosen = |name: &str| cfg.tests.iter().any(|n| n == name);
        for (_, test) in tests.iter_mut().filter(|(name, _)| chosen(name)) {
            let (got, seen) = guarded(guard, clock, test)?;
            for mut r in got {
                r.seen = seen;
                out.record(&r)?;
                rows.push(r);
            }
        }
        for i in 0..4 {
            let _ = std::fs::remove_file(t.dir.join(format!("b-read-{i}")));
        }
    }
    out.tables(&tables(&rows))?;
    Ok(rows)
}

/// The arm's tables: one per test.
#[must_use]
pub fn tables(rows: &[Row]) -> Vec<Table> {
    let mut out = Vec::new();
    for test in ["write", "fdatasync", "mixed", "punch", "dontneed"] {
        let mut t = Table::new(
            &format!("B, cache device: {test}"),
            &[
                "target",
                "params",
                "MB/s",
                "p50 ms",
                "p99 ms",
                "max ms",
                "more",
                "util % (per device)",
                "foreign MB/s",
                "load",
            ],
        );
        for r in rows.iter().filter(|r| r.test == test) {
            let more: Vec<String> = r
                .extra
                .iter()
                .map(|(k, v)| format!("{k}={}", num(*v, 1)))
                .collect();
            let util: Vec<String> =
                r.io.iter()
                    .map(|(k, v)| format!("{k}:{}", num(v.util, 0)))
                    .collect();
            t.row(vec![
                r.target.clone(),
                r.params.clone(),
                num(r.mb_s, 1),
                r.lat.map_or("-".into(), |l| num(l.p50, 2)),
                r.lat.map_or("-".into(), |l| num(l.p99, 2)),
                r.lat.map_or("-".into(), |l| num(l.max, 2)),
                more.join(" "),
                util.join(" "),
                num(r.seen.foreign_mb_s, 1),
                num(r.seen.load1, 0),
            ]);
        }
        if !t.rows().is_empty() {
            out.push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests;
