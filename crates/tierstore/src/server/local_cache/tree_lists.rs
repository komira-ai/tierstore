//! What `tierstore fs` tells the node of the version 2 tree lists it holds mapped, for the cache's
//! `/metricz`: how many and how many bytes, the cap `fs.tree_list_ram_bytes`, and the lists it
//! released and mapped past the cap (`server/treefs/core/lists.rs`).
//!
//! At each look for idle trees (every quarter of `fs.tree_idle_secs`, at most a minute apart)
//! the mount sends, from the thread that sends `started` (`fill_client.rs`),
//! `treelists <mount> <process> <mapped> <bytes> <cap> <cap-releases> <idle-releases> <over-cap>`:
//!
//! - `<mount>`: the id of the mount, the name it uses in `pin <mount> ...`;
//! - `<process>`: the id of the mount process (its pid);
//! - `<mapped>` and `<bytes>`: the lists mapped in the mount's process now, each once however many
//!   trees show it, and the bytes their mappings take in whole pages;
//! - `<cap>`: `fs.tree_list_ram_bytes`;
//! - `<cap-releases>`, `<idle-releases>` and `<over-cap>`: since that process started, the lists
//!   the mount released to make room under the cap, those the sweep for idle trees released,
//!   and those mapped past the cap: running totals, so that a report lost loses nothing the next
//!   one carries.
//!
//! The node keeps, for each mount, the three gauges of the last report and the three counters,
//! each grown by what the process's total grew by since its report before, and by the whole
//! total of a process it has not seen (or whose total went down), as `descriptors` are
//! (`local_cache/descriptors.rs`), with the same bounds: [`PROCESSES`] processes a mount and
//! [`MOST_MOUNTS`] mounts. The report is advice: a node of a release before this one answers
//! `refused unknown request`, which the mount ignores, and a report this node cannot read is
//! refused and adds nothing.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use super::descriptors::{MOST_MOUNTS, PROCESSES, number};
use super::{LocalCache, locked};
use crate::server::fill_socket::request::is_name;

/// The families, each with a `# TYPE` line, written out for `docs/metrics.md`.
const MAPPED: &str = "tierstore_fs_tree_lists_mapped";
const BYTES: &str = "tierstore_fs_tree_list_mapped_bytes";
pub(in crate::server) const CAP: &str = "tierstore_fs_tree_list_ram_bytes";
const RELEASES: &str = "tierstore_fs_tree_list_releases_total";
const OVER_CAP: &str = "tierstore_fs_tree_list_over_cap_total";

/// The tree lists a mount holds mapped. The report `treelists`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct TreeLists {
    /// The id of the mount.
    pub mount: String,
    /// The id of the mount process.
    pub process: u64,
    /// The lists mapped now.
    pub mapped: u64,
    /// The bytes their mappings take, in whole pages.
    pub bytes: u64,
    /// `fs.tree_list_ram_bytes`.
    pub cap: u64,
    /// Since the process started: the lists released for the cap, those released idle, and those
    /// mapped past the cap.
    pub totals: Totals,
}

/// The running totals of a `treelists` report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Totals {
    /// The lists released to make room under the cap.
    pub cap: u64,
    /// The lists the sweep for idle trees released.
    pub idle: u64,
    /// The lists mapped past the cap.
    pub over_cap: u64,
}

impl Totals {
    fn words(self) -> [u64; 3] {
        [self.cap, self.idle, self.over_cap]
    }
}

impl TreeLists {
    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        let [cap, idle, over] = self.totals.words();
        format!(
            "treelists {} {} {} {} {} {cap} {idle} {over}",
            self.mount, self.process, self.mapped, self.bytes, self.cap
        )
    }

    /// The report whose words after `treelists` are `rest`: a mount name and seven numbers; none
    /// for anything else.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let (mount, numbers) = rest.split_once(' ')?;
        let numbers: Vec<u64> = numbers.split(' ').map(number).collect::<Option<_>>()?;
        let [process, mapped, bytes, cap, released, idle, over_cap] = numbers.as_slice() else {
            return None;
        };
        is_name(mount).then(|| Self {
            mount: mount.to_owned(),
            process: *process,
            mapped: *mapped,
            bytes: *bytes,
            cap: *cap,
            totals: Totals {
                cap: *released,
                idle: *idle,
                over_cap: *over_cap,
            },
        })
    }
}

/// `name` as the value of a label: `\` and `"` escaped.
fn escaped(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What one mount reported.
#[derive(Debug, Default)]
struct Mount {
    /// The gauges of the last report: lists, bytes and the cap.
    gauges: [u64; 3],
    /// What the totals of its processes grew by, in all: cap releases, idle releases, over cap.
    counters: [u64; 3],
    /// The last totals of each of its latest processes, the latest last.
    processes: Vec<(u64, [u64; 3])>,
}

/// The reports of the mounts of one cache.
#[derive(Debug, Default)]
pub(super) struct Lists {
    mounts: Mutex<BTreeMap<String, Mount>>,
}

impl Lists {
    /// Takes the report: the gauges of its mount, and what the totals of its process grew by
    /// added (module docs).
    fn reported(&self, report: TreeLists) {
        let mut mounts = locked(&self.mounts);
        if !mounts.contains_key(&report.mount) && mounts.len() >= MOST_MOUNTS {
            return;
        }
        let mount = mounts.entry(report.mount).or_default();
        mount.gauges = [report.mapped, report.bytes, report.cap];
        let totals = report.totals.words();
        let seen = mount
            .processes
            .iter()
            .position(|(process, _)| *process == report.process);
        let before = seen.map(|at| mount.processes.remove(at).1);
        for (at, now) in totals.into_iter().enumerate() {
            let grew = match before {
                Some(before) if now >= before[at] => now - before[at],
                _ => now,
            };
            mount.counters[at] = mount.counters[at].saturating_add(grew);
        }
        mount.processes.push((report.process, totals));
        if mount.processes.len() > PROCESSES {
            let _ = mount.processes.remove(0);
        }
    }

    /// The five families in the Prometheus text format, the mounts in the order of their ids.
    pub(super) fn render(&self) -> String {
        let mounts = locked(&self.mounts);
        let mut text = String::new();
        for (at, name) in [MAPPED, BYTES, CAP].into_iter().enumerate() {
            let _ = writeln!(text, "# TYPE {name} gauge");
            for (id, mount) in mounts.iter() {
                let _ = writeln!(
                    text,
                    "{name}{{mount=\"{}\"}} {}",
                    escaped(id),
                    mount.gauges[at]
                );
            }
        }
        let _ = writeln!(text, "# TYPE {RELEASES} counter");
        for (at, why) in ["cap", "idle"].into_iter().enumerate() {
            for (id, mount) in mounts.iter() {
                let _ = writeln!(
                    text,
                    "{RELEASES}{{mount=\"{}\",why=\"{why}\"}} {}",
                    escaped(id),
                    mount.counters[at]
                );
            }
        }
        let _ = writeln!(text, "# TYPE {OVER_CAP} counter");
        for (id, mount) in mounts.iter() {
            let _ = writeln!(
                text,
                "{OVER_CAP}{{mount=\"{}\"}} {}",
                escaped(id),
                mount.counters[2]
            );
        }
        text
    }
}

impl LocalCache {
    /// A mount reported the tree lists it holds mapped.
    pub(in crate::server) fn tree_lists_reported(&self, report: TreeLists) {
        self.inner.starts.tree_lists.reported(report);
    }
}

#[cfg(test)]
#[path = "tree_lists_tests.rs"]
mod tests;
