//! What `tierstore fs` tells the node of the cache files it holds open, for the cache's
//! `/metricz`: how many it holds now, and how many opens found no descriptor.
//!
//! The mount process shares one descriptor per cache file (a blob or a pack) across all its open
//! handles. About once a minute it sends, from the thread that sends `started`
//! (`fill_client.rs`), `descriptors <mount> <held> <short>`:
//!
//! - `<mount>`: the id of the mount, the name it uses in `pin <mount> ...`;
//! - `<held>`: the cache files the mount process holds open now;
//! - `<short>`: the opens of files of the mount that failed for lack of descriptors (`EMFILE`,
//!   `ENFILE`) since the mount's previous report.
//!
//! The node keeps, for each mount, the last `held` (the gauge `tierstore_fs_cache_descriptors`)
//! and the sum of every `short` (the counter `tierstore_fs_open_descriptor_failures_total`, which
//! so goes on counting across restarts of the mount process). It keeps at most [`MOST_MOUNTS`]
//! mounts: a report of another mount, when that many are kept, is answered `ok` and counted
//! nowhere, so a client that makes up mount ids cannot grow the node's memory.
//!
//! The report is advice, like the others of the mount: one is lost when the thread that sends it
//! has a queue of 1024, when the fill socket is away, or when the node is of a release before this
//! one (it answers `refused unknown request`, which the mount ignores). A report this node cannot
//! read is refused and adds nothing.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use super::{LocalCache, locked};
use crate::server::fill_socket::request::is_name;

/// The families, each with a `# TYPE` line, written out for `docs/metrics.md`.
const HELD: &str = "tierstore_fs_cache_descriptors";
const SHORT: &str = "tierstore_fs_open_descriptor_failures_total";

/// The most mounts the node keeps the reports of.
pub(in crate::server) const MOST_MOUNTS: usize = 64;

/// The cache files a mount holds open, and the opens short of descriptors. The report
/// `descriptors`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct Descriptors {
    /// The id of the mount.
    pub mount: String,
    /// The cache files the mount process holds open now.
    pub held: u64,
    /// The opens that failed for lack of descriptors since the previous report.
    pub short: u64,
}

impl Descriptors {
    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        format!("descriptors {} {} {}", self.mount, self.held, self.short)
    }

    /// The report whose words after `descriptors` are `rest`: a mount name and two numbers; none
    /// for anything else.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let mut words = rest.split(' ');
        let (mount, held, short) = (words.next()?, words.next()?, words.next()?);
        if words.next().is_some() || !is_name(mount) {
            return None;
        }
        Some(Self {
            mount: mount.to_owned(),
            held: number(held)?,
            short: number(short)?,
        })
    }
}

/// The number `word` writes in decimal digits, with no sign; none for anything else.
fn number(word: &str) -> Option<u64> {
    if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    word.parse().ok()
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
    /// The sum of `short` of every report.
    short: u64,
}

/// The reports of the mounts of one cache.
#[derive(Debug, Default)]
pub(super) struct Held {
    mounts: Mutex<BTreeMap<String, Mount>>,
}

impl Held {
    /// Takes the report: the last `held` of its mount, and its `short` added.
    fn reported(&self, report: Descriptors) {
        let mut mounts = locked(&self.mounts);
        if !mounts.contains_key(&report.mount) && mounts.len() >= MOST_MOUNTS {
            return;
        }
        let mount = mounts.entry(report.mount).or_default();
        mount.held = report.held;
        mount.short = mount.short.saturating_add(report.short);
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
