//! The storm test of the io_uring transport (`fsbench storm`; `docs/design/fetch-cache-serve.md`,
//! 5.2 item 4 and 17.1, row T-B3): **`depth x 1.5` parked reads issued from one CPU, and the same
//! on every CPU at once, and the latency of metadata during it**.
//!
//! Over io_uring a request goes to the ring queue of the CPU its caller runs on and waits there,
//! in order, for that queue's next free entry, and a parked READ holds its entry until it is
//! answered (K12 of `docs/design/tsfuse-kernel-findings.md`). On `/dev/fuse` a parked READ holds
//! no reader. The storm measures what that does to the metadata of the stormed CPU and of
//! another.
//!
//! One run mounts the storm's file system ([`crate::stormfs`]) on `tsfuse` with the transport
//! asked for, and runs three phases, each as long as `window` (or until every probe has made
//! `max_probes` `stat`s):
//! 1. `idle`: no storm.
//! 2. `one-cpu`: `per_cpu` storm threads pinned to `storm_cpu`.
//! 3. `every-cpu`: `per_cpu` storm threads pinned to each online CPU.
//!
//! A storm thread opens its own cold file and reads 4 KiB at an offset it has not read before,
//! over and over; the file system parks each such READ for `park` and then answers it. Two
//! probe threads, pinned to `storm_cpu` and to another CPU, `stat` the probe file over and over
//! (entries and attributes are not cached, so each is a request to the server), and each
//! `stat`'s latency goes into a histogram. A phase's probes start once every storm thread has
//! had a read answered (the storm is at its height), and once a `sample_every` the ring entries
//! held (`uring_entries_parked`), the READs parked in the file system and the requests the
//! kernel holds for the connection are sampled. The storm threads stop when the probes end.
//! Each run's record ([`crate::stormrec::StormRecord`]) is appended to the results file as a
//! line of JSON. No figure is judged here: the measurement document reads them. The process's
//! limit of open files is raised to its hard limit first ([`raise_open_files`]).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tsfuse::Serving;
use tsfuse::mount::{MountOptions, Mounter};

use crate::Error;
use crate::clock::Clock;
use crate::hist::Histogram;
use crate::host::Host;
use crate::stormfs::{ColdRead, StormFs};
use crate::stormrec::{FORMAT, PhaseRecord, ProbeRecord, Sampled, StormRecord};
use crate::tsnull::{Detach, Transport, start, transport_of};

/// The name of the process in its errors.
const PROCESS: &str = "storm";

/// The source the mount table shows.
const FSNAME: &str = "fsbench-storm";

/// The bytes of each storm read.
const READ_BYTES: usize = 4096;

/// The distance between the offsets a storm thread reads: more than the readahead window, so no
/// read is answered by the readahead of the one before.
const STRIDE: u64 = 1 << 20;

/// What a series of the storm does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Where to mount (an empty directory).
    pub mountpoint: PathBuf,
    /// The series' label.
    pub label: String,
    /// The runs.
    pub runs: u32,
    /// The transport the session asks for.
    pub transport: Transport,
    /// The `/dev/fuse` readers.
    pub readers: NonZeroUsize,
    /// How long a cold READ is parked.
    pub park: Duration,
    /// How the storm threads read.
    pub read: ColdRead,
    /// The storm threads on each stormed CPU.
    pub per_cpu: u32,
    /// The CPUs the every-CPU storm pins threads to, ascending ([`usable_cpus`]).
    pub cpus: Vec<usize>,
    /// The CPU of the one-CPU storm.
    pub storm_cpu: usize,
    /// The CPU of the probe that the one-CPU storm leaves alone; `None` with one CPU.
    pub other_cpu: Option<usize>,
    /// How long each phase's probes run.
    pub window: Duration,
    /// The most `stat`s of each probe in a phase (0: no limit).
    pub max_probes: u64,
    /// The interval of the samples.
    pub sample_every: Duration,
    /// What to record (`--setting KEY=VALUE`).
    pub settings: BTreeMap<String, String>,
    /// Where `/proc` and `/sys` are.
    pub host: Host,
}

/// The storm threads on a CPU for a queue of `depth` entries: `ceil(depth x 1.5)`, the design's
/// storm.
#[must_use]
pub fn storm_per_cpu(depth: u16) -> u32 {
    (u32::from(depth) * 3).div_ceil(2)
}

/// The CPUs of a list as `/sys` writes them (`0-3,8,10-11`), ascending; `None` if it is not one.
#[must_use]
pub fn cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let (first, last): (usize, usize) = (first.parse().ok()?, last.parse().ok()?);
        if last < first {
            return None;
        }
        cpus.extend(first..=last);
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

/// The online CPUs of `host`.
///
/// # Errors
/// The list cannot be read or is not one.
pub fn online_cpus(host: &Host) -> Result<Vec<usize>, Error> {
    let path = host.sys.join("devices/system/cpu/online");
    let text = std::fs::read_to_string(&path).map_err(Error::at("read", &path))?;
    cpu_list(&text).ok_or_else(|| Error::Parse {
        path,
        line: 1,
        why: format!("not a CPU list: {}", text.trim()),
    })
}

/// The online CPUs of `host` that this process may run on (its affinity): the CPUs a storm can
/// pin threads to.
///
/// # Errors
/// As [`online_cpus`].
pub fn usable_cpus(host: &Host) -> Result<Vec<usize>, Error> {
    let mut cpus = online_cpus(host)?;
    if let Ok(allowed) = rustix::thread::sched_getaffinity(None) {
        cpus.retain(|&cpu| cpu < rustix::thread::CpuSet::MAX_CPU && allowed.is_set(cpu));
    }
    Ok(cpus)
}

/// The CPU of the other probe: `asked` if given, else the last online CPU that is not
/// `storm_cpu`; both must be online.
///
/// # Errors
/// `storm_cpu` or `asked` is not online, or `asked` is `storm_cpu`.
pub fn other_cpu(
    cpus: &[usize],
    storm_cpu: usize,
    asked: Option<usize>,
) -> Result<Option<usize>, Error> {
    let online = |cpu: usize| {
        cpus.contains(&cpu)
            .then_some(cpu)
            .ok_or_else(|| Error::Usage(format!("CPU {cpu} is not online")))
    };
    online(storm_cpu)?;
    match asked {
        Some(cpu) if cpu == storm_cpu => Err(Error::Usage(format!(
            "the other probe's CPU {cpu} is the storm's"
        ))),
        Some(cpu) => online(cpu).map(Some),
        None => Ok(cpus.iter().rev().copied().find(|&c| c != storm_cpu)),
    }
}

/// A phase to run: its name, the CPU of each storm thread (thread `k` reads cold file `ck`), and
/// each probe's role, CPU and whether the phase storms that CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseSpec {
    /// `idle`, `one-cpu` or `every-cpu`.
    pub name: &'static str,
    /// The CPU of each storm thread.
    pub storm: Vec<usize>,
    /// The CPUs stormed.
    pub stormed_cpus: u32,
    /// The probes: role, CPU, stormed.
    pub probes: Vec<(&'static str, usize, bool)>,
}

/// The three phases of a run of `plan`.
#[must_use]
pub fn phases(plan: &Plan) -> Vec<PhaseSpec> {
    let per = plan.per_cpu as usize;
    let probes = |storm: bool, every: bool| {
        let mut probes = vec![("storm-cpu", plan.storm_cpu, storm)];
        if let Some(cpu) = plan.other_cpu {
            probes.push(("other-cpu", cpu, every));
        }
        probes
    };
    let every: Vec<usize> = plan
        .cpus
        .iter()
        .flat_map(|&cpu| std::iter::repeat_n(cpu, per))
        .collect();
    vec![
        PhaseSpec {
            name: "idle",
            storm: Vec::new(),
            stormed_cpus: 0,
            probes: probes(false, false),
        },
        PhaseSpec {
            name: "one-cpu",
            storm: vec![plan.storm_cpu; per],
            stormed_cpus: u32::from(per > 0),
            probes: probes(per > 0, false),
        },
        PhaseSpec {
            name: "every-cpu",
            storm: every,
            stormed_cpus: if per > 0 { plan.cpus.len() as u32 } else { 0 },
            probes: probes(per > 0, per > 0),
        },
    ]
}

/// Pins the calling thread to `cpu`.
///
/// # Errors
/// The kernel refuses (the CPU is not one of the thread's allowed CPUs, or not there).
pub fn pin(cpu: usize) -> std::io::Result<()> {
    if cpu >= rustix::thread::CpuSet::MAX_CPU {
        return Err(std::io::Error::from_raw_os_error(22));
    }
    let mut set = rustix::thread::CpuSet::new();
    set.set(cpu);
    rustix::thread::sched_setaffinity(None, &set).map_err(Into::into)
}

/// A count that threads add to, and that a thread waits on.
#[derive(Debug, Default)]
struct Gate {
    count: Mutex<usize>,
    changed: Condvar,
}

impl Gate {
    fn arrive(&self) {
        let mut count = self
            .count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count += 1;
        self.changed.notify_all();
    }

    fn wait_for(&self, n: usize) {
        let count = self
            .count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.changed
                .wait_while(count, |c| *c < n)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

/// What the storm threads of a phase share.
#[derive(Debug, Default)]
struct StormShared {
    stop: AtomicBool,
    started: Gate,
    reads: AtomicU64,
    failures: Mutex<BTreeMap<String, u64>>,
}

impl StormShared {
    /// Counts a failure of step `what` (`pin`, `open`, `read`) with `error`.
    fn fail(&self, what: &str, error: &std::io::Error) {
        *self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(format!("{what}: {error}"))
            .or_insert(0) += 1;
    }
}

/// A storm thread: pinned to `cpu`, reads `path` at offsets it has not read until told to stop;
/// arrives at the gate after its first read (or at once if it cannot start).
fn storm_thread(path: &Path, cpu: usize, read: ColdRead, shared: &StormShared) {
    let file = match pin(cpu)
        .map_err(|e| ("pin", e))
        .and_then(|()| File::open(path).map_err(|e| ("open", e)))
    {
        Ok(file) => file,
        Err((what, e)) => {
            shared.fail(what, &e);
            shared.started.arrive();
            return;
        }
    };
    if read == ColdRead::Buffered {
        // Without it the kernel may read ahead past the page asked for; the storm is the same
        // either way, so a refusal changes nothing that matters.
        let _ = rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::Random);
    }
    let mut buf = vec![0u8; READ_BYTES];
    let mut offset = 0u64;
    let mut first = true;
    while !shared.stop.load(Ordering::Relaxed) {
        match file.read_at(&mut buf, offset) {
            Ok(_) => shared.reads.fetch_add(1, Ordering::Relaxed),
            Err(e) => {
                shared.fail("read", &e);
                0
            }
        };
        offset = offset.wrapping_add(STRIDE);
        if first {
            shared.started.arrive();
            first = false;
        }
    }
}

/// A probe thread: pinned to `cpu`, `stat`s `path` until `done` says so (asked with the `stat`s
/// made); the latency of each and the failures.
fn probe_thread(
    path: &Path,
    cpu: usize,
    clock: Clock,
    done: impl Fn(u64) -> bool,
) -> (Histogram, u64) {
    let mut hist = Histogram::default();
    let mut errors = 0;
    if pin(cpu).is_err() {
        return (hist.sealed(), 1);
    }
    let mut made = 0;
    while !done(made) {
        let t = clock.ns();
        match std::fs::metadata(path) {
            Ok(_) => hist.add(clock.ns().saturating_sub(t)),
            Err(_) => errors += 1,
        }
        made += 1;
    }
    (hist.sealed(), errors)
}

/// What a phase reads of the session while it runs.
struct Watch<'a> {
    serving: &'a Serving,
    fs: &'a StormFs<Clock>,
    host: &'a Host,
    connection: u64,
}

/// Runs one phase.
fn run_phase(plan: &Plan, spec: &PhaseSpec, watch: &Watch<'_>, clock: Clock) -> PhaseRecord {
    let shared = StormShared::default();
    let live = AtomicU64::new(spec.probes.len() as u64);
    let record = std::thread::scope(|scope| {
        for (k, &cpu) in spec.storm.iter().enumerate() {
            let (path, shared) = (plan.mountpoint.join(format!("c{k}")), &shared);
            scope.spawn(move || storm_thread(&path, cpu, plan.read, shared));
        }
        shared.started.wait_for(spec.storm.len());
        let reads_before = shared.reads.load(Ordering::Relaxed);
        let begin = clock.ns();
        let probes: Vec<_> = spec
            .probes
            .iter()
            .map(|&(_, cpu, _)| {
                let (path, live) = (plan.mountpoint.join("probe"), &live);
                scope.spawn(move || {
                    let ended = probe_thread(&path, cpu, clock, |made| {
                        (plan.max_probes > 0 && made >= plan.max_probes)
                            || clock.ns().saturating_sub(begin) >= duration_ns(plan.window)
                    });
                    live.fetch_sub(1, Ordering::Release);
                    ended
                })
            })
            .collect();
        let (mut held, mut parked, mut waiting) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            held.push(watch.serving.uring_entries_held());
            parked.push(watch.fs.counts().parked);
            if let Some(w) = watch.host.fuse_waiting(watch.connection) {
                waiting.push(w);
            }
            if live.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::sleep(plan.sample_every);
        }
        let window_ns = clock.ns().saturating_sub(begin);
        let storm_reads = shared.reads.load(Ordering::Relaxed) - reads_before;
        shared.stop.store(true, Ordering::Relaxed);
        let probes = probes
            .into_iter()
            .zip(&spec.probes)
            .map(|(handle, &(role, cpu, stormed))| {
                let (stat, errors) = handle.join().unwrap_or_else(|_| (Histogram::default(), 1));
                ProbeRecord {
                    role: role.to_owned(),
                    cpu,
                    stormed,
                    errors,
                    stat,
                }
            })
            .collect();
        PhaseRecord {
            name: spec.name.to_owned(),
            storm_threads: spec.storm.len() as u32,
            stormed_cpus: spec.stormed_cpus,
            window_ns,
            storm_reads,
            storm_errors: 0,
            storm_failures: BTreeMap::new(),
            probes,
            held: Sampled::of(&held),
            parked: Sampled::of(&parked),
            waiting: (!waiting.is_empty()).then(|| Sampled::of(&waiting)),
        }
    });
    let failures = shared
        .failures
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    PhaseRecord {
        storm_errors: failures.values().sum(),
        storm_failures: failures,
        ..record
    }
}

impl crate::stormfs::Clock for Clock {
    fn now(&self) -> u64 {
        self.ns()
    }
}

fn duration_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// One run: mount, the three phases, unmount.
fn one_run(plan: &Plan, run: u32) -> Result<StormRecord, Error> {
    let clock = Clock::start();
    let config = plan.transport.init_config();
    let max_read = usize::from(config.max_pages) * rustix::param::page_size();
    let cold_files = u64::from(plan.per_cpu) * plan.cpus.len() as u64;
    let fs = Arc::new(StormFs::new(
        cold_files, plan.read, plan.park, clock, max_read,
    ));
    let options = MountOptions {
        allow_other: false,
        subtype: Some(FSNAME.to_owned()),
        ..MountOptions::tree(FSNAME)
    };
    let (device, mount) = Mounter::default()
        .mount(&plan.mountpoint, &options)
        .map_err(Error::of(PROCESS, "mount"))?;
    let detach = Detach::new(mount.clone());
    let (serving, _) = start(device, plan.readers, plan.transport, fs.clone())?;
    let parker = {
        let fs = fs.clone();
        std::thread::spawn(move || fs.park_loop())
    };
    let ran = (|| {
        let connection = crate::host::fuse_connection(&plan.mountpoint)?;
        let environment = plan.host.environment(connection);
        let watch = Watch {
            serving: &serving,
            fs: &fs,
            host: &plan.host,
            connection,
        };
        let phases = phases(plan)
            .iter()
            .map(|spec| run_phase(plan, spec, &watch, clock))
            .collect();
        Ok::<_, Error>((environment, phases))
    })();
    fs.stop();
    let _ = parker.join();
    let (transport, memory) = transport_of(serving.transport());
    let (environment, phases) = ran?;
    mount
        .unmount(false)
        .map_err(Error::of(PROCESS, "unmount"))?;
    detach.disarm();
    serving.join().map_err(Error::of(PROCESS, "serve"))?;
    Ok(StormRecord {
        format: FORMAT,
        label: plan.label.clone(),
        run,
        transport,
        uring_registered_bytes: memory.map(|m| m.bytes),
        uring_depth: memory.map(|m| m.depth),
        uring_queues: memory.map(|m| m.queues),
        readers: plan.readers.get(),
        park_us: u64::try_from(plan.park.as_micros()).unwrap_or(u64::MAX),
        read: plan.read.label().to_owned(),
        per_cpu: plan.per_cpu,
        storm_cpu: plan.storm_cpu,
        other_cpu: plan.other_cpu,
        settings: plan.settings.clone(),
        environment,
        phases,
    })
}

/// Raises this process's limit of open files to its hard limit: a storm on every CPU opens a
/// file per thread (704 on 88 CPUs at the default depth) beside the session's descriptors
/// (three per io_uring queue), past the usual soft limit of 1024. Returns the soft limit after.
///
/// # Errors
/// The kernel refuses the new limit.
pub fn raise_open_files() -> std::io::Result<Option<u64>> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: limit.maximum,
            maximum: limit.maximum,
        },
    )?;
    Ok(getrlimit(Resource::Nofile).current)
}

/// The runs of `plan`: each run's record is appended to `out` as a line of JSON, and a line
/// about it written to `log`. Returns the records.
///
/// # Errors
/// A run cannot mount, start its session, or unmount, or `out` cannot be written.
pub fn run(
    plan: &Plan,
    mut out: impl Write,
    log: &mut dyn Write,
) -> Result<Vec<StormRecord>, Error> {
    raise_open_files().map_err(Error::of(PROCESS, "raise the limit of open files"))?;
    let mut records = Vec::new();
    for n in 0..plan.runs {
        let record = one_run(plan, n)?;
        // A record always encodes: it holds numbers, strings and maps of strings.
        let line = serde_json::to_string(&record).unwrap_or_default();
        writeln!(out, "{line}")
            .and_then(|()| out.flush())
            .map_err(Error::of(PROCESS, "write the record"))?;
        let _ = writeln!(
            log,
            "storm {} run {n}: {}, {} phases",
            plan.label,
            record.transport,
            record.phases.len()
        );
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
#[path = "storm_tests.rs"]
mod tests;
