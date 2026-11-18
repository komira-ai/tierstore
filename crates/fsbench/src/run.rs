//! `fsbench run`: the runs of one variant in one regime, each written to the results file as it
//! ends.
//!
//! A run starts its readers and waits until each has read the list, starts the burners, reads
//! the CPU times, tells every reader to go, and samples D state and the FUSE requests waiting
//! once per interval until the last reader answers. It reads the CPU times again and stops the
//! burners. Then the first `throughput_readers` readers read the largest files whole, cold and
//! then warm, with no burner running. The readers are stopped (their input is closed) and the
//! record is written. Every run starts new processes, so no run inherits a reader's state.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::Error;
use crate::clock::Clock;
use crate::hist::Histogram;
use crate::host::{self, CgroupCpu, Host, NodeCpu, ProcessCpu};
use crate::list::{self, FileList};
use crate::reader::{Answer, CycleCounts, Go, WholeCounts};
use crate::record::{Cpu, Cycles, FORMAT, Input, RunRecord, Samples, Server, Throughput};

/// What to run.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The `fsbench` executable the readers and burners run.
    pub exe: PathBuf,
    /// The file list.
    pub list: PathBuf,
    /// The directory the list's paths are below.
    pub tree: PathBuf,
    /// The variant's label.
    pub variant: String,
    /// The regime's label.
    pub regime: String,
    /// How many runs.
    pub runs: u32,
    /// The reader processes of the cycles.
    pub readers: u32,
    /// The CPU burners.
    pub burners: u32,
    /// How long the cycles last, in milliseconds.
    pub cycle_ms: u64,
    /// The most cycles per reader (0: no limit).
    pub max_cycles: u64,
    /// The bytes a cycle reads.
    pub read_bytes: u32,
    /// The bytes of the largest files that are read whole.
    pub throughput_budget: u64,
    /// The readers of the whole-file reads, the first of the readers.
    pub throughput_readers: u32,
    /// The interval of the samples.
    pub sample_every: Duration,
    /// The process that serves the mount.
    pub server_pid: u32,
    /// The user and group the readers run as (the harness's own when `None`).
    pub as_user: Option<(u32, u32)>,
    /// What the operator declares of the server.
    pub settings: BTreeMap<String, String>,
    /// Where `/proc` and `/sys` are.
    pub host: Host,
}

/// A child process and its two pipes.
struct Kid {
    name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
}

/// The children of a phase: killed and waited for when dropped, so an error leaves none behind.
struct Kids(Vec<Kid>);

impl Drop for Kids {
    fn drop(&mut self) {
        for kid in &mut self.0 {
            kid.stdin = None;
            let _ = kid.child.kill();
            let _ = kid.child.wait();
        }
    }
}

impl Kids {
    /// Starts `count` children named `name <i>` of `exe` with `args`.
    fn start(
        plan: &Plan,
        name: &str,
        count: u32,
        args: &[&std::ffi::OsStr],
    ) -> Result<Self, Error> {
        let mut kids = Self(Vec::new());
        for i in 0..count {
            let name = format!("{name} {i}");
            let mut command = Command::new(&plan.exe);
            command
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            if let Some((uid, gid)) = plan.as_user {
                command.uid(uid).gid(gid);
            }
            let start = format!("start {}", plan.exe.display());
            let mut child = command.spawn().map_err(Error::of(&name, &start))?;
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().map(BufReader::new);
            kids.0.push(Kid {
                name,
                child,
                stdin,
                stdout,
            });
        }
        Ok(kids)
    }

    /// Writes `go` to child `i`.
    fn send(&mut self, i: usize, go: &Go) -> Result<(), Error> {
        let kid = &mut self.0[i];
        // A go always encodes: it holds numbers alone.
        let line = serde_json::to_string(go).unwrap_or_default();
        kid.stdin
            .as_mut()
            .map_or(Err(std::io::ErrorKind::BrokenPipe.into()), |stdin| {
                writeln!(stdin, "{line}").and_then(|()| stdin.flush())
            })
            .map_err(Error::of(&kid.name, "send the go"))
    }

    /// The next answer of child `i`.
    fn answer(&mut self, i: usize) -> Result<Answer, Error> {
        let kid = &mut self.0[i];
        let mut line = String::new();
        let read = kid
            .stdout
            .as_mut()
            .map_or(Ok(0), |stdout| stdout.read_line(&mut line))
            .map_err(Error::of(&kid.name, "read the answer"))?;
        if read == 0 {
            return Err(Error::child(&kid.name, "ended without its answer"));
        }
        let what = format!("not an answer: {}", line.trim_end());
        serde_json::from_str(&line).map_err(Error::of(&kid.name, &what))
    }

    /// Closes every child's input and waits for it to end; each must end with success.
    fn finish(mut self) -> Result<(), Error> {
        let kids = std::mem::take(&mut self.0);
        for mut kid in kids {
            kid.stdin = None;
            let status = kid.child.wait().map_err(Error::of(&kid.name, "wait"))?;
            ended(&kid.name, status)?;
        }
        Ok(())
    }
}

/// Success when `status` is, else an error that names the child and how it ended.
fn ended(name: &str, status: std::process::ExitStatus) -> Result<(), Error> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::child(name, format!("ended with {status}")))
    }
}

/// The ready of child `name`.
fn ready(name: &str, answer: Answer) -> Result<(), Error> {
    match answer {
        Answer::Ready => Ok(()),
        other => Err(Error::child(
            name,
            format!("answered {other:?} before its ready"),
        )),
    }
}

/// The counts of child `name`'s cycles.
fn cycles_of(name: &str, answer: Answer) -> Result<CycleCounts, Error> {
    match answer {
        Answer::Cycles(counts) => Ok(*counts),
        other => Err(Error::child(
            name,
            format!("answered {other:?} to a cycles go"),
        )),
    }
}

/// The counts of child `name`'s whole-file reads.
fn whole_of(name: &str, answer: Answer) -> Result<WholeCounts, Error> {
    match answer {
        Answer::Whole(counts) => Ok(counts),
        other => Err(Error::child(
            name,
            format!("answered {other:?} to a whole-file go"),
        )),
    }
}

/// Sets its flag when dropped: the sampler stops however the phase ends.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The CPU times at one moment.
struct CpuAt {
    server: ProcessCpu,
    cgroup: Option<CgroupCpu>,
    node: NodeCpu,
}

impl CpuAt {
    fn read(plan: &Plan) -> Result<Self, Error> {
        Ok(Self {
            server: plan.host.process_cpu(plan.server_pid)?,
            cgroup: plan.host.cgroup_cpu(plan.server_pid),
            node: plan.host.node_cpu()?,
        })
    }
}

/// The CPU spent from `from` to `to` over `cycles` cycles.
fn cpu_between(from: &CpuAt, to: &CpuAt, cycles: u64) -> Cpu {
    let per_1k = |ms: f64| ms * 1000.0 / cycles.max(1) as f64;
    let server = ProcessCpu {
        user_ms: to.server.user_ms.saturating_sub(from.server.user_ms),
        system_ms: to.server.system_ms.saturating_sub(from.server.system_ms),
    };
    let cgroup = from.cgroup.zip(to.cgroup).map(|(a, b)| CgroupCpu {
        user_us: b.user_us.saturating_sub(a.user_us),
        system_us: b.system_us.saturating_sub(a.system_us),
    });
    let node = NodeCpu {
        user_ms: to.node.user_ms.saturating_sub(from.node.user_ms),
        system_ms: to.node.system_ms.saturating_sub(from.node.system_ms),
        busy_ms: to.node.busy_ms.saturating_sub(from.node.busy_ms),
    };
    Cpu {
        server,
        cgroup,
        node,
        server_ms_per_1k: per_1k((server.user_ms + server.system_ms) as f64),
        cgroup_system_ms_per_1k: cgroup.map(|c| per_1k(c.system_us as f64 / 1000.0)),
        node_busy_ms_per_1k: per_1k(node.busy_ms as f64),
    }
}

/// The samples of D state and of the requests waiting on `connection`, taken every `every` until
/// `done` says so after a sample (it is asked with the samples taken; one at least is taken).
fn sample(host: &Host, connection: u64, every: Duration, done: &dyn Fn(usize) -> bool) -> Samples {
    let (mut d, mut waiting) = (Vec::new(), Vec::new());
    loop {
        d.push(host.d_state());
        waiting.extend(host.fuse_waiting(connection));
        if done(d.len()) {
            break;
        }
        std::thread::sleep(every);
    }
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len().max(1) as f64;
    let d_values: Vec<f64> = d.iter().map(|&v| f64::from(v)).collect();
    let w_values: Vec<f64> = waiting.iter().map(|&v| v as f64).collect();
    let listed = waiting.len() == d.len();
    Samples {
        count: d.len() as u64,
        d_state_mean: mean(&d_values),
        d_state_max: d.iter().copied().max().unwrap_or(0),
        waiting_mean: listed.then(|| mean(&w_values)),
        waiting_max: waiting.iter().copied().max().filter(|_| listed),
    }
}

/// The cycles of the readers, added up.
fn merge(all: Vec<CycleCounts>, wall_ns: u64) -> Cycles {
    let mut total = CycleCounts::default();
    for counts in all {
        total.count += counts.count;
        total.errors += counts.errors;
        total.first_error = total.first_error.or(counts.first_error);
        for (into, from) in [
            (&mut total.stat, &counts.stat),
            (&mut total.open, &counts.open),
            (&mut total.read, &counts.read),
            (&mut total.close, &counts.close),
        ] {
            into.merge(from);
        }
    }
    let seal = |h: Histogram| h.sealed();
    Cycles {
        count: total.count,
        errors: total.errors,
        first_error: total.first_error,
        wall_ns,
        per_s: total.count as f64 * 1e9 / wall_ns.max(1) as f64,
        stat: seal(total.stat),
        open: seal(total.open),
        read: seal(total.read),
        close: seal(total.close),
    }
}

/// The whole-file reads of the readers, added up.
fn throughput(all: &[WholeCounts], wall_ns: u64) -> Throughput {
    let (files, bytes, errors) = all.iter().fold((0, 0, 0), |(f, b, e), c| {
        (f + c.files, b + c.bytes, e + c.errors)
    });
    Throughput {
        files,
        bytes,
        errors,
        wall_ns,
        mb_per_s: bytes as f64 * 1e3 / wall_ns.max(1) as f64,
    }
}

/// One whole-file phase over the first `throughput_readers` readers.
fn whole_phase(plan: &Plan, readers: &mut Kids, cold: bool) -> Result<Throughput, Error> {
    let clock = Clock::start();
    let of = plan.throughput_readers;
    for index in 0..of {
        let go = Go::Whole {
            index,
            of,
            budget: plan.throughput_budget,
            cold,
        };
        readers.send(index as usize, &go)?;
    }
    let mut all = Vec::new();
    for index in 0..of as usize {
        let answer = readers.answer(index)?;
        all.push(whole_of(&readers.0[index].name, answer)?);
    }
    Ok(throughput(&all, clock.ns()))
}

/// One run, number `run`.
fn one(plan: &Plan, list: &FileList, connection: u64, run: u32) -> Result<RunRecord, Error> {
    let env = plan.host.environment(connection);
    let args = [
        "reader".as_ref(),
        "--list".as_ref(),
        plan.list.as_os_str(),
        "--tree".as_ref(),
        plan.tree.as_os_str(),
    ];
    let mut readers = Kids::start(plan, "reader", plan.readers, &args)?;
    for i in 0..plan.readers as usize {
        let answer = readers.answer(i)?;
        ready(&readers.0[i].name, answer)?;
    }
    let burners = Kids::start(plan, "burner", plan.burners, &["burn".as_ref()])?;
    let before = CpuAt::read(plan)?;
    let clock = Clock::start();
    for index in 0..plan.readers {
        let go = Go::Cycles {
            index,
            of: plan.readers,
            ns: plan.cycle_ms.saturating_mul(1_000_000),
            max_cycles: plan.max_cycles,
            read_bytes: plan.read_bytes,
        };
        readers.send(index as usize, &go)?;
    }
    let stop = StopOnDrop(Arc::new(AtomicBool::new(false)));
    let sampler = {
        let (host, every, stop) = (plan.host.clone(), plan.sample_every, Arc::clone(&stop.0));
        std::thread::spawn(move || {
            sample(&host, connection, every, &|_| stop.load(Ordering::Relaxed))
        })
    };
    let mut answers = Vec::new();
    for i in 0..plan.readers as usize {
        let answer = readers.answer(i)?;
        answers.push(cycles_of(&readers.0[i].name, answer)?);
    }
    let wall_ns = clock.ns();
    // Read when the cycles end, before the sampler is waited for (it may be in its sleep).
    let after = CpuAt::read(plan)?;
    drop(stop);
    // The sampler does not panic; if it did, the record would show no sample (`count` 0).
    let samples = sampler.join().unwrap_or_default();
    burners.finish()?;
    let cycles = merge(answers, wall_ns);
    let cpu = cpu_between(&before, &after, cycles.count);
    let cold = whole_phase(plan, &mut readers, true)?;
    let warm = whole_phase(plan, &mut readers, false)?;
    readers.finish()?;
    Ok(RunRecord {
        format: FORMAT,
        variant: plan.variant.clone(),
        regime: plan.regime.clone(),
        run,
        settings: plan.settings.clone(),
        input: Input {
            list_sha256: list.sha256.clone(),
            files: list.files.len() as u64,
            bytes: list.bytes(),
            readers: plan.readers,
            burners: plan.burners,
            cycle_ms: plan.cycle_ms,
            max_cycles: plan.max_cycles,
            read_bytes: plan.read_bytes,
            throughput_budget: plan.throughput_budget,
            throughput_readers: plan.throughput_readers,
        },
        env,
        server: Server {
            pid: plan.server_pid,
            exe_sha256: host::file_sha256(
                &plan.host.proc.join(plan.server_pid.to_string()).join("exe"),
            ),
        },
        cycles,
        cold,
        warm,
        cpu,
        samples,
    })
}

/// A line a person reads of a run that ended.
fn summary(record: &RunRecord) -> String {
    let ms = |h: &Histogram, q| h.quantile(q) as f64 / 1e6;
    format!(
        "fsbench: {} {} run {}: {:.0} cycles/s ({} errors), open p50 {:.2} ms p99 {:.2} ms, \
         cold {:.0} MB/s, warm {:.0} MB/s, server {:.1} ms per 1k cycles, D {:.0}",
        record.variant,
        record.regime,
        record.run,
        record.cycles.per_s,
        record.cycles.errors,
        ms(&record.cycles.open, 0.5),
        ms(&record.cycles.open, 0.99),
        record.cold.mb_per_s,
        record.warm.mb_per_s,
        record.cpu.server_ms_per_1k,
        record.samples.d_state_mean,
    )
}

/// Refuses a plan that cannot be run: no run, no reader, no whole-file reader, or more whole-file
/// readers than readers.
fn check(plan: &Plan) -> Result<(), Error> {
    if plan.runs == 0
        || plan.readers == 0
        || plan.throughput_readers == 0
        || plan.throughput_readers > plan.readers
    {
        return Err(Error::Usage(
            "a run needs at least one run and one reader, and from one whole-file reader to as many as there are readers"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Runs `plan`: each run's record is written to `out` as one line as soon as it ends, and a
/// line about it to `log`. Returns the records.
///
/// # Errors
/// A plan that cannot be run (no reader, more whole-file readers than readers, no run), a list
/// that cannot be read, a child that fails, or a record that cannot be written.
pub fn run(plan: &Plan, mut out: impl Write, mut log: impl Write) -> Result<Vec<RunRecord>, Error> {
    check(plan)?;
    let list = list::read(&plan.list)?;
    let connection = host::fuse_connection(&plan.tree)?;
    let mut records = Vec::new();
    for run in 0..plan.runs {
        let record = one(plan, &list, connection, run)?;
        // A record always encodes: every map of it has string keys.
        let line = serde_json::to_string(&record).unwrap_or_default();
        writeln!(out, "{line}")
            .and_then(|()| out.flush())
            .map_err(Error::of("results", "write a record"))?;
        let _ = writeln!(log, "{}", summary(&record));
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
