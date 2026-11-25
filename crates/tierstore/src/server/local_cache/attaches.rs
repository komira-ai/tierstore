//! What `tierstore fs` tells the node of the trees it reads, for the cache's `/metricz`: how long
//! each attach of a tree on a lookup took, and where each manifest of a tree it read came from.
//!
//! - **Attach.** A lookup of `trees/<digest>` that no run has attached reads the tree and attaches
//!   it (`treefs/core/implicit.rs`). When it ends, the mount sends `attached <outcome> <ms>`: the
//!   milliseconds from the lookup's start to its answer on the mount's clock, and the outcome
//!   (`ok`, `absent` for a tree the upstream has no root manifest of, `failed` for any other that
//!   cannot be read or numbered). One sample of `tierstore_fs_tree_attach_seconds` each.
//! - **Manifests.** Every read of a tree from its manifests (`treefs/core/placed.rs`) sends, when
//!   it ends, `fetched <cache> <filled> <failed> <fill-ms> <counts>`: the manifests read from the
//!   cache directory, filled through the fill socket and then read, and not had (the fill failed,
//!   or the file was not there after it); the milliseconds of all its fills; and how many of those
//!   fills took each bucket of [`BOUNDS_MS`] and above the last, one count each. They add to
//!   `tierstore_fs_manifest_fetch_total` and `tierstore_fs_manifest_fill_seconds`. A read that
//!   fetched no manifest (a placement's tree, which is read whole) sends nothing.
//!
//! - **Levels.** The same read sends, after its `fetched`, `levels <levels> <rounds>`: how many
//!   levels of the tree it went down (1 for a tree that is a root alone), and its rounds of fills
//!   over the fill socket, each waited for before the next: one for each level in which it filled
//!   a manifest, and one for a `fill-tree` it asked (`source/tree_fill.rs`). They add to
//!   `tierstore_fs_tree_levels_total` and `tierstore_fs_tree_fill_rounds_total`, so that the
//!   rounds of an attach can be held against its time.
//!
//! The mount sends them from the thread that sends `started` (`fill_client.rs`), so no lookup
//! waits on them; one is lost when that thread has a queue of 1024, when the fill socket is away,
//! or when the node is of a release before the report. A report this node cannot read (another
//! number of buckets, from a mount of another release) is refused and adds nothing.
//!
//! The node also times each fill of a manifest it makes itself, on its own clock, from the request
//! to the end of the fill (a permit of the fill socket's pool, the upstream's answers, the write
//! into the cache): `tierstore_cache_manifest_fill_seconds`, by the way it was asked (`via`):
//! `fill`, a `fill <sha256>` of the fill socket or the same call made in process, or `tree`, a
//! manifest filled for a `fill-tree`. Held against `tierstore_fs_manifest_fill_seconds`, the
//! mount's time for the same fills, it says what the socket adds to each.

use std::fmt::Write as _;
use std::sync::Mutex;

use super::starts::seconds;
use super::{LocalCache, locked};

/// The families, with a `# TYPE` line each, and their series, each written out for
/// `docs/metrics.md`.
const ATTACH: &str = "tierstore_fs_tree_attach_seconds";
const ATTACH_BUCKET: &str = "tierstore_fs_tree_attach_seconds_bucket";
const ATTACH_SUM: &str = "tierstore_fs_tree_attach_seconds_sum";
const ATTACH_COUNT: &str = "tierstore_fs_tree_attach_seconds_count";
const FETCHES: &str = "tierstore_fs_manifest_fetch_total";
const FILL: &str = "tierstore_fs_manifest_fill_seconds";
const FILL_BUCKET: &str = "tierstore_fs_manifest_fill_seconds_bucket";
const FILL_SUM: &str = "tierstore_fs_manifest_fill_seconds_sum";
const FILL_COUNT: &str = "tierstore_fs_manifest_fill_seconds_count";
const LEVELS: &str = "tierstore_fs_tree_levels_total";
const ROUNDS: &str = "tierstore_fs_tree_fill_rounds_total";
const NODE_FILL: &str = "tierstore_cache_manifest_fill_seconds";
const NODE_FILL_BUCKET: &str = "tierstore_cache_manifest_fill_seconds_bucket";
const NODE_FILL_SUM: &str = "tierstore_cache_manifest_fill_seconds_sum";
const NODE_FILL_COUNT: &str = "tierstore_cache_manifest_fill_seconds_count";

/// The upper bounds of the buckets of both histograms, in milliseconds: from a manifest read from
/// the cache directory, under a millisecond, through the launch budget of 100 ms, to a fill that
/// waits out a slow upstream.
pub(in crate::server) const BOUNDS_MS: [u64; 16] = [
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 300_000,
];

/// The buckets of a histogram: one for each bound, and one above the last.
pub(in crate::server) const BUCKETS: usize = BOUNDS_MS.len() + 1;

/// The bucket a span of `ms` milliseconds falls in: that of the first bound at or over it, or the
/// one above the last.
pub(in crate::server) fn bucket_of(ms: u64) -> usize {
    BOUNDS_MS
        .iter()
        .position(|bound| ms <= *bound)
        .unwrap_or(BOUNDS_MS.len())
}

/// How an attach on a lookup ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Outcome {
    /// The tree was attached.
    Ok,
    /// The upstream has no root manifest of the tree: the lookup is `ENOENT`.
    Absent,
    /// The tree could not be read or numbered: the lookup is `EIO`.
    Failed,
}

impl Outcome {
    /// Every outcome, in the order they are rendered.
    const ALL: [Self; 3] = [Self::Ok, Self::Absent, Self::Failed];

    /// The word of the outcome, in a report and as the label `outcome`.
    pub(in crate::server) fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Absent => "absent",
            Self::Failed => "failed",
        }
    }

    fn of(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|outcome| outcome.word() == word)
    }
}

/// An attach on a lookup: how it ended and how long it took. The report `attached`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) struct Attach {
    pub outcome: Outcome,
    pub ms: u64,
}

impl Attach {
    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        format!("attached {} {}", self.outcome.word(), self.ms)
    }

    /// The report whose words after `attached` are `rest`; none for words that are not one.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let (outcome, ms) = rest.split_once(' ')?;
        Some(Self {
            outcome: Outcome::of(outcome)?,
            ms: number(ms)?,
        })
    }
}

/// Where the manifests of one read of a tree came from, and how long their fills took. The report
/// `fetched`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Fetches {
    /// Read from the cache directory.
    pub cache: u64,
    /// Filled through the fill socket, then read.
    pub filled: u64,
    /// Not had: the fill failed, or the file was not there after it.
    pub failed: u64,
    /// The milliseconds of every fill asked for, whatever its answer.
    pub fill_ms: u64,
    /// How many of those fills fell in each bucket of [`BOUNDS_MS`], not cumulated.
    pub fills: [u64; BUCKETS],
}

impl Fetches {
    /// The manifests asked for.
    pub(in crate::server) fn manifests(&self) -> u64 {
        self.cache + self.filled + self.failed
    }

    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        let mut line = format!(
            "fetched {} {} {} {}",
            self.cache, self.filled, self.failed, self.fill_ms
        );
        for count in self.fills {
            let _ = write!(line, " {count}");
        }
        line
    }

    /// The report whose words after `fetched` are `rest`: four numbers and one for each bucket.
    /// None for anything else.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let numbers: Vec<u64> = rest.split(' ').map(number).collect::<Option<_>>()?;
        let [cache, filled, failed, fill_ms, counts @ ..] = numbers.as_slice() else {
            return None;
        };
        Some(Self {
            cache: *cache,
            filled: *filled,
            failed: *failed,
            fill_ms: *fill_ms,
            fills: counts.try_into().ok()?,
        })
    }
}

/// The levels of one read of a tree from its manifests, and its rounds of fills over the fill
/// socket. The report `levels`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::server) struct Levels {
    /// The levels of the tree the read went down.
    pub levels: u64,
    /// Its fills over the fill socket that waited for one another: one for each level with a fill
    /// of a manifest, and one for a `fill-tree`.
    pub rounds: u64,
}

impl Levels {
    /// The line of the report.
    pub(in crate::server) fn line(&self) -> String {
        format!("levels {} {}", self.levels, self.rounds)
    }

    /// The report whose words after `levels` are `rest`: two numbers. None for anything else.
    pub(in crate::server) fn parse(rest: &str) -> Option<Self> {
        let (levels, rounds) = rest.split_once(' ')?;
        Some(Self {
            levels: number(levels)?,
            rounds: number(rounds)?,
        })
    }
}

/// The way the node was asked for a fill of a manifest it times: the label `via` of
/// `tierstore_cache_manifest_fill_seconds`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Via {
    /// A `fill <sha256>` of the fill socket, or the same call made in process.
    Fill,
    /// A manifest the node filled for a `fill-tree`.
    Tree,
}

impl Via {
    /// Every way, in the order they are rendered.
    const ALL: [Self; 2] = [Self::Fill, Self::Tree];

    /// The word of the way, as the label `via`.
    fn word(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Tree => "tree",
        }
    }
}

/// The number `word` writes in decimal digits, with no sign; none for anything else.
fn number(word: &str) -> Option<u64> {
    if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    word.parse().ok()
}

/// A histogram of spans in milliseconds over [`BOUNDS_MS`].
#[derive(Debug, Default)]
struct Histogram {
    /// The spans in each bucket, not cumulated.
    buckets: [u64; BUCKETS],
    count: u64,
    sum_ms: u64,
}

impl Histogram {
    /// Adds the spans whose buckets are `buckets` and whose sum is `sum_ms`.
    fn add(&mut self, buckets: &[u64; BUCKETS], sum_ms: u64) {
        for (mine, more) in self.buckets.iter_mut().zip(buckets) {
            *mine = mine.saturating_add(*more);
        }
        let more = buckets.iter().fold(0u64, |all, n| all.saturating_add(*n));
        self.count = self.count.saturating_add(more);
        self.sum_ms = self.sum_ms.saturating_add(sum_ms);
    }

    /// The series of the histogram `name`, with `label` (`outcome="ok",`, or nothing) before `le`
    /// in its buckets and alone on its sum and count.
    fn render(&self, text: &mut String, names: [&str; 3], label: &str) {
        let [bucket, sum, count] = names;
        let mut below = 0u64;
        for (bound, n) in BOUNDS_MS.iter().zip(self.buckets) {
            below = below.saturating_add(n);
            let le = seconds(*bound);
            let _ = writeln!(text, "{bucket}{{{label}le=\"{le}\"}} {below}");
        }
        let _ = writeln!(text, "{bucket}{{{label}le=\"+Inf\"}} {}", self.count);
        let alone = label.trim_end_matches(',');
        let braces = if alone.is_empty() {
            String::new()
        } else {
            format!("{{{alone}}}")
        };
        let _ = writeln!(text, "{sum}{braces} {}", seconds(self.sum_ms));
        let _ = writeln!(text, "{count}{braces} {}", self.count);
    }
}

/// The counts of the reports taken so far.
#[derive(Debug, Default)]
struct State {
    /// `tierstore_fs_tree_attach_seconds`, by [`Outcome::ALL`].
    attaches: [Histogram; 3],
    /// `tierstore_fs_manifest_fetch_total`: cache, fill, error.
    fetches: [u64; 3],
    /// `tierstore_fs_manifest_fill_seconds`.
    fills: Histogram,
    /// `tierstore_fs_tree_levels_total` and `tierstore_fs_tree_fill_rounds_total`.
    levels: Levels,
    /// `tierstore_cache_manifest_fill_seconds`, by [`Via::ALL`].
    node_fills: [Histogram; 2],
}

/// The reports of the mounts of one cache.
#[derive(Debug, Default)]
pub(super) struct Attaches {
    state: Mutex<State>,
}

impl Attaches {
    /// Counts the report of an attach.
    fn attached(&self, attach: Attach) {
        let mut one = [0u64; BUCKETS];
        one[bucket_of(attach.ms)] = 1;
        // The outcomes are numbered in the order of `Outcome::ALL`.
        let at = attach.outcome as usize;
        locked(&self.state).attaches[at].add(&one, attach.ms);
    }

    /// Counts the report of the manifests of a read.
    fn fetched(&self, fetches: &Fetches) {
        let mut state = locked(&self.state);
        let counted = [fetches.cache, fetches.filled, fetches.failed];
        for (mine, more) in state.fetches.iter_mut().zip(counted) {
            *mine = mine.saturating_add(more);
        }
        state.fills.add(&fetches.fills, fetches.fill_ms);
    }

    /// Counts the report of the levels of a read.
    fn levels(&self, levels: Levels) {
        let mut state = locked(&self.state);
        state.levels.levels = state.levels.levels.saturating_add(levels.levels);
        state.levels.rounds = state.levels.rounds.saturating_add(levels.rounds);
    }

    /// Counts a fill of a manifest the node made, asked for by `via`, that took `ms`.
    fn node_filled(&self, via: Via, ms: u64) {
        let mut one = [0u64; BUCKETS];
        one[bucket_of(ms)] = 1;
        // The ways are numbered in the order of `Via::ALL`.
        locked(&self.state).node_fills[via as usize].add(&one, ms);
    }

    /// The families in the Prometheus text format, all of one moment: the attaches by outcome,
    /// the manifests by where they came from, the mounts' fills, the levels and rounds of the
    /// reads, and the node's own fills of manifests by the way they were asked.
    pub(super) fn render(&self) -> String {
        let state = locked(&self.state);
        let mut text = format!("# TYPE {ATTACH} histogram\n");
        for (outcome, histogram) in Outcome::ALL.iter().zip(&state.attaches) {
            let label = format!("outcome=\"{}\",", outcome.word());
            let names = [ATTACH_BUCKET, ATTACH_SUM, ATTACH_COUNT];
            histogram.render(&mut text, names, &label);
        }
        let _ = writeln!(text, "# TYPE {FETCHES} counter");
        for (result, count) in ["cache", "fill", "error"].iter().zip(state.fetches) {
            let _ = writeln!(text, "{FETCHES}{{result=\"{result}\"}} {count}");
        }
        let _ = writeln!(text, "# TYPE {FILL} histogram");
        let names = [FILL_BUCKET, FILL_SUM, FILL_COUNT];
        state.fills.render(&mut text, names, "");
        let _ = writeln!(text, "# TYPE {LEVELS} counter");
        let _ = writeln!(text, "{LEVELS} {}", state.levels.levels);
        let _ = writeln!(text, "# TYPE {ROUNDS} counter");
        let _ = writeln!(text, "{ROUNDS} {}", state.levels.rounds);
        let _ = writeln!(text, "# TYPE {NODE_FILL} histogram");
        for (via, histogram) in Via::ALL.iter().zip(&state.node_fills) {
            let label = format!("via=\"{}\",", via.word());
            let names = [NODE_FILL_BUCKET, NODE_FILL_SUM, NODE_FILL_COUNT];
            histogram.render(&mut text, names, &label);
        }
        text
    }
}

impl LocalCache {
    /// A mount reported an attach of a tree on a lookup.
    pub(in crate::server) fn tree_attached(&self, attach: Attach) {
        self.inner.starts.attaches.attached(attach);
    }

    /// A mount reported where the manifests of a read of a tree came from.
    pub(in crate::server) fn manifests_fetched(&self, fetches: &Fetches) {
        self.inner.starts.attaches.fetched(fetches);
    }

    /// A mount reported the levels of a read of a tree and its rounds of fills.
    pub(in crate::server) fn tree_levels(&self, levels: Levels) {
        self.inner.starts.attaches.levels(levels);
    }

    /// This node filled a manifest asked for by `via` in `ms` milliseconds of its clock.
    pub(super) fn manifest_filled(&self, via: Via, ms: u64) {
        self.inner.starts.attaches.node_filled(via, ms);
    }
}

#[cfg(test)]
#[path = "attaches_tests.rs"]
mod tests;
