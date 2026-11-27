//! What `tierstore fs` tells the node of the attaches and detaches of runs, for the cache's
//! `/metricz`: how long each took, and the parts of an attach (`docs/design/virtual-build-fs.md`
//! section 11: `attach_ms`, split into the manifest fetch, the flatten and the table append, and
//! `detach_ms`).
//!
//! An attach is the control socket's `attach <digest> for <run>`, timed on the mount's clock from
//! the request to its answer (`treefs/core/runs.rs`); its three parts add up to it:
//!
//! - **fetch**: from the start to the end of the last manifest the read of the tree fetched (from
//!   the cache directory, or filled through the fill socket), its `fill-tree` included; a tree in
//!   memory already, or one another request is reading, or one read again into memory under the
//!   number it has, counts the whole read here;
//! - **flatten**: from there to the end of the read: the entries made of the manifests, or the
//!   tree list mapped;
//! - **append**: from there to the answer: the tree's number in the table of tree numbers (a
//!   record appended for a tree that has none), the tree kept in memory, the attachments file
//!   written.
//!
//! An attach refused before the read (a digest or a run that is not one, a process being
//! replaced) has every part 0. When it ends, the mount sends
//! `run-attached <outcome> <ms> <fetch-ms> <flatten-ms> <append-ms>`, `ok` when it answered a path
//! and `failed` when it did not; one sample of `tierstore_fs_run_attach_seconds` and one of each
//! part of `tierstore_fs_run_attach_part_seconds`.
//!
//! A detach is `detach <run>`, timed the same way, and sent as `run-detached <outcome> <ms>`: `ok`
//! when it ended the run's attachments, `failed` when it did not (the run attached nothing, or
//! the process is being replaced); one sample of `tierstore_fs_run_detach_seconds`. A run detached
//! with `detach-all-except` is one sample too.
//!
//! The mount sends them from the thread that sends `started` (`fill_client.rs`), so no attach
//! waits on them; one is lost when that thread has a queue of 1024, when the fill socket is away,
//! or when the node is of a release before the report (it answers `refused unknown request`, which
//! the mount ignores).

use std::fmt::Write as _;
use std::sync::Mutex;

use super::attaches::{BUCKETS, Histogram, bucket_of};
use super::descriptors::number;
use super::{LocalCache, locked};

/// The families, with a `# TYPE` line each, and their series, each written out for
/// `docs/metrics.md`.
const ATTACH: &str = "tierstore_fs_run_attach_seconds";
const ATTACH_BUCKET: &str = "tierstore_fs_run_attach_seconds_bucket";
const ATTACH_SUM: &str = "tierstore_fs_run_attach_seconds_sum";
const ATTACH_COUNT: &str = "tierstore_fs_run_attach_seconds_count";
const PART: &str = "tierstore_fs_run_attach_part_seconds";
const PART_BUCKET: &str = "tierstore_fs_run_attach_part_seconds_bucket";
const PART_SUM: &str = "tierstore_fs_run_attach_part_seconds_sum";
const PART_COUNT: &str = "tierstore_fs_run_attach_part_seconds_count";
const DETACH: &str = "tierstore_fs_run_detach_seconds";
const DETACH_BUCKET: &str = "tierstore_fs_run_detach_seconds_bucket";
const DETACH_SUM: &str = "tierstore_fs_run_detach_seconds_sum";
const DETACH_COUNT: &str = "tierstore_fs_run_detach_seconds_count";

/// How an attach or a detach of a run ended: the label `outcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum RunOutcome {
    /// It was done: the attach answered a path, the detach ended the run's attachments.
    Ok,
    /// It was refused.
    Failed,
}

impl RunOutcome {
    /// Every outcome, in the order they are rendered.
    const ALL: [Self; 2] = [Self::Ok, Self::Failed];

    /// The word of the outcome, in a report and as the label `outcome`.
    fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }

    fn of(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|outcome| outcome.word() == word)
    }
}

/// The parts of an attach, in the order of a report and of the rendered series: the label
/// `part`.
const PARTS: [&str; 3] = ["fetch", "flatten", "append"];

/// An attach of a run: how it ended, how long it took, and its parts. The report `run-attached`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct RunAttach {
    pub outcome: RunOutcome,
    pub ms: u64,
    /// The milliseconds of each part, in the order of [`PARTS`]: fetch, flatten, append.
    pub parts: [u64; 3],
}

impl RunAttach {
    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        let [fetch, flatten, append] = self.parts;
        format!(
            "run-attached {} {} {fetch} {flatten} {append}",
            self.outcome.word(),
            self.ms
        )
    }

    /// The report whose words after `run-attached` are `rest`: an outcome and four numbers. None
    /// for anything else.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let (outcome, numbers) = rest.split_once(' ')?;
        let numbers: Vec<u64> = numbers.split(' ').map(number).collect::<Option<_>>()?;
        let [ms, fetch, flatten, append] = numbers.as_slice() else {
            return None;
        };
        Some(Self {
            outcome: RunOutcome::of(outcome)?,
            ms: *ms,
            parts: [*fetch, *flatten, *append],
        })
    }
}

/// A detach of a run: how it ended and how long it took. The report `run-detached`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct RunDetach {
    pub outcome: RunOutcome,
    pub ms: u64,
}

impl RunDetach {
    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        format!("run-detached {} {}", self.outcome.word(), self.ms)
    }

    /// The report whose words after `run-detached` are `rest`: an outcome and a number. None for
    /// anything else.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let (outcome, ms) = rest.split_once(' ')?;
        Some(Self {
            outcome: RunOutcome::of(outcome)?,
            ms: number(ms)?,
        })
    }
}

/// One span of `ms` milliseconds in a histogram.
fn one(histogram: &mut Histogram, ms: u64) {
    let mut buckets = [0u64; BUCKETS];
    buckets[bucket_of(ms)] = 1;
    histogram.add(&buckets, ms);
}

/// The counts of the reports taken so far, each by [`RunOutcome::ALL`].
#[derive(Debug, Default)]
struct State {
    /// `tierstore_fs_run_attach_seconds`.
    attaches: [Histogram; 2],
    /// `tierstore_fs_run_attach_part_seconds`, by [`PARTS`] and then by outcome.
    parts: [[Histogram; 2]; 3],
    /// `tierstore_fs_run_detach_seconds`.
    detaches: [Histogram; 2],
}

/// The reports of the attaches and detaches of runs of the mounts of one cache.
#[derive(Debug, Default)]
pub(super) struct Runs {
    state: Mutex<State>,
}

impl Runs {
    /// Counts the report of an attach.
    fn attached(&self, attach: RunAttach) {
        let mut state = locked(&self.state);
        // The outcomes are numbered in the order of `RunOutcome::ALL`.
        let at = attach.outcome as usize;
        one(&mut state.attaches[at], attach.ms);
        for (part, ms) in state.parts.iter_mut().zip(attach.parts) {
            one(&mut part[at], ms);
        }
    }

    /// Counts the report of a detach.
    fn detached(&self, detach: RunDetach) {
        one(
            &mut locked(&self.state).detaches[detach.outcome as usize],
            detach.ms,
        );
    }

    /// The families in the Prometheus text format, all of one moment: the attaches by outcome,
    /// their parts by part and outcome, and the detaches by outcome.
    pub(super) fn render(&self) -> String {
        let state = locked(&self.state);
        let mut text = format!("# TYPE {ATTACH} histogram\n");
        for (outcome, histogram) in RunOutcome::ALL.iter().zip(&state.attaches) {
            let label = format!("outcome=\"{}\",", outcome.word());
            let names = [ATTACH_BUCKET, ATTACH_SUM, ATTACH_COUNT];
            histogram.render(&mut text, names, &label);
        }
        let _ = writeln!(text, "# TYPE {PART} histogram");
        for (part, histograms) in PARTS.iter().zip(&state.parts) {
            for (outcome, histogram) in RunOutcome::ALL.iter().zip(histograms) {
                let label = format!("part=\"{part}\",outcome=\"{}\",", outcome.word());
                let names = [PART_BUCKET, PART_SUM, PART_COUNT];
                histogram.render(&mut text, names, &label);
            }
        }
        let _ = writeln!(text, "# TYPE {DETACH} histogram");
        for (outcome, histogram) in RunOutcome::ALL.iter().zip(&state.detaches) {
            let label = format!("outcome=\"{}\",", outcome.word());
            let names = [DETACH_BUCKET, DETACH_SUM, DETACH_COUNT];
            histogram.render(&mut text, names, &label);
        }
        text
    }
}

impl LocalCache {
    /// A mount reported an attach of a run.
    pub(in crate::server) fn run_attached(&self, attach: RunAttach) {
        self.inner.starts.runs.attached(attach);
    }

    /// A mount reported a detach of a run.
    pub(in crate::server) fn run_detached(&self, detach: RunDetach) {
        self.inner.starts.runs.detached(detach);
    }
}

#[cfg(test)]
#[path = "runs_tests.rs"]
mod tests;
