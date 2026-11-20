//! `fsbench dstate-report`'s input: D state and FUSE waiting on a build node under a real build,
//! from what the node's own read-only tools printed (`docs/design/fetch-cache-serve.md`, 17.1,
//! "D-state and FUSE waiting under a real komira build").
//!
//! The build runs as the farm's jobs, on nodes that serve other work, so nothing of this harness
//! runs there: a sampler of standard tools, run as root over ssh, prints once per tick what the
//! kernel says of the threads that matter, and this module reads that text afterwards
//! ([`parse`]) into one [`NodeRun`] per node and run. The text is sections under a line that
//! names them:
//!
//! ```text
//! @node <kernel> <cpus>
//! #mounts
//! <major:minor> <source>                 (the FUSE mounts, from mountinfo)
//! @tick <unix nanoseconds>
//! #load
//! <the line of /proc/loadavg>
//! #waiting
//! /sys/fs/fuse/connections/<n>/waiting:<requests>
//! #ps
//! <tid> <state> <wchan> <owner>          (owner: `job`, `srv` (the FUSE server) or `-`)
//! #count
//! <threads of the jobs> <jobs>
//! #stack
//! /proc/<pid>/task/<tid>/stack:[<0>] <function>+<offset>/<size>
//! @end <unix nanoseconds>
//! ```
//!
//! `#ps` holds one row per thread in D state, per running thread of a job, and per thread asleep
//! in `request_wait_answer`: a thread of a job that is in none of these sleeps outside FUSE, and
//! `#count` counts every thread of the jobs (a job is a cgroup of the farm's job agent). `#stack`
//! holds only the stack lines that name a FUSE function, a folio wait or a page fault, read just
//! after `#ps`; a thread's row and its stack are two looks a few milliseconds apart, so a thread
//! is classified by its row's state and `wchan` and by whatever its stack says, and a stack line
//! of a thread with no row is not counted.
//!
//! A thread *waits on FUSE* when its `wchan` is `request_wait_answer` or its stack holds a FUSE
//! function other than the server's own (`fuse_dev_*`), split three ways ([`FuseWait`]): a
//! request the kernel sent and waits for the answer to (`request_wait_answer`: LOOKUP, GETATTR,
//! OPEN, a direct READ), a page of a FUSE file the kernel reads (a FUSE function and a folio
//! wait: a buffered READ in flight), and the rest. `fuse_d_state_threads` (5.3) is the threads in
//! D state that wait on FUSE. A page fault of a mapped file has no FUSE function on its stack
//! whatever its file system, so a thread in D state in `filemap_fault` with no FUSE function is
//! counted apart ([`Tick::d_fault`]).
//!
//! A sampler stopped mid-way leaves a file with no `@end` line, whose last tick may be cut: that
//! tick is left out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Error;

/// The source of the FUSE mounts whose connections are read (`tierstore fs`'s).
pub const MOUNT_SOURCE: &str = "tierstore-fs";

/// The `wchan` of a thread that waits for the answer to a FUSE request.
const REQUEST_WAIT: &str = "request_wait_answer";

/// One node over one run of a build.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeRun {
    /// The run's label (`run1`).
    pub run: String,
    /// The node's label (`n1`): not its host name.
    pub node: String,
    /// `uname -r` of the node.
    pub kernel: String,
    /// The CPUs online.
    pub cpus: u32,
    /// The connections of the FUSE mounts read (`/sys/fs/fuse/connections/<n>`).
    pub connections: Vec<u32>,
    /// How long the build ran, milliseconds.
    pub build_ms: u64,
    /// The ticks, in order; [`Tick::t_ms`] is from the start of the build.
    pub ticks: Vec<Tick>,
}

/// One tick of the sampler on one node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tick {
    /// Milliseconds from the start of the build (negative before it).
    pub t_ms: i64,
    /// The one-minute load average.
    pub load1: f64,
    /// The requests waiting on the connections read, summed; `None` when none was listed.
    pub waiting: Option<u64>,
    /// The jobs with a thread.
    pub jobs: u32,
    /// The threads of the jobs.
    pub job_threads: u32,
    /// Of those, running (`R`).
    pub job_running: u32,
    /// Of those, in D state.
    pub job_d: u32,
    /// Of those, neither running nor in D state (asleep, `S`) and waiting on FUSE.
    pub job_fuse_s: u32,
    /// Of those, waiting on FUSE for the answer to a request, in S or D.
    pub job_fuse_request: u32,
    /// Of those, waiting on FUSE for a page of a file being read, in S or D.
    pub job_fuse_page: u32,
    /// Of those, waiting on FUSE some other way, in S or D.
    pub job_fuse_other: u32,
    /// Every thread of the node in D state.
    pub d: u32,
    /// The threads in D state that wait on FUSE (`fuse_d_state_threads`).
    pub fuse_d: u32,
    /// The threads in D state in a page fault with no FUSE function on the stack.
    pub d_fault: u32,
}

impl Tick {
    /// The job threads doing or waiting for work: running, in D state, or asleep on FUSE.
    #[must_use]
    pub fn job_active(&self) -> u32 {
        self.job_running + self.job_d + self.job_fuse_s
    }

    /// The job threads waiting on FUSE, in S or D.
    #[must_use]
    pub fn job_fuse(&self) -> u32 {
        self.job_fuse_request + self.job_fuse_page + self.job_fuse_other
    }
}

/// How a thread waits on FUSE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuseWait {
    /// The answer to a request (`request_wait_answer`).
    Request,
    /// A page of a FUSE file (a FUSE function and a folio wait).
    Page,
    /// Another FUSE function.
    Other,
}

/// What a thread's `wchan` and stack lines named.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stack {
    request: bool,
    fuse: bool,
    server: bool,
    folio: bool,
    fault: bool,
}

impl Stack {
    /// Takes in one function of the stack (or the `wchan`).
    fn add(&mut self, function: &str) {
        if function == REQUEST_WAIT {
            self.request = true;
        } else if function.starts_with("fuse_dev_") {
            self.server = true;
        } else if function.starts_with("fuse_") || function.starts_with("__fuse_") {
            self.fuse = true;
        } else if function.starts_with("folio_wait") {
            self.folio = true;
        } else if function == "filemap_fault" {
            self.fault = true;
        }
    }

    /// How the thread waits on FUSE, if it does.
    fn wait(self) -> Option<FuseWait> {
        if self.server {
            None
        } else if self.request {
            Some(FuseWait::Request)
        } else if self.fuse && self.folio {
            Some(FuseWait::Page)
        } else if self.fuse {
            Some(FuseWait::Other)
        } else {
            None
        }
    }
}

/// One `#ps` row.
#[derive(Clone, Debug)]
struct Row {
    state: char,
    wchan: String,
    job: bool,
}

/// The section a line is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Head,
    Mounts,
    Load,
    Waiting,
    Ps,
    Count,
    Stack,
}

/// One tick as read, before it is counted.
#[derive(Debug, Default)]
struct RawTick {
    at_ns: u64,
    load1: f64,
    waiting: BTreeMap<u32, u64>,
    rows: BTreeMap<u32, Row>,
    count: Option<(u32, u32)>,
    stacks: BTreeMap<u32, Stack>,
}

impl RawTick {
    /// The [`Tick`] of this tick, at `t_ms` from the build's start, reading `connections`.
    fn count(&self, t_ms: i64, connections: &BTreeSet<u32>) -> Tick {
        let read: Vec<u64> = self
            .waiting
            .iter()
            .filter(|&(c, _)| connections.contains(c))
            .map(|(_, w)| *w)
            .collect();
        let (job_threads, jobs) = self.count.unwrap_or_default();
        let mut tick = Tick {
            t_ms,
            load1: self.load1,
            waiting: (!read.is_empty()).then(|| read.iter().sum()),
            jobs,
            job_threads,
            ..Tick::default()
        };
        for (tid, row) in &self.rows {
            let mut stack = self.stacks.get(tid).copied().unwrap_or_default();
            stack.add(&row.wchan);
            let wait = stack.wait();
            if row.state == 'D' {
                tick.d += 1;
                if wait.is_some() {
                    tick.fuse_d += 1;
                } else if stack.fault {
                    tick.d_fault += 1;
                }
            }
            if !row.job {
                continue;
            }
            match (row.state, wait) {
                ('R', _) => {
                    tick.job_running += 1;
                    continue;
                }
                ('D', _) => tick.job_d += 1,
                (_, Some(_)) => tick.job_fuse_s += 1,
                (_, None) => {}
            }
            match wait {
                Some(FuseWait::Request) => tick.job_fuse_request += 1,
                Some(FuseWait::Page) => tick.job_fuse_page += 1,
                Some(FuseWait::Other) => tick.job_fuse_other += 1,
                None => {}
            }
        }
        tick
    }
}

/// What the head of the sampler's text said.
#[derive(Debug, Default)]
struct Head {
    kernel: String,
    cpus: u32,
    connections: BTreeSet<u32>,
}

/// The parse of a sampler's text, line by line.
struct Parser<'a> {
    path: &'a Path,
    head: Head,
    section: Section,
    ticks: Vec<RawTick>,
    ended: bool,
}

impl Parser<'_> {
    fn bad(&self, line: usize, why: impl Into<String>) -> Error {
        Error::Parse {
            path: self.path.to_path_buf(),
            line,
            why: why.into(),
        }
    }

    /// The tick being read, or an error at `line` when there is none.
    fn tick(&mut self, line: usize) -> Result<&mut RawTick, Error> {
        if self.ended {
            return Err(self.bad(line, "a sample after `@end`"));
        }
        match self.ticks.last_mut() {
            Some(tick) => Ok(tick),
            None => Err(Error::Parse {
                path: self.path.to_path_buf(),
                line,
                why: "a sample outside a tick".to_owned(),
            }),
        }
    }

    fn line(&mut self, n: usize, line: &str) -> Result<(), Error> {
        if let Some(rest) = line.strip_prefix('@') {
            return self.marker(n, rest);
        }
        if let Some(name) = line.strip_prefix('#') {
            self.section = match name {
                "mounts" => Section::Mounts,
                "load" => Section::Load,
                "waiting" => Section::Waiting,
                "ps" => Section::Ps,
                "count" => Section::Count,
                "stack" => Section::Stack,
                other => return Err(self.bad(n, format!("an unknown section `{other}`"))),
            };
            return Ok(());
        }
        match self.section {
            Section::Head => Err(self.bad(n, "a line before any section")),
            Section::Mounts => self.mount(n, line),
            Section::Load => {
                let load = line.split_whitespace().next().and_then(|f| f.parse().ok());
                let load = load.ok_or_else(|| self.bad(n, "not a load average"))?;
                self.tick(n)?.load1 = load;
                Ok(())
            }
            Section::Waiting => {
                let parsed = waiting(line);
                let (connection, count) =
                    parsed.ok_or_else(|| self.bad(n, "not a connection's waiting count"))?;
                self.tick(n)?.waiting.insert(connection, count);
                Ok(())
            }
            Section::Ps => {
                let parsed = row(line);
                let (tid, row) = parsed.ok_or_else(|| {
                    self.bad(n, "a ps row wants a tid, a state, a wchan and an owner")
                })?;
                self.tick(n)?.rows.insert(tid, row);
                Ok(())
            }
            Section::Count => {
                let mut fields = line.split_whitespace().map(|f| f.parse::<u32>().ok());
                let (Some(Some(threads)), Some(Some(jobs)), None) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    return Err(self.bad(n, "a count wants the jobs' threads and the jobs"));
                };
                self.tick(n)?.count = Some((threads, jobs));
                Ok(())
            }
            Section::Stack => {
                let parsed = stack(line);
                let (tid, function) = parsed.ok_or_else(|| self.bad(n, "not a stack line"))?;
                self.tick(n)?.stacks.entry(tid).or_default().add(function);
                Ok(())
            }
        }
    }

    fn marker(&mut self, n: usize, rest: &str) -> Result<(), Error> {
        let fields: Vec<&str> = rest.split_whitespace().collect();
        match fields[..] {
            ["node", kernel, cpus] => {
                let cpus = cpus
                    .parse::<u32>()
                    .map_err(|_| self.bad(n, "not a count of CPUs"))?;
                self.head.kernel = kernel.to_owned();
                self.head.cpus = cpus;
            }
            [word @ ("tick" | "end"), at] => {
                let at = at
                    .parse::<u64>()
                    .map_err(|_| self.bad(n, format!("`@{word}` wants a time")))?;
                if self.ended {
                    return Err(self.bad(n, format!("`@{word}` after `@end`")));
                }
                if word == "end" {
                    self.ended = true;
                } else {
                    self.ticks.push(RawTick {
                        at_ns: at,
                        ..RawTick::default()
                    });
                }
            }
            _ => return Err(self.bad(n, format!("an unknown marker `@{rest}`"))),
        }
        self.section = Section::Head;
        Ok(())
    }

    fn mount(&mut self, n: usize, line: &str) -> Result<(), Error> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [device, source] = fields[..] else {
            return Err(self.bad(n, "a mount wants a device and a source"));
        };
        let minor = device.split_once(':').and_then(|(_, m)| m.parse().ok());
        let minor = minor.ok_or_else(|| self.bad(n, "not a device `major:minor`"))?;
        if source == MOUNT_SOURCE {
            self.head.connections.insert(minor);
        }
        Ok(())
    }
}

/// `/sys/fs/fuse/connections/<n>/waiting:<count>` as the connection and the count.
fn waiting(line: &str) -> Option<(u32, u64)> {
    let rest = line.strip_prefix("/sys/fs/fuse/connections/")?;
    let (connection, count) = rest.split_once("/waiting:")?;
    Some((connection.parse().ok()?, count.parse().ok()?))
}

/// `<tid> <state> <wchan> <owner>` as the thread and its row.
fn row(line: &str) -> Option<(u32, Row)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [tid, state, wchan, owner] = fields[..] else {
        return None;
    };
    let row = Row {
        state: state.chars().next().unwrap_or('?'),
        wchan: wchan.to_owned(),
        job: owner == "job",
    };
    Some((tid.parse().ok()?, row))
}

/// `/proc/<pid>/task/<tid>/stack:[<0>] <function>+<offset>` as the thread and the function.
fn stack(line: &str) -> Option<(u32, &str)> {
    let rest = line.strip_prefix("/proc/")?;
    let (_pid, rest) = rest.split_once("/task/")?;
    let (tid, rest) = rest.split_once("/stack:")?;
    let (_, frame) = rest.split_once("] ")?;
    let function = frame.split_once('+').map_or(frame, |(f, _)| f);
    Some((tid.parse().ok()?, function))
}

/// The milliseconds from `from` to `to`, negative when `to` is earlier.
fn ms_between(from: u64, to: u64) -> i64 {
    let ms = |ns: u64| i64::try_from(ns / 1_000_000).unwrap_or(i64::MAX);
    ms(to) - ms(from)
}

/// The [`NodeRun`] of node `node` in run `run`, from its sampler's text `text` (read from `path`,
/// which errors name), for a build from `start_ns` to `end_ns` (Unix nanoseconds).
///
/// # Errors
/// A line is not what the sampler prints (the error names it), the text has no `@node` line, or
/// the build ends before it starts.
pub fn parse(
    path: &Path,
    text: &str,
    (run, node): (&str, &str),
    (start_ns, end_ns): (u64, u64),
) -> Result<NodeRun, Error> {
    if end_ns < start_ns {
        return Err(Error::Usage(format!(
            "{}: the build ends before it starts",
            path.display()
        )));
    }
    let mut parser = Parser {
        path,
        head: Head::default(),
        section: Section::Head,
        ticks: Vec::new(),
        ended: false,
    };
    for (n, line) in text.lines().enumerate() {
        if !line.trim().is_empty() {
            parser.line(n + 1, line)?;
        }
    }
    if parser.head.kernel.is_empty() {
        return Err(parser.bad(1, "no `@node` line"));
    }
    if !parser.ended {
        parser.ticks.pop();
    }
    let connections = &parser.head.connections;
    let ticks = parser
        .ticks
        .iter()
        .map(|t| t.count(ms_between(start_ns, t.at_ns), connections))
        .collect();
    Ok(NodeRun {
        run: run.to_owned(),
        node: node.to_owned(),
        kernel: parser.head.kernel,
        cpus: parser.head.cpus,
        connections: connections.iter().copied().collect(),
        build_ms: (end_ns - start_ns) / 1_000_000,
        ticks,
    })
}

/// The index of a real-build measurement: its runs, each with the sampler's text of every node.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    /// The runs, in order.
    pub runs: Vec<RunEntry>,
}

/// One run of the index.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEntry {
    /// The run's label.
    pub run: String,
    /// When the build started, Unix nanoseconds.
    pub build_start_ns: u64,
    /// When the build ended, Unix nanoseconds.
    pub build_end_ns: u64,
    /// What the operator recorded (`targets`, `commands`, `server`, ...).
    pub settings: BTreeMap<String, String>,
    /// The nodes: a label and the sampler's text, relative to the index, `.zst` when compressed.
    pub nodes: Vec<NodeEntry>,
}

/// One node of a run of the index.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeEntry {
    /// The node's label (not its host name).
    pub node: String,
    /// The sampler's text, relative to the index.
    pub raw: PathBuf,
}

/// The text of the file at `path`, decompressed when its name ends in `.zst`.
fn text_of(path: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(Error::at("read", path))?;
    let bytes = if path.extension().is_some_and(|e| e == "zst") {
        let mut out = Vec::new();
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes.as_slice())
            .map_err(|e| Error::io("decompress", path, std::io::Error::other(format!("{e:?}"))))?;
        std::io::Read::read_to_end(&mut decoder, &mut out)
            .map_err(Error::at("decompress", path))?;
        out
    } else {
        bytes
    };
    String::from_utf8(bytes).map_err(|e| {
        Error::io(
            "read",
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        )
    })
}

/// The runs of the index at `path`, every node's text read and parsed, with each run's settings.
///
/// # Errors
/// The index or a node's text cannot be read or is not what it should be (the error names it).
pub fn load(path: &Path) -> Result<Vec<(RunEntry, Vec<NodeRun>)>, Error> {
    let text = text_of(path)?;
    let index: Index = serde_json::from_str(&text).map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        line: e.line(),
        why: format!("not an index of runs: {e}"),
    })?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for entry in index.runs {
        let mut nodes = Vec::new();
        for node in &entry.nodes {
            let raw = dir.join(&node.raw);
            let parsed = parse(
                &raw,
                &text_of(&raw)?,
                (&entry.run, &node.node),
                (entry.build_start_ns, entry.build_end_ns),
            )?;
            nodes.push(parsed);
        }
        out.push((entry, nodes));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "dstate_tests.rs"]
mod tests;
