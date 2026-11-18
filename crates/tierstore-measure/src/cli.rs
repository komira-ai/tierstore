//! The `ts-measure` command line: one verb per arm (or step of an arm). Every verb writes its
//! rows and tables under `--out`; durations are in milliseconds and sizes in MiB.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::Error;
use crate::blockdev::{backing_file, device_of};
use crate::clock::Clock;
use crate::io::MIB;
use crate::report::Out;
use crate::run::{Runner, preflight};
use crate::workload::Pattern;

/// The farm measurements of tierstore's fetch, cache and cold-tier design.
#[derive(Debug, Parser)]
#[command(name = "ts-measure")]
pub struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

/// Flags every verb has.
#[derive(Debug, Clone, Args)]
pub struct Common {
    /// The directory the verb writes its rows and tables to.
    #[arg(long)]
    pub out: PathBuf,
    /// The proc file system.
    #[arg(long, default_value = "/proc")]
    pub proc_root: PathBuf,
    /// The sys file system.
    #[arg(long, default_value = "/sys")]
    pub sys_root: PathBuf,
    /// Seconds of the idle check before a run (0 skips it).
    #[arg(long, default_value_t = 60)]
    pub idle_secs: u64,
    /// The most `%util` a sampled device may show in the idle check.
    #[arg(long, default_value_t = 5.0)]
    pub idle_max_util: f64,
    /// A process whose disk bytes are foreign work (repeat; none turns the guard off).
    #[arg(long)]
    pub guard_comm: Vec<String>,
    /// The most foreign MB/s a clean cell sees.
    #[arg(long, default_value_t = 2.0)]
    pub guard_max_mb_s: f64,
    /// The highest load a clean cell ends with.
    #[arg(long, default_value_t = 16.0)]
    pub guard_max_load: f64,
    /// Attempts per cell.
    #[arg(long, default_value_t = 4)]
    pub guard_attempts: u32,
    /// The quiet check's window in ms.
    #[arg(long, default_value_t = 10_000)]
    pub guard_quiet_ms: u64,
    /// The longest wait for quiet in ms.
    #[arg(long, default_value_t = 900_000)]
    pub guard_max_wait_ms: u64,
}

impl Common {
    fn diskstats(&self) -> PathBuf {
        self.proc_root.join("diskstats")
    }

    fn guard(&self) -> Option<crate::guard::Guard> {
        (!self.guard_comm.is_empty()).then(|| crate::guard::Guard {
            proc_root: self.proc_root.clone(),
            comms: self.guard_comm.clone(),
            max_mb_s: self.guard_max_mb_s,
            max_load: self.guard_max_load,
            attempts: self.guard_attempts,
            quiet_window: ms(self.guard_quiet_ms),
            max_wait: ms(self.guard_max_wait_ms),
        })
    }
}

#[derive(Debug, Subcommand)]
enum Verb {
    /// E: node facts (read only).
    Facts {
        #[command(flatten)]
        common: Common,
        /// The node's name in tables.
        #[arg(long)]
        node: String,
        /// The SSD's device name, for its partition table and free space.
        #[arg(long)]
        ssd: Option<String>,
        /// HDD mount points, for C-6 drive facts.
        #[arg(long)]
        hdd: Vec<PathBuf>,
        /// Silo part files to map per drive.
        #[arg(long, default_value_t = 4)]
        parts: usize,
    },
    /// D: hash rates.
    Cpu {
        #[command(flatten)]
        common: Common,
        /// The node's name in tables.
        #[arg(long)]
        node: String,
        /// Milliseconds per rate.
        #[arg(long, default_value_t = 3000)]
        ms: u64,
    },
    /// A: write the region files (`a-<i>.dat`) on each drive.
    HddPrep(Hdd),
    /// A: the raw HDD grid.
    Hdd(Hdd),
    /// B: the cache device.
    Cache(Cache),
    /// C-5: the pack-write cells.
    WriteCells(Write5),
    /// C-5: the sustained curve and the rewrite after deletes.
    WriteSustained(Write5),
    /// C-5: steady-state churn on a mounted dedicated file system.
    Churn(Churn),
    /// Make an xfs on a loop device over a new image and mount it.
    LoopfsSetup {
        /// The image file to create.
        #[arg(long)]
        image: PathBuf,
        /// Its size in GiB.
        #[arg(long)]
        gib: u64,
        /// Attach with `--direct-io=on`.
        #[arg(long)]
        direct_io: bool,
        /// The mount point.
        #[arg(long)]
        mnt: PathBuf,
    },
    /// C-1 and C-6: list a silo bucket as `key<TAB>size` lines, and its size distribution.
    SiloList {
        #[command(flatten)]
        common: Common,
        /// Silo's S3 endpoint.
        #[arg(long, default_value = "http://127.0.0.1:30900")]
        endpoint: String,
        /// The bucket.
        #[arg(long, default_value = "kbf-cas")]
        bucket: String,
        /// Silo's environment file with its root credentials.
        #[arg(long)]
        credentials: PathBuf,
    },
    /// C-1: ranged GETs through the local silo.
    SiloGet {
        #[command(flatten)]
        common: Common,
        /// Silo's S3 endpoint.
        #[arg(long, default_value = "http://127.0.0.1:30900")]
        endpoint: String,
        /// The bucket.
        #[arg(long, default_value = "kbf-cas")]
        bucket: String,
        /// Silo's environment file with its root credentials.
        #[arg(long)]
        credentials: PathBuf,
        /// The listing `silo-list` wrote; packs of at least `--min-pack-mib` are read.
        #[arg(long)]
        listing: PathBuf,
        /// The smallest pack read, MiB.
        #[arg(long, default_value_t = 1024)]
        min_pack_mib: u64,
        /// This node's index among the reading nodes, and their count.
        #[arg(long, default_value_t = 0)]
        node_index: usize,
        /// Nodes reading at once.
        #[arg(long, default_value_t = 1)]
        nodes: usize,
        /// The node's name in tables.
        #[arg(long)]
        node: String,
        /// Start at this Unix time (seconds), so several nodes start together.
        #[arg(long)]
        start_at: Option<u64>,
        /// Part sizes in MiB.
        #[arg(long, value_delimiter = ',', default_value = "4,8,16,32,64")]
        parts: Vec<u64>,
        /// Concurrent GETs per node.
        #[arg(long, value_delimiter = ',', default_value = "1,2,4,8")]
        conc: Vec<usize>,
        /// The node's drives (`sdb,sdc`).
        #[arg(long, value_delimiter = ',')]
        devs: Vec<String>,
        /// Warm-up per cell in ms.
        #[arg(long, default_value_t = 5000)]
        warmup_ms: u64,
        /// Measured time per cell in ms.
        #[arg(long, default_value_t = 20000)]
        measure_ms: u64,
    },
    /// C-6 and C-7: the size distribution and the placement analysis of a listing.
    Analyse {
        #[command(flatten)]
        common: Common,
        /// The listing `silo-list` wrote.
        #[arg(long)]
        listing: PathBuf,
        /// Copies.
        #[arg(long, default_value_t = 2)]
        copies: usize,
        /// Extra probe depth.
        #[arg(long, default_value_t = 2)]
        probe_extra: usize,
    },
    /// C-2 to C-4, mixed, slow: write the packs the prototype serves.
    ColdPrep(ColdDrives),
    /// C-2, C-3, C-4, mixed and slow cells against an in-process prototype on loopback.
    ColdLocal {
        #[command(flatten)]
        drives: ColdDrives,
        /// Which sweeps: c2, c3, c4, mixed, slow.
        #[arg(long, value_delimiter = ',', default_value = "c2,c3,c4,mixed,slow")]
        arms: Vec<String>,
        /// Warm-up per cell in ms.
        #[arg(long, default_value_t = 5000)]
        warmup_ms: u64,
        /// Measured time per cell in ms.
        #[arg(long, default_value_t = 20000)]
        measure_ms: u64,
    },
    /// Serve the prototype on an address until `--secs` pass (for peer and many-node loads).
    ColdServe {
        #[command(flatten)]
        drives: ColdDrives,
        /// The address to listen on.
        #[arg(long)]
        listen: std::net::SocketAddr,
        /// How long to serve, seconds.
        #[arg(long)]
        secs: u64,
    },
    /// Run a load against a prototype elsewhere and record what this client saw.
    ColdLoad {
        #[command(flatten)]
        common: Common,
        /// The service.
        #[arg(long)]
        addr: std::net::SocketAddr,
        /// The label in tables.
        #[arg(long)]
        label: String,
        /// The drive read.
        #[arg(long, default_value = "hdd1")]
        drive: String,
        /// Packs per drive and their size in MiB.
        #[arg(long, default_value_t = 24)]
        packs: usize,
        /// Their size in MiB.
        #[arg(long, default_value_t = 1024)]
        pack_mib: u64,
        /// Background streams.
        #[arg(long, default_value_t = 0)]
        background: usize,
        /// MiB per background GET (0: whole packs).
        #[arg(long, default_value_t = 64)]
        part_mib: u64,
        /// MiB per demand (0: none).
        #[arg(long, default_value_t = 1)]
        demand_mib: u64,
        /// ms between demands.
        #[arg(long, default_value_t = 500)]
        every_ms: u64,
        /// Start at this Unix time (seconds).
        #[arg(long)]
        start_at: Option<u64>,
        /// Warm-up in ms.
        #[arg(long, default_value_t = 5000)]
        warmup_ms: u64,
        /// Measured time in ms.
        #[arg(long, default_value_t = 20000)]
        measure_ms: u64,
    },
    /// Unmount, detach and remove a loop file system made by `loopfs-setup`.
    LoopfsTeardown {
        /// The image file.
        #[arg(long)]
        image: PathBuf,
        /// The loop device (`/dev/loop7`).
        #[arg(long)]
        dev: String,
        /// The mount point.
        #[arg(long)]
        mnt: PathBuf,
    },
}

/// Flags of arm A.
#[derive(Debug, Clone, Args)]
struct Hdd {
    #[command(flatten)]
    common: Common,
    /// A scratch directory per drive, the first used alone for the set `one`.
    #[arg(long, required = true)]
    dir: Vec<PathBuf>,
    /// Drive sets.
    #[arg(long, value_delimiter = ',', default_value = "one,both")]
    sets: Vec<String>,
    /// Request sizes in MiB.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8,16")]
    sizes: Vec<u64>,
    /// Streams per drive.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8")]
    streams: Vec<usize>,
    /// Read modes.
    #[arg(long, value_delimiter = ',', default_value = "ra8192,ra512,direct")]
    modes: Vec<String>,
    /// Patterns.
    #[arg(long, value_delimiter = ',', default_value = "adjacent,scattered")]
    patterns: Vec<String>,
    /// Region files per drive.
    #[arg(long, default_value_t = 8)]
    files: usize,
    /// Size of a region file in MiB.
    #[arg(long, default_value_t = 8192)]
    file_mib: u64,
    /// Warm-up per cell in ms.
    #[arg(long, default_value_t = 5000)]
    warmup_ms: u64,
    /// Measured time per cell in ms.
    #[arg(long, default_value_t = 20000)]
    measure_ms: u64,
}

/// Flags of arm B.
#[derive(Debug, Clone, Args)]
struct Cache {
    #[command(flatten)]
    common: Common,
    /// A target as `label=dir`; repeat for each.
    #[arg(long, required = true)]
    target: Vec<String>,
    /// For a target, `label=mountpoint`: run `fstrim` there after the punch test.
    #[arg(long)]
    fstrim: Vec<String>,
    /// Write block sizes in KiB.
    #[arg(long, value_delimiter = ',', default_value = "128,1024,4096")]
    blocks: Vec<usize>,
    /// Concurrent writers.
    #[arg(long, value_delimiter = ',', default_value = "1,4,16")]
    jobs: Vec<usize>,
    /// Warm-up per cell in ms.
    #[arg(long, default_value_t = 5000)]
    warmup_ms: u64,
    /// Measured time per cell in ms.
    #[arg(long, default_value_t = 20000)]
    measure_ms: u64,
    /// Commits of the `fdatasync` test.
    #[arg(long, default_value_t = 60)]
    sync_commits: usize,
    /// MiB of the mixed test's read set.
    #[arg(long, default_value_t = 4096)]
    read_set_mib: u64,
    /// MiB of the punch test's file.
    #[arg(long, default_value_t = 1024)]
    punch_mib: u64,
    /// MiB written per `DONTNEED` policy.
    #[arg(long, default_value_t = 4096)]
    dontneed_mib: u64,
    /// The cap of the cap-driven policy in MiB.
    #[arg(long, default_value_t = 256)]
    dontneed_cap_mib: u64,
}

/// Flags of the C-5 write verbs.
#[derive(Debug, Clone, Args)]
struct Write5 {
    #[command(flatten)]
    common: Common,
    /// The scratch directory on the drive.
    #[arg(long)]
    dir: PathBuf,
    /// Pack sizes in MiB.
    #[arg(long, value_delimiter = ',', default_value = "64,1024")]
    sizes: Vec<u64>,
    /// Writers per drive.
    #[arg(long, value_delimiter = ',', default_value = "1,2")]
    writers: Vec<usize>,
    /// Measured time per cell in ms.
    #[arg(long, default_value_t = 20000)]
    measure_ms: u64,
    /// MiB of the sustained phase.
    #[arg(long, default_value_t = 204_800)]
    sustained_mib: u64,
    /// MiB per pack of the sustained phase.
    #[arg(long, default_value_t = 1024)]
    pack_mib: u64,
    /// MiB per window of the curve.
    #[arg(long, default_value_t = 10_240)]
    window_mib: u64,
    /// The cap of the rewrite phase in MiB.
    #[arg(long, default_value_t = 409_600)]
    rewrite_cap_mib: u64,
    /// The free fraction the drive keeps after the arm.
    #[arg(long, default_value_t = 0.5)]
    keep_free: f64,
}

/// Flags of the churn arm.
#[derive(Debug, Clone, Args)]
struct Churn {
    #[command(flatten)]
    common: Common,
    /// The mount point of the dedicated file system.
    #[arg(long)]
    dir: PathBuf,
    /// The band's bottom, percent.
    #[arg(long)]
    lo: f64,
    /// The band's top, percent.
    #[arg(long)]
    hi: f64,
    /// Pack sizes in MiB.
    #[arg(long, value_delimiter = ',', default_value = "64,128,256,512,1024")]
    sizes: Vec<u64>,
    /// Capacities of writes after the fill.
    #[arg(long, default_value_t = 3.0)]
    turns: f64,
    /// MiB per window.
    #[arg(long, default_value_t = 8192)]
    window_mib: u64,
    /// The reader reads 4 MiB every this many ms.
    #[arg(long, default_value_t = 200)]
    read_every_ms: u64,
    /// The label in tables.
    #[arg(long)]
    label: String,
}

/// The prototype's drives and packs.
#[derive(Debug, Clone, Args)]
struct ColdDrives {
    #[command(flatten)]
    common: Common,
    /// A drive as `name=dir`; repeat (the first is the one loaded, the second the other copy).
    #[arg(long, required = true)]
    drive: Vec<String>,
    /// Packs per drive.
    #[arg(long, default_value_t = 24)]
    packs: usize,
    /// Their size in MiB.
    #[arg(long, default_value_t = 1024)]
    pack_mib: u64,
}

impl ColdDrives {
    fn map(&self) -> Result<std::collections::BTreeMap<String, PathBuf>, Error> {
        self.drive.iter().map(|d| labelled(d)).collect()
    }

    fn packs(&self) -> crate::cold::arms::Packs {
        crate::cold::arms::Packs {
            count: self.packs,
            size: self.pack_mib * MIB,
        }
    }
}

fn wait_until(clock: &dyn Clock, at: Option<u64>) {
    let ahead = at.map_or(0, |t| t.saturating_sub(clock.unix_secs()));
    let end = clock.now() + Duration::from_secs(ahead);
    while clock.now() < end {
        clock.sleep(Duration::from_millis(100));
    }
}

/// What a verb runs with: the programs and the clock (fakes in tests).
pub struct Env<'a> {
    /// The programs.
    pub runner: &'a dyn Runner,
    /// The clock.
    pub clock: &'a dyn Clock,
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn patterns(names: &[String]) -> Result<Vec<Pattern>, Error> {
    names
        .iter()
        .map(|n| match n.as_str() {
            "adjacent" => Ok(Pattern::Adjacent),
            "scattered" => Ok(Pattern::Scattered),
            other => Err(Error::Invalid(format!(
                "pattern {other}: want adjacent or scattered"
            ))),
        })
        .collect()
}

fn labelled(v: &str) -> Result<(String, PathBuf), Error> {
    v.split_once('=')
        .map(|(l, p)| (l.to_string(), PathBuf::from(p)))
        .ok_or_else(|| Error::Invalid(format!("{v}: want label=path")))
}

/// The devices to sample for a directory: its device, and for a loop the device under its image.
fn devices_of(dir: &Path, sys: &Path) -> Result<Vec<String>, Error> {
    let dev = device_of(dir, sys)?;
    let mut v = vec![dev.clone()];
    if let Some(img) = backing_file(sys, &dev) {
        v.push(device_of(&img, sys)?);
    }
    Ok(v)
}

fn idle(c: &Common, devs: &[String], env: &Env<'_>) -> Result<(), Error> {
    if c.idle_secs > 0 {
        let s = crate::diskstats::Sampler::new(&c.diskstats(), devs);
        preflight(&s, env.clock, c.idle_secs, c.idle_max_util)?;
    }
    Ok(())
}

fn hdd_config(h: &Hdd) -> Result<crate::hdd::Config, Error> {
    let drives = h
        .dir
        .iter()
        .map(|d| {
            std::fs::create_dir_all(d).map_err(Error::io("mkdir", d))?;
            Ok(crate::hdd::Drive {
                dir: d.clone(),
                dev: device_of(d, &h.common.sys_root)?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(crate::hdd::Config {
        drives,
        sets: h.sets.clone(),
        sizes_mib: h.sizes.clone(),
        streams: h.streams.clone(),
        modes: h
            .modes
            .iter()
            .map(|m| crate::hdd::ReadMode::parse(m))
            .collect::<Result<_, _>>()?,
        patterns: patterns(&h.patterns)?,
        files: h.files.max(h.streams.iter().copied().max().unwrap_or(1)),
        file_size: h.file_mib * MIB,
        warmup: ms(h.warmup_ms),
        measure: ms(h.measure_ms),
        sys: h.common.sys_root.clone(),
    })
}

fn write_config(w: &Write5) -> Result<crate::write::Config, Error> {
    std::fs::create_dir_all(&w.dir).map_err(Error::io("mkdir", &w.dir))?;
    Ok(crate::write::Config {
        dir: w.dir.clone(),
        dev: device_of(&w.dir, &w.common.sys_root)?,
        sizes_mib: w.sizes.clone(),
        writers: w.writers.clone(),
        measure: ms(w.measure_ms),
        sustained: w.sustained_mib * MIB,
        pack: w.pack_mib * MIB,
        window: w.window_mib * MIB,
        rewrite_cap: w.rewrite_cap_mib * MIB,
        keep_free: w.keep_free,
        diskstats: w.common.diskstats(),
    })
}

fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::net)
}

/// Runs a parsed command line.
///
/// # Errors
/// The verb's error.
pub fn run(cli: Cli, env: &Env<'_>, stdout: &mut dyn Write) -> Result<(), Error> {
    match cli.verb {
        Verb::Facts {
            common,
            node,
            ssd,
            hdd,
            parts,
        } => {
            let out = Out::new(&common.out, &format!("facts-{node}"))?;
            let mut facts = crate::facts::collect(
                &crate::facts::node_probes(&common.proc_root, &common.sys_root, ssd.as_deref()),
                &common.proc_root,
                env.runner,
            );
            let cpuinfo =
                std::fs::read_to_string(common.proc_root.join("cpuinfo")).unwrap_or_default();
            facts.insert(
                "sha_ni".into(),
                crate::cpu::has_sha_ni(&cpuinfo).to_string(),
            );
            for m in &hdd {
                let dev = device_of(m, &common.sys_root)?;
                let ps = crate::facts::sample_parts(&m.join("kbf-cas"), parts, 16 << 20, 200_000);
                facts.extend(crate::facts::collect(
                    &crate::facts::drive_probes(&common.sys_root, m, &dev, &ps),
                    &common.proc_root,
                    env.runner,
                ));
            }
            out.record(&facts)?;
            let p = out.path("md");
            std::fs::write(
                &p,
                crate::facts::markdown(&format!("E and C-6, facts: {node}"), &facts),
            )
            .map_err(Error::io("write", &p))?;
        }
        Verb::Cpu {
            common,
            node,
            ms: n,
        } => {
            let out = Out::new(&common.out, &format!("cpu-{node}"))?;
            let all = std::thread::available_parallelism().map_or(1, usize::from);
            let rows = crate::cpu::run(env.clock, ms(n), all);
            for r in &rows {
                out.record(r)?;
            }
            let cpuinfo =
                std::fs::read_to_string(common.proc_root.join("cpuinfo")).unwrap_or_default();
            out.tables(&[crate::cpu::table(
                &node,
                crate::cpu::has_sha_ni(&cpuinfo),
                &rows,
            )])?;
        }
        Verb::HddPrep(h) => crate::hdd::prepare(&hdd_config(&h)?)?,
        Verb::Hdd(h) => {
            let cfg = hdd_config(&h)?;
            let devs: Vec<String> = cfg.drives.iter().map(|d| d.dev.clone()).collect();
            idle(&h.common, &devs, env)?;
            let out = Out::new(&h.common.out, "a")?;
            let guard = h.common.guard();
            crate::hdd::run(&cfg, env.clock, &h.common.diskstats(), &out, guard.as_ref())?;
        }
        Verb::Cache(c) => {
            let mut targets = Vec::new();
            let mut extras = Vec::new();
            for t in &c.target {
                let (label, dir) = labelled(t)?;
                std::fs::create_dir_all(&dir).map_err(Error::io("mkdir", &dir))?;
                let devs = devices_of(&dir, &c.common.sys_root)?;
                let image = backing_file(&c.common.sys_root, &devs[0]);
                let fstrim = c
                    .fstrim
                    .iter()
                    .map(|v| labelled(v))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|(l, _)| *l == label)
                    .map(|(_, m)| m);
                targets.push(crate::cache::Target { label, dir, devs });
                extras.push(crate::cache::Extras { image, fstrim });
            }
            let all: Vec<String> = targets.iter().flat_map(|t| t.devs.clone()).collect();
            idle(&c.common, &all, env)?;
            let cfg = crate::cache::Config {
                targets,
                blocks_kib: c.blocks.clone(),
                jobs: c.jobs.clone(),
                modes: vec![
                    crate::cache::WriteMode::BufferedSync,
                    crate::cache::WriteMode::Direct,
                ],
                warmup: ms(c.warmup_ms),
                measure: ms(c.measure_ms),
                sync_files: 8,
                sync_commits: c.sync_commits,
                sync_background: 4,
                mixed_read_set: c.read_set_mib * MIB,
                punch_bytes: c.punch_mib * MIB,
                dontneed_bytes: c.dontneed_mib * MIB,
                dontneed_cap: c.dontneed_cap_mib * MIB,
                diskstats: c.common.diskstats(),
            };
            let out = Out::new(&c.common.out, "b")?;
            let guard = c.common.guard();
            crate::cache::run(&cfg, &extras, env.runner, env.clock, &out, guard.as_ref())?;
        }
        Verb::WriteCells(w) => {
            let cfg = write_config(&w)?;
            idle(&w.common, std::slice::from_ref(&cfg.dev), env)?;
            let out = Out::new(&w.common.out, "c5-cells")?;
            let rows = crate::write::cells(&cfg, env.runner, env.clock)?;
            for r in &rows {
                out.record(r)?;
            }
            out.tables(&crate::write::tables(&rows))?;
        }
        Verb::WriteSustained(w) => {
            let cfg = write_config(&w)?;
            idle(&w.common, std::slice::from_ref(&cfg.dev), env)?;
            let out = Out::new(&w.common.out, "c5-sustained")?;
            let rows = crate::write::sustained(&cfg, env.runner, env.clock)?;
            for r in &rows {
                out.record(r)?;
            }
            out.tables(&crate::write::tables(&rows))?;
        }
        Verb::Churn(c) => {
            let devs = devices_of(&c.dir, &c.common.sys_root)?;
            idle(&c.common, &devs, env)?;
            let cfg = crate::churn::Config {
                dir: c.dir.clone(),
                devs,
                lo: c.lo,
                hi: c.hi,
                sizes_mib: c.sizes.clone(),
                turns: c.turns,
                window: c.window_mib * MIB,
                read_every: ms(c.read_every_ms),
                diskstats: c.common.diskstats(),
            };
            let out = Out::new(&c.common.out, &format!("c5-churn-{}", c.label))?;
            let rows = crate::churn::run(&cfg, &crate::churn::Statvfs, env.runner, env.clock)?;
            for r in &rows {
                out.record(r)?;
            }
            let size_gib = crate::io::fs_space(&c.dir)?.0 as f64 / f64::from(1u32 << 30);
            out.tables(&crate::churn::tables(
                &c.label,
                &rows,
                c.window_mib as f64 / 1024.0,
                size_gib,
            ))?;
        }
        Verb::LoopfsSetup {
            image,
            gib,
            direct_io,
            mnt,
        } => {
            let fs = crate::loopfs::setup(env.runner, &image, gib << 30, direct_io, &mnt)?;
            writeln!(stdout, "{}", fs.dev).map_err(Error::io("write", Path::new("stdout")))?;
        }
        Verb::SiloList {
            common,
            endpoint,
            bucket,
            credentials,
        } => {
            let store =
                crate::silo::store(&endpoint, &bucket, crate::silo::credentials(&credentials)?)?;
            let out = Out::new(&common.out, &format!("silo-list-{bucket}"))?;
            let rt = runtime()?;
            let n = rt.block_on(crate::silo::list(&store, "", &out.path("tsv")))?;
            writeln!(stdout, "{n} objects").map_err(Error::io("write", Path::new("stdout")))?;
        }
        Verb::SiloGet {
            common,
            endpoint,
            bucket,
            credentials,
            listing,
            min_pack_mib,
            node_index,
            nodes,
            node,
            start_at,
            parts,
            conc,
            devs,
            warmup_ms,
            measure_ms,
        } => {
            let store =
                crate::silo::store(&endpoint, &bucket, crate::silo::credentials(&credentials)?)?;
            let text = std::fs::read_to_string(&listing).map_err(Error::io("read", &listing))?;
            let packs: Vec<(String, u64)> = crate::sizes::parse(&text)
                .into_iter()
                .filter(|(_, n)| *n >= min_pack_mib * MIB)
                .map(|(k, n)| (k.to_string(), n))
                .collect();
            let cfg = crate::silo::Config {
                parts_mib: parts,
                conc,
                warmup: ms(warmup_ms),
                measure: ms(measure_ms),
                nodes,
                devs,
                diskstats: common.diskstats(),
            };
            let out = Out::new(&common.out, &format!("c1-silo-{node}-nodes{nodes}"))?;
            let rt = runtime()?;
            wait_until(env.clock, start_at);
            let rows = crate::silo::run(
                &cfg,
                &store,
                crate::silo::Cursor::new(&packs, node_index, nodes),
                env.clock,
                &rt,
            )?;
            for r in &rows {
                out.record(r)?;
            }
            out.tables(&[crate::silo::table(&node, &rows)])?;
        }
        Verb::Analyse {
            common,
            listing,
            copies,
            probe_extra,
        } => {
            let text = std::fs::read_to_string(&listing).map_err(Error::io("read", &listing))?;
            let objs = crate::sizes::parse(&text);
            let sizes = crate::sizes::analyse(objs.iter().copied());
            let drives = crate::placement::farm(6, 2);
            let place = crate::placement::analyse(objs.iter().copied(), &drives, copies, "n3");
            let out = Out::new(&common.out, "c6-c7")?;
            out.record(&sizes)?;
            out.record(&place)?;
            let mut t = crate::sizes::tables("silo", &sizes);
            t.extend(crate::placement::tables(&place, copies, probe_extra));
            out.tables(&t)?;
        }
        Verb::ColdPrep(d) => {
            let dirs: Vec<PathBuf> = d.map()?.into_values().collect();
            crate::cold::arms::prepare(&dirs, &d.packs())?;
        }
        Verb::ColdLocal {
            drives,
            arms,
            warmup_ms,
            measure_ms,
        } => {
            let map = drives.map()?;
            let names: Vec<String> = map.keys().cloned().collect();
            let first = labelled(&drives.drive[0])?.0;
            let other = names
                .iter()
                .find(|n| **n != first)
                .unwrap_or(&first)
                .clone();
            let mut devs = Vec::new();
            for p in map.values() {
                devs.push(device_of(p, &drives.common.sys_root)?);
            }
            idle(&drives.common, &devs, env)?;
            let sampler = crate::diskstats::Sampler::new(&drives.common.diskstats(), &devs);
            let guard = drives.common.guard();
            let out = Out::new(&drives.common.out, "cold-local")?;
            let cells = crate::cold::arms::cells_for(&arms, &first, &other)?;
            let packs = drives.packs();
            let mut rows = Vec::new();
            for c in &cells {
                let (r, _) = crate::guard::guarded(guard.as_ref(), env.clock, || {
                    crate::cold::arms::local_cell(
                        c,
                        &map,
                        &packs,
                        env.clock,
                        Some(&sampler),
                        ms(warmup_ms),
                        ms(measure_ms),
                    )
                })?;
                out.record(&r)?;
                rows.push(r);
            }
            out.tables(&crate::cold::arms::tables(&rows))?;
        }
        Verb::ColdServe {
            drives,
            listen,
            secs,
        } => {
            let b = crate::cold::arms::base("x");
            let server = crate::cold::server::Server::new(
                crate::cold::server::Config {
                    drives: drives.map()?,
                    params: b.params,
                    chunk: b.chunk,
                    path: b.path,
                    release_after: None,
                },
                env.clock,
            );
            let l = std::net::TcpListener::bind(listen)
                .map_err(|e| Error::Net(format!("listen {listen}: {e}")))?;
            let stop = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| {
                let h = scope.spawn(|| server.serve(&l, &stop));
                let end = env.clock.now() + Duration::from_secs(secs);
                while env.clock.now() < end {
                    env.clock.sleep(Duration::from_millis(100));
                }
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                crate::run::joined(h.join())
            })?;
            let out = Out::new(&drives.common.out, "cold-serve")?;
            out.record(&server.take_served())?;
        }
        Verb::ColdLoad {
            common,
            addr,
            label,
            drive,
            packs,
            pack_mib,
            background,
            part_mib,
            demand_mib,
            every_ms,
            start_at,
            warmup_ms,
            measure_ms,
        } => {
            let mut load = crate::cold::arms::base(&drive).load;
            load.background = background;
            load.part = part_mib * MIB;
            load.demand = demand_mib * MIB;
            load.every = ms(every_ms);
            let p = crate::cold::arms::Packs {
                count: packs,
                size: pack_mib * MIB,
            };
            wait_until(env.clock, start_at);
            let (o, _) =
                crate::run::measure(env.clock, None, ms(warmup_ms), ms(measure_ms), |span| {
                    crate::cold::arms::run_load(addr, &p, &load, env.clock, span)
                })?;
            let r = crate::cold::arms::row(
                "load",
                &label,
                &o,
                ms(measure_ms).as_secs_f64(),
                &[],
                crate::cold::sched::Counts::default(),
                0,
            );
            let out = Out::new(&common.out, &format!("cold-load-{label}"))?;
            out.record(&r)?;
            out.tables(&crate::cold::arms::tables(&[r]))?;
        }
        Verb::LoopfsTeardown { image, dev, mnt } => {
            crate::loopfs::teardown(env.runner, &crate::loopfs::LoopFs { image, dev, mnt })?;
        }
    }
    Ok(())
}

/// The program: this process's arguments, the machine's clock and programs, its standard
/// streams.
#[must_use]
pub fn main() -> std::process::ExitCode {
    let clock = crate::clock::RealClock::new();
    let env = Env {
        runner: &crate::run::System,
        clock: &clock,
    };
    std::process::ExitCode::from(main_with(
        std::env::args_os(),
        &env,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

/// Parses `args` (the program name first) and runs the verb; returns the exit status: 0, 1 for
/// a failed verb (its error on `stderr`), 2 for a bad command line.
pub fn main_with(
    args: impl IntoIterator<Item = OsString>,
    env: &Env<'_>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let _ = write!(stderr, "{e}");
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    match run(cli, env, stdout) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(stderr, "ts-measure: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
