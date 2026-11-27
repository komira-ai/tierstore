//! What the file system's core tells the cache of the opens of files of its trees, for the
//! cache's `/metricz`: how long each open took, by how it ended and where its bytes came from, and
//! how many opens wait now.
//!
//! The core times every open of a file from the request's arrival to its answer, on its own clock
//! (`treefs/core/opens.rs`), and counts the opens in flight. At each look for idle trees (every
//! quarter of `fs.tree_idle_secs`, at most a minute apart) and when its readers stop for a
//! hand-over, it reports them, as `descriptors` are reported (`descriptors.rs`), by calling the
//! cache in this process (`treefs/in_process.rs`): an [`Opens`] of
//!
//! - `mount`: the id of the mount;
//! - `process`: the id of the mount process (its pid);
//! - `waiting`: the opens in flight now;
//! - then, for each pair of an [`Outcome`] and a [`Source`] in the order of [`KINDS`], the
//!   milliseconds of all those opens and how many fell in each bucket of
//!   [`BOUNDS_MS`](super::attaches::BOUNDS_MS) and above the last: running totals since that
//!   process started, so that a report lost loses nothing the next one carries.
//!
//! The node adds to `tierstore_fs_open_seconds` what each process's totals grew by since its
//! report before, and the whole totals of a process it has not seen (or one of whose totals went
//! down: a new process given the same id). The totals of the
//! [`PROCESSES`](super::descriptors::PROCESSES) latest processes of a mount are kept, so the two
//! processes of a hand-over, reporting in turns, are counted apart and neither twice. The gauge
//! `tierstore_fs_opens_waiting` is the sum over the mounts of each one's last `waiting`. At most
//! [`MOST_MOUNTS`](super::descriptors::MOST_MOUNTS) mounts are kept: a report of another, when
//! that many are, is counted nowhere.
//!
//! The report is advice, like the others of the core: it reaches the cache's counts at once, on
//! the caller's thread, and a lost one loses nothing the next carries.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use super::attaches::{BUCKETS, Histogram};
use super::descriptors::{MOST_MOUNTS, PROCESSES};
use super::{LocalCache, locked};

/// The families, each with a `# TYPE` line, and the series of the histogram, each written out for
/// `docs/metrics.md`.
const OPEN: &str = "tierstore_fs_open_seconds";
const OPEN_BUCKET: &str = "tierstore_fs_open_seconds_bucket";
const OPEN_SUM: &str = "tierstore_fs_open_seconds_sum";
const OPEN_COUNT: &str = "tierstore_fs_open_seconds_count";
const WAITING: &str = "tierstore_fs_opens_waiting";

/// How an open ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Outcome {
    /// It was answered with a handle.
    Ok,
    /// It was answered with an error.
    Error,
}

/// Where the bytes of an open came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Source {
    /// The mount had them without asking the node: the cache held the file.
    Cached,
    /// The mount asked the node for them (a fill of the blob, or the span of a pack) and waited
    /// for its answer.
    Filled,
}

/// The pairs of an outcome and a source, in the order of a report and of the rendered series.
pub(in crate::server) const KINDS: [(Outcome, Source); 4] = [
    (Outcome::Ok, Source::Cached),
    (Outcome::Ok, Source::Filled),
    (Outcome::Error, Source::Cached),
    (Outcome::Error, Source::Filled),
];

/// The place of `(outcome, source)` in [`KINDS`].
pub(in crate::server) fn kind(outcome: Outcome, source: Source) -> usize {
    let error = usize::from(outcome == Outcome::Error);
    let filled = usize::from(source == Source::Filled);
    error * 2 + filled
}

/// The words of a pair, as its labels.
fn labels((outcome, source): (Outcome, Source)) -> String {
    let outcome = match outcome {
        Outcome::Ok => "ok",
        Outcome::Error => "error",
    };
    let source = match source {
        Source::Cached => "cached",
        Source::Filled => "filled",
    };
    format!("outcome=\"{outcome}\",source=\"{source}\",")
}

/// The opens of one kind: the milliseconds of them all, and how many fell in each bucket, not
/// cumulated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Spans {
    pub ms: u64,
    pub buckets: [u64; BUCKETS],
}

impl Spans {
    /// Whether no number of `self` is below that of `before`: the totals of one process, grown.
    fn grew_from(&self, before: &Self) -> bool {
        self.ms >= before.ms
            && self
                .buckets
                .iter()
                .zip(before.buckets)
                .all(|(n, b)| *n >= b)
    }

    /// What `self` has beyond `before`.
    fn less(&self, before: &Self) -> Self {
        let mut buckets = self.buckets;
        for (n, b) in buckets.iter_mut().zip(before.buckets) {
            *n = n.saturating_sub(b);
        }
        Self {
            ms: self.ms.saturating_sub(before.ms),
            buckets,
        }
    }
}

/// The opens of a mount process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct Opens {
    /// The id of the mount.
    pub mount: String,
    /// The id of the mount process.
    pub process: u64,
    /// The opens in flight now.
    pub waiting: u64,
    /// The opens of each kind of [`KINDS`] since the mount process started.
    pub spans: [Spans; KINDS.len()],
}

/// What the totals `now` of the process `process` of a mount add to the histograms, given
/// `processes`, the last totals of the latest processes of that mount (the latest last): what
/// they grew by since that process's report before, or all of them for a process not kept (or one
/// whose totals went down: a new process given the same id). `processes` then keeps `now` as the
/// latest, and at most [`PROCESSES`] processes. Also the rule of the requests' report
/// (`requests.rs`).
pub(super) fn grown<const N: usize>(
    processes: &mut Vec<(u64, [Spans; N])>,
    process: u64,
    now: [Spans; N],
) -> [Spans; N] {
    let seen = processes.iter().position(|(kept, _)| *kept == process);
    let before = seen.map(|at| processes.remove(at).1);
    let before = before
        .filter(|before| {
            now.iter()
                .zip(before)
                .all(|(now, before)| now.grew_from(before))
        })
        .unwrap_or([Spans::default(); N]);
    let mut more = [Spans::default(); N];
    for ((more, now), before) in more.iter_mut().zip(&now).zip(&before) {
        *more = now.less(before);
    }
    processes.push((process, now));
    if processes.len() > PROCESSES {
        let _ = processes.remove(0);
    }
    more
}

/// What one mount reported.
#[derive(Debug, Default)]
struct Mount {
    /// `waiting` of the last report.
    waiting: u64,
    /// The last totals of each of its latest processes, the latest last.
    processes: Vec<(u64, [Spans; KINDS.len()])>,
}

/// The reports taken so far.
#[derive(Debug, Default)]
struct State {
    mounts: BTreeMap<String, Mount>,
    /// `tierstore_fs_open_seconds`, by [`KINDS`].
    opens: [Histogram; KINDS.len()],
}

/// The reports of the opens of the mounts of one cache.
#[derive(Debug, Default)]
pub(super) struct Opened {
    state: Mutex<State>,
}

impl Opened {
    /// Takes the report: the last `waiting` of its mount, and what the totals of its process grew
    /// by added (module docs).
    fn reported(&self, report: Opens) {
        let mut state = locked(&self.state);
        let State { mounts, opens } = &mut *state;
        if !mounts.contains_key(&report.mount) && mounts.len() >= MOST_MOUNTS {
            return;
        }
        let mount = mounts.entry(report.mount).or_default();
        mount.waiting = report.waiting;
        let more = grown(&mut mount.processes, report.process, report.spans);
        for (histogram, more) in opens.iter_mut().zip(&more) {
            histogram.add(&more.buckets, more.ms);
        }
    }

    /// The two families in the Prometheus text format, all of one moment: the histogram of each
    /// kind, in the order of [`KINDS`], and the gauge.
    pub(super) fn render(&self) -> String {
        let state = locked(&self.state);
        let mut text = format!("# TYPE {OPEN} histogram\n");
        for (pair, histogram) in KINDS.into_iter().zip(&state.opens) {
            let names = [OPEN_BUCKET, OPEN_SUM, OPEN_COUNT];
            histogram.render(&mut text, names, &labels(pair));
        }
        let waiting = state
            .mounts
            .values()
            .fold(0u64, |all, mount| all.saturating_add(mount.waiting));
        let _ = writeln!(text, "# TYPE {WAITING} gauge");
        let _ = writeln!(text, "{WAITING} {waiting}");
        text
    }
}

impl LocalCache {
    /// A mount reported the opens of its files.
    pub(in crate::server) fn opens_reported(&self, report: Opens) {
        self.inner.starts.opens.reported(report);
    }
}

// The requests of the mounts' frontends, whose totals the node follows by the rule of the opens
// (`grown`).
#[path = "requests.rs"]
pub(in crate::server) mod requests;

#[cfg(test)]
#[path = "opens_tests.rs"]
mod tests;
