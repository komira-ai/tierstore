//! What the file system's core tells the cache of the cache files it holds open, for the cache's
//! `/metricz`: how many it holds now, and how many opens found no descriptor.
//!
//! The core shares one descriptor per cache file (a blob or a pack) across all its open handles.
//! At each look for idle trees (every quarter of `fs.tree_idle_secs`, at most a minute apart) it
//! reports, by calling the cache in this process (`treefs/in_process.rs`), a [`Descriptors`]:
//!
//! - `mount`: the id of the mount, the name it uses in its pins;
//! - `process`: the id of the mount process (its pid);
//! - `held`: the cache files the mount process holds open now;
//! - `short`: the opens of files of the mount that failed for lack of descriptors (`EMFILE`,
//!   `ENFILE`) since that process started: a running total, so that a report lost (which a
//!   process short of descriptors is likely to lose) loses no failure the next one carries.
//!
//! The node keeps, for each mount, the last `held` (the gauge `tierstore_fs_cache_descriptors`)
//! and the counter `tierstore_fs_open_descriptor_failures_total`, which grows by what each
//! process's total grew by since its report before, and by the whole total of a process it has
//! not seen (or whose total went down: a new process given the same id), so it goes on counting
//! across restarts and hand-overs of the mount process. The last total of the [`PROCESSES`] latest
//! processes of a mount is kept, so the two processes of a hand-over, reporting in turns, are
//! counted apart. It keeps at most [`MOST_MOUNTS`] mounts: a report of another mount, when that
//! many are kept, is counted nowhere, so made-up mount ids cannot grow the
//! node's memory.
//!
//! The report is advice, like the others of the core: it reaches the cache's counts at once, on
//! the caller's thread, and a lost one loses no failure the next carries.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use super::{LocalCache, locked};

/// The families, each with a `# TYPE` line, written out for `docs/metrics.md`.
const HELD: &str = "tierstore_fs_cache_descriptors";
const SHORT: &str = "tierstore_fs_open_descriptor_failures_total";

/// The most mounts the node keeps the reports of.
pub(in crate::server) const MOST_MOUNTS: usize = 64;

/// The most processes of one mount whose last total is kept.
pub(in crate::server) const PROCESSES: usize = 2;

/// The cache files a mount holds open, and the opens short of descriptors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct Descriptors {
    /// The id of the mount.
    pub mount: String,
    /// The id of the mount process.
    pub process: u64,
    /// The cache files the mount process holds open now.
    pub held: u64,
    /// The opens that failed for lack of descriptors since the mount process started.
    pub short: u64,
}

/// `name` as the value of a label: `\` and `"` escaped.
fn escaped(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What one mount reported.
#[derive(Debug, Default)]
struct Mount {
    /// `held` of the last report.
    held: u64,
    /// What the totals of its processes grew by, in all.
    short: u64,
    /// The last total of each of its latest processes, the latest last.
    processes: Vec<(u64, u64)>,
}

/// The reports of the mounts of one cache.
#[derive(Debug, Default)]
pub(super) struct Held {
    mounts: Mutex<BTreeMap<String, Mount>>,
}

impl Held {
    /// Takes the report: the last `held` of its mount, and what the total of its process grew by
    /// added (module docs).
    fn reported(&self, report: Descriptors) {
        let mut mounts = locked(&self.mounts);
        if !mounts.contains_key(&report.mount) && mounts.len() >= MOST_MOUNTS {
            return;
        }
        let mount = mounts.entry(report.mount).or_default();
        mount.held = report.held;
        let seen = mount
            .processes
            .iter()
            .position(|(process, _)| *process == report.process);
        let before = seen.map(|at| mount.processes.remove(at).1);
        let grew = match before {
            Some(before) if report.short >= before => report.short - before,
            _ => report.short,
        };
        mount.short = mount.short.saturating_add(grew);
        mount.processes.push((report.process, report.short));
        if mount.processes.len() > PROCESSES {
            let _ = mount.processes.remove(0);
        }
    }

    /// The two families in the Prometheus text format, the mounts in the order of their ids.
    pub(super) fn render(&self) -> String {
        let mounts = locked(&self.mounts);
        let mut text = format!("# TYPE {HELD} gauge\n");
        for (id, mount) in mounts.iter() {
            let _ = writeln!(text, "{HELD}{{mount=\"{}\"}} {}", escaped(id), mount.held);
        }
        let _ = writeln!(text, "# TYPE {SHORT} counter");
        for (id, mount) in mounts.iter() {
            let _ = writeln!(text, "{SHORT}{{mount=\"{}\"}} {}", escaped(id), mount.short);
        }
        text
    }
}

impl LocalCache {
    /// A mount reported the cache files it holds open.
    pub(in crate::server) fn descriptors_reported(&self, report: Descriptors) {
        self.inner.starts.descriptors.reported(report);
    }
}

#[cfg(test)]
#[path = "descriptors_tests.rs"]
mod tests;
