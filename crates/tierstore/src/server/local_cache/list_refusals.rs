//! The tree lists the mount provider refused as not mountable through tierstore
//! (`docs/design/fetch-cache-serve.md`, 3.5 and 12), and the trees it found with no L2 sidecar:
//! `tierstore_list_refused_total{reason}`, `tierstore_l2_sidecar_missing_total` and
//! `tierstore_l2_sidecar_fallback_total`, for the cache's `/metricz`.
//!
//! The provider reads a tree's L2 sidecar (`cas/l2/<id>`), and refuses a tree whose sidecar is
//! there and that it cannot read with `ListVersionUnsupported` ([`Refusal`]): the object there is
//! a list of another version (`v1_list`, `unknown_version`) or names a digest function this
//! release does not know (`unknown_cell_digest`). Each refusal of a preparation counts once under
//! its reason. A tree whose sidecar is not there is not refused: the provider places it from its
//! list `cas/l/<id>`, as the release before the sidecar did. Each such preparation counts once in
//! `tierstore_l2_sidecar_missing_total`, the count the design's rollout watches (zero for 7 days
//! before the CAS service stops writing version 2 lists), and each tree so placed once in
//! `tierstore_l2_sidecar_fallback_total`. `missing_sidecar` is kept as a reason, at zero: no tree
//! is refused for it. A sidecar that is there and refused as malformed is not a refusal of the
//! version, and is not counted here: the preparation's log line says why.

use std::fmt::Write as _;
use std::sync::atomic::AtomicU64;

use tierstore_fetch_api::list::Refusal;

// A module of `starts`, whose `Starts` holds the counters beside the mounts' reports.
use super::super::LocalCache;
use super::super::metrics::{bump, read};

/// The families, each with a `# TYPE` line, written out for `docs/metrics.md`.
pub(in crate::server::local_cache) const REFUSED: &str = "tierstore_list_refused_total";
pub(in crate::server::local_cache) const SIDECAR_MISSING: &str =
    "tierstore_l2_sidecar_missing_total";
pub(in crate::server::local_cache) const SIDECAR_FALLBACK: &str =
    "tierstore_l2_sidecar_fallback_total";

/// The reasons, in the order of their counters: the labels of [`Refusal::label`].
const REASONS: [Refusal; 4] = [
    Refusal::V1List,
    Refusal::MissingSidecar,
    Refusal::UnknownCellDigest(0),
    Refusal::UnknownVersion(0),
];

/// The place of `refusal` in [`REASONS`], whatever its payload.
const fn place(refusal: Refusal) -> usize {
    match refusal {
        Refusal::V1List => 0,
        Refusal::MissingSidecar => 1,
        Refusal::UnknownCellDigest(_) => 2,
        Refusal::UnknownVersion(_) => 3,
    }
}

/// The refusals of one cache's provider.
#[derive(Debug, Default)]
pub(in crate::server::local_cache) struct ListRefusals {
    /// By reason, in the order of [`REASONS`].
    refused: [AtomicU64; REASONS.len()],
    /// The preparations that found no sidecar.
    missing: AtomicU64,
    /// The trees placed from their list for want of a sidecar.
    fallback: AtomicU64,
}

impl ListRefusals {
    /// Counts a refusal for `refusal`.
    fn refused(&self, refusal: Refusal) {
        bump(&self.refused[place(refusal)]);
    }

    /// The families in the Prometheus text format: every reason, counted or not, the
    /// preparations that found no sidecar, and the trees placed from their list for want of one.
    pub(in crate::server::local_cache) fn render(&self) -> String {
        let mut text = format!("# TYPE {REFUSED} counter\n");
        for (refusal, count) in REASONS.iter().zip(&self.refused) {
            let label = refusal.label();
            let _ = writeln!(text, "{REFUSED}{{reason=\"{label}\"}} {}", read(count));
        }
        let _ = writeln!(text, "# TYPE {SIDECAR_MISSING} counter");
        let _ = writeln!(text, "{SIDECAR_MISSING} {}", read(&self.missing));
        let _ = writeln!(text, "# TYPE {SIDECAR_FALLBACK} counter");
        let _ = writeln!(text, "{SIDECAR_FALLBACK} {}", read(&self.fallback));
        text
    }
}

impl LocalCache {
    /// The mount provider refused a tree's list for `refusal`: one preparation, counted once.
    pub(in crate::server) fn list_refused(&self, refusal: Refusal) {
        self.inner.starts.list_refusals.refused(refusal);
    }

    /// The mount provider found no L2 sidecar of a tree: one preparation, counted once.
    pub(in crate::server) fn sidecar_missing(&self) {
        bump(&self.inner.starts.list_refusals.missing);
    }

    /// The mount provider placed a tree from its list for want of an L2 sidecar: one tree placed,
    /// counted once.
    pub(in crate::server) fn sidecar_fallback(&self) {
        bump(&self.inner.starts.list_refusals.fallback);
    }
}

#[cfg(test)]
#[path = "list_refusals_tests.rs"]
mod tests;
