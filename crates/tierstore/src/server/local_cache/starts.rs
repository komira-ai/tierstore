//! `tierstore_request_to_start_seconds` (`docs/design/fetch-cache-serve.md`, sections 1 and 12):
//! the time from a job's request to the node to the start of its command, one sample per run, from
//! a stamp the scheduler's request leaves to a stamp the mount leaves.
//!
//! - **Request.** The agent of jobscheduler asks the mount provider to `Prepare` a source of a run
//!   the moment it has the job. The first `Prepare` of a run that this process takes stamps the
//!   run with the cache's clock ([`LocalCache::run_requested`]); a later `Prepare` of the run,
//!   another source of it or the same one asked again, leaves the stamp as it is.
//! - **Start.** The job's command is the first to look inside the run's trees: the agent opens
//!   each tree's root and the kernel looks the roots up when it mounts the job's overlay, and
//!   neither reads below a root (jobscheduler's `docs/design/job-rootfs.md`, section 7.2). So
//!   the file system's core tells the cache, on its report `started <path>`, the first time a lookup
//!   in a directory of a tree placed at `path`, or a listing of one, reaches it, and the first
//!   such report of any source of the run ends the span ([`LocalCache::run_started`]).
//!
//! A run is sampled once: a report of another source of it, or of the same tree read into the
//! mount's memory again, adds nothing. A run is known by the directory its sources' paths are in,
//! `mount-provider/<agent>/<run>` (the provider's module `names`); a report of a path in any other
//! directory, such as that of a placement of the trees API, or of a run this process did not see
//! a `Prepare` of (it started after the `Prepare`), adds nothing. A release or a sweep of a source
//! of the run forgets it ([`LocalCache::run_forgotten`]), so a run that ends before its command
//! touches its trees is not sampled. At most [`MOST_RUNS`] runs are kept: a request beyond them
//! forgets the run requested longest ago.
//!
//! The clock is the cache's, the wall clock of the node; a span over a step of it back counts as 0.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use super::attaches::Attaches;
use super::attaches::runs::Runs;
use super::descriptors::Held;
use super::opens::Opened;
use super::opens::requests::Answered;
use super::tree_lists::Lists;
use super::{LocalCache, locked};

/// The family, with a `# TYPE` line, and its three series, each written out for `docs/metrics.md`.
pub(in crate::server) const REQUEST_TO_START: &str = "tierstore_request_to_start_seconds";
const BUCKET: &str = "tierstore_request_to_start_seconds_bucket";
const SUM: &str = "tierstore_request_to_start_seconds_sum";
const COUNT: &str = "tierstore_request_to_start_seconds_count";

/// The upper bounds of the buckets, in milliseconds: around the launch budget of 100 to 200 ms
/// (section 1), up to jobscheduler's start timeout of 900 s.
pub(super) const BOUNDS_MS: [u64; 16] = [
    5, 10, 25, 50, 100, 200, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000,
    900_000,
];

/// The most runs kept between their request and their start or release.
pub(super) const MOST_RUNS: usize = 4096;

/// A run between its first `Prepare` and its release.
#[derive(Debug)]
struct Run {
    /// When its first `Prepare` was taken, on the cache's clock.
    requested_ms: u64,
    /// Whether its span was sampled.
    started: bool,
}

/// The runs, and the histogram of their spans.
#[derive(Debug, Default)]
struct State {
    /// The runs, by the directory of their sources' paths.
    runs: BTreeMap<String, Run>,
    /// The spans in each bucket of [`BOUNDS_MS`], not cumulated; the spans above the last bound
    /// are the count less their sum.
    buckets: [u64; BOUNDS_MS.len()],
    count: u64,
    sum_ms: u64,
}

/// The runs and the histogram of one cache, and what its mounts report of their attaches.
#[derive(Debug, Default)]
pub(super) struct Starts {
    state: Mutex<State>,
    /// The attaches and the manifest reads the mounts report (`attaches`).
    pub(super) attaches: Attaches,
    /// The descriptors the mounts report (`descriptors`).
    pub(super) descriptors: Held,
    /// The opens of files the mounts report (`opens`).
    pub(super) opens: Opened,
    /// The tree lists the mounts hold mapped (`tree_lists`).
    pub(super) tree_lists: Lists,
    /// The requests of the mounts' frontends (`requests`).
    pub(super) requests: Answered,
    /// The attaches and detaches of runs the mounts report (`runs`).
    pub(super) runs: Runs,
}

impl Starts {
    /// The run `run` was asked for at `now`, unless it was before.
    fn requested(&self, run: &str, now: u64) {
        let mut state = locked(&self.state);
        if state.runs.contains_key(run) {
            return;
        }
        let full = state.runs.len() >= MOST_RUNS;
        let oldest = full.then(|| {
            let runs = state.runs.iter();
            let oldest = runs.min_by_key(|(_, kept)| kept.requested_ms);
            oldest.map(|(key, _)| key.clone())
        });
        if let Some(oldest) = oldest.flatten() {
            let _ = state.runs.remove(&oldest);
        }
        let _ = state.runs.insert(
            run.to_owned(),
            Run {
                requested_ms: now,
                started: false,
            },
        );
    }

    /// The tree at `path` was first looked into at `now`: the span of its run, in milliseconds, if
    /// this sampled it.
    fn started(&self, path: &str, now: u64) -> Option<u64> {
        let (run, _) = path.rsplit_once('/')?;
        let mut state = locked(&self.state);
        let kept = state.runs.get_mut(run).filter(|kept| !kept.started)?;
        kept.started = true;
        let span = now.saturating_sub(kept.requested_ms);
        let bucket = BOUNDS_MS.iter().position(|bound| span <= *bound);
        if let Some(bucket) = bucket {
            state.buckets[bucket] += 1;
        }
        state.count += 1;
        state.sum_ms = state.sum_ms.saturating_add(span);
        Some(span)
    }

    /// Forgets the run `run`.
    fn forget(&self, run: &str) {
        let _ = locked(&self.state).runs.remove(run);
    }

    /// The runs kept.
    #[cfg(test)]
    fn runs(&self) -> usize {
        locked(&self.state).runs.len()
    }

    /// The family in the Prometheus text format: cumulative buckets, the sum in seconds and the
    /// count, all of one moment; then the families of the mounts' reports (`attaches`,
    /// `descriptors`, `opens`, `tree_lists`, `requests`, `runs`).
    pub(super) fn render(&self) -> String {
        let state = locked(&self.state);
        let mut text = format!("# TYPE {REQUEST_TO_START} histogram\n");
        let mut below = 0;
        for (bound, count) in BOUNDS_MS.iter().zip(state.buckets) {
            below += count;
            let le = seconds(*bound);
            let _ = writeln!(text, "{BUCKET}{{le=\"{le}\"}} {below}");
        }
        let _ = writeln!(text, "{BUCKET}{{le=\"+Inf\"}} {}", state.count);
        let _ = writeln!(text, "{SUM} {}", seconds(state.sum_ms));
        let _ = writeln!(text, "{COUNT} {}", state.count);
        text.push_str(&self.attaches.render());
        text.push_str(&self.descriptors.render());
        text.push_str(&self.opens.render());
        text.push_str(&self.tree_lists.render());
        text.push_str(&self.requests.render());
        text.push_str(&self.runs.render());
        text
    }
}

/// `ms` milliseconds in seconds, in decimal with no trailing zero: `0.005`, `0.1`, `2.5`, `60`.
pub(super) fn seconds(ms: u64) -> String {
    let (whole, part) = (ms / 1000, ms % 1000);
    if part == 0 {
        return whole.to_string();
    }
    let part = format!("{part:03}");
    format!("{whole}.{}", part.trim_end_matches('0'))
}

impl LocalCache {
    /// The run whose sources' paths are in the directory `run` was asked for now: its first
    /// `Prepare`. A run asked for before keeps its stamp.
    pub(in crate::server) fn run_requested(&self, run: &str) {
        let now = self.inner.clock.now();
        self.inner.starts.requested(run, now);
    }

    /// The mount first looked into the tree at the fs location `path` now: the span of its run in
    /// milliseconds, if this ended it.
    pub(in crate::server) fn run_started(&self, path: &str) -> Option<u64> {
        let now = self.inner.clock.now();
        self.inner.starts.started(path, now)
    }

    /// Forgets the run whose sources' paths are in the directory `run`: a source of it was released.
    pub(in crate::server) fn run_forgotten(&self, run: &str) {
        self.inner.starts.forget(run);
    }

    /// The runs this cache keeps between their request and their start or release.
    #[cfg(test)]
    pub(in crate::server) fn runs_kept(&self) -> usize {
        self.inner.starts.runs()
    }
}

#[cfg(test)]
#[path = "starts_tests.rs"]
mod tests;
