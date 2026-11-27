//! The answers `UNAVAILABLE` of the mount provider, by reason:
//! `tierstore_provider_unavailable_total{reason}`, for the cache's `/metricz`.
//!
//! The job agent asks again after every `UNAVAILABLE` until its `--mount-provider-wait` has
//! passed, and then fails the start `input-unavailable` with the last reason it was given. So these
//! counts say, without a look at the jobs, why a node's starts wait and fail on the provider's
//! side:
//!
//! - `no_fs`: a `Prepare` or a `Mount` of a provider whose mount no process of this release serves
//!   (its `served` flag is clear). A file with a `mount-provider` entry and no `fs` entry is
//!   refused at load, so a node that loaded its file does not count it.
//! - `not_ready`: a `Mount` that waited its time and found the tree not yet placed.
//! - `not_prepared_here`: a `Mount` of a source this process has no preparation of and finds no
//!   placement of (the process started after the source's `Prepare`).
//! - `tree_gone`: a `Mount` whose source's placement had ended without a release; it is prepared
//!   again.
//! - `read_failed`: a `Mount` whose preparation could not read the tree's sidecar or list (an
//!   upstream error, or no answer within `fetch_timeout`); it is begun again.
//! - `store_failed`: a `Mount` whose preparation the tree store could not make now (too many
//!   placements, trees off or read-only, a disk error); it is begun again.
//! - `not_released`: a `Release` or a `Sweep` whose placement the store could not remove now.
//!
//! Each answer counts once. A start the agent fails outside the provider (its own look at the
//! directory the provider answered) is not seen here, and neither is a `Prepare` of a new source
//! refused because the provider holds `trees.max_sources` sources, which is counted in
//! `tierstore_provider_prepare_refused_total` (`preparations.rs`).

use std::fmt::Write as _;
use std::sync::atomic::AtomicU64;

// A module of `starts`, whose `Starts` holds the counters beside the mounts' reports.
use super::super::LocalCache;
use super::super::metrics::{bump, read};

/// The family, with its `# TYPE` line, written out for `docs/metrics.md`.
pub(in crate::server::local_cache) const UNAVAILABLE: &str = "tierstore_provider_unavailable_total";

/// Why the mount provider answered `UNAVAILABLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Unavailable {
    /// No process of this release serves the mount: the node's file has no `fs` interface.
    NoFs,
    /// The tree was not placed within the wait of a `Mount`.
    NotReady,
    /// This process has no preparation of the source and finds no placement of it.
    NotPreparedHere,
    /// The source's placement ended without a release.
    TreeGone,
    /// The preparation could not read the tree's sidecar or list.
    ReadFailed,
    /// The tree store could not make the tree now.
    StoreFailed,
    /// The store could not remove a placement now.
    NotReleased,
}

/// The reasons, in the order of their counters.
const REASONS: [Unavailable; 7] = [
    Unavailable::NoFs,
    Unavailable::NotReady,
    Unavailable::NotPreparedHere,
    Unavailable::TreeGone,
    Unavailable::ReadFailed,
    Unavailable::StoreFailed,
    Unavailable::NotReleased,
];

impl Unavailable {
    /// The value of the `reason` label.
    pub(in crate::server) const fn label(self) -> &'static str {
        match self {
            Self::NoFs => "no_fs",
            Self::NotReady => "not_ready",
            Self::NotPreparedHere => "not_prepared_here",
            Self::TreeGone => "tree_gone",
            Self::ReadFailed => "read_failed",
            Self::StoreFailed => "store_failed",
            Self::NotReleased => "not_released",
        }
    }

    /// The place of the reason in [`REASONS`].
    const fn place(self) -> usize {
        match self {
            Self::NoFs => 0,
            Self::NotReady => 1,
            Self::NotPreparedHere => 2,
            Self::TreeGone => 3,
            Self::ReadFailed => 4,
            Self::StoreFailed => 5,
            Self::NotReleased => 6,
        }
    }
}

/// The answers `UNAVAILABLE` of one cache's provider, by reason.
#[derive(Debug, Default)]
pub(in crate::server::local_cache) struct ProviderUnavailable {
    /// By reason, in the order of [`REASONS`].
    counts: [AtomicU64; REASONS.len()],
}

impl ProviderUnavailable {
    /// Counts one answer for `reason`.
    fn count(&self, reason: Unavailable) {
        bump(&self.counts[reason.place()]);
    }

    /// The family in the Prometheus text format: every reason, counted or not.
    pub(in crate::server::local_cache) fn render(&self) -> String {
        let mut text = format!("# TYPE {UNAVAILABLE} counter\n");
        for (reason, count) in REASONS.iter().zip(&self.counts) {
            let label = reason.label();
            let _ = writeln!(text, "{UNAVAILABLE}{{reason=\"{label}\"}} {}", read(count));
        }
        text
    }
}

impl LocalCache {
    /// The mount provider answered a call `UNAVAILABLE` for `reason`: counted once.
    pub(in crate::server) fn provider_unavailable(&self, reason: Unavailable) {
        self.inner.starts.provider_unavailable.count(reason);
    }
}

#[cfg(test)]
#[path = "provider_unavailable_tests.rs"]
mod tests;
