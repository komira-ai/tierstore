//! The counters of the OCI read front, shown by `GET /metricz` on the S3 listener.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::OciMode;

/// The calls the front tells apart in its counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RouteKind {
    Ping,
    Manifest,
    Blob,
    /// `/v2/<name>/tags/list`.
    Tags,
    /// The paths of uploads: where one begins, and one that has begun.
    Upload,
    Other,
}

/// The label of each route, in the order of [`RouteKind`].
const ROUTES: [&str; 6] = ["ping", "manifest", "blob", "tags", "upload", "other"];

/// How a request ended, by the status it was answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// 200.
    Ok,
    /// 201: a blob or a manifest was stored.
    Created,
    /// 202: an upload began, or took a chunk.
    Accepted,
    /// 204: an upload was cancelled.
    NoContent,
    /// 206.
    Partial,
    /// 404.
    NotFound,
    /// 400, 411 or 413: a request the front refused as malformed (a bad name or digest, bytes that
    /// are not their digest, a manifest that is invalid or names a blob that is not there, a
    /// body without a length where the store needs one, a manifest over the limit).
    Invalid,
    /// 405.
    NotAllowed,
    /// 416: a range of a blob that holds no byte of it, or a chunk of an upload that does not start
    /// where the upload is.
    Unsatisfiable,
    /// 401: no key of the server, or a wrong secret.
    Unauthorized,
    /// 403: a key of the server that lacks the right the call needs.
    Denied,
    /// 5xx: a store that failed, an object that is not what its name says.
    Failed,
    /// 409: a tag that another push moved first.
    Conflict,
}

/// The label of each outcome, in the order of [`Outcome`].
const OUTCOMES: [&str; 13] = [
    "ok",
    "created",
    "accepted",
    "no_content",
    "partial",
    "not_found",
    "invalid",
    "not_allowed",
    "range_not_satisfiable",
    "unauthorized",
    "denied",
    "failed",
    "conflict",
];

/// What the front has done since it started: requests by route and outcome, the bytes of blobs
/// sent, the objects whose bytes were not their digest, and the requests that came too late in a
/// drain.
#[derive(Debug, Default)]
pub struct OciMetrics {
    requests: [[AtomicU64; OUTCOMES.len()]; ROUTES.len()],
    bytes: AtomicU64,
    mismatches: AtomicU64,
    /// The re-checks of a pulled tag that failed, so that the stored tag was served.
    recheck_failures: AtomicU64,
    /// The connections whose client wrote after the front shut its end in a drain: the drain of
    /// the front's listener counts into it.
    late: Arc<AtomicU64>,
    mode: OciMode,
}

impl OciMetrics {
    /// Counters of a front that carries its connections as `mode` says. (`Default` is HTTPS.)
    #[must_use]
    pub fn with_mode(mode: OciMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Counts one request.
    pub(super) fn request(&self, route: RouteKind, outcome: Outcome) {
        self.requests[route as usize][outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts `count` bytes of a blob sent.
    pub(super) fn sent(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    /// Counts an object whose bytes were not the digest it is named by.
    pub(super) fn mismatch(&self) {
        self.mismatches.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a re-check of a pulled tag with the upstream that failed.
    pub(super) fn recheck_failed(&self) {
        self.recheck_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// The re-checks of a pulled tag that failed, each answered with the stored tag.
    #[must_use]
    pub fn recheck_failures(&self) -> u64 {
        self.recheck_failures.load(Ordering::Relaxed)
    }

    /// The requests counted for `route` and `outcome`.
    #[must_use]
    pub fn requests(&self, route: &str, outcome: &str) -> u64 {
        let route = ROUTES.iter().position(|name| *name == route);
        let outcome = OUTCOMES.iter().position(|name| *name == outcome);
        match (route, outcome) {
            (Some(route), Some(outcome)) => self.requests[route][outcome].load(Ordering::Relaxed),
            _ => 0,
        }
    }

    /// The bytes of blobs sent.
    #[must_use]
    pub fn bytes_sent(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The objects found not to be their digest.
    #[must_use]
    pub fn mismatches(&self) -> u64 {
        self.mismatches.load(Ordering::Relaxed)
    }

    /// The connections whose client wrote after the front had shut its end in a drain: the
    /// requests that came too late to be answered (see [`OciDrain`](super::OciDrain)).
    #[must_use]
    pub(crate) fn late_requests(&self) -> u64 {
        self.late.load(Ordering::SeqCst)
    }

    /// The counter of late requests, for the drain of the front's listener to count into.
    pub(super) fn late_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.late)
    }

    /// The counters in the Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str("# TYPE tierstore_oci_requests_total counter\n");
        for (r, route) in ROUTES.iter().enumerate() {
            for (o, outcome) in OUTCOMES.iter().enumerate() {
                let count = self.requests[r][o].load(Ordering::Relaxed);
                let labels = format!("route=\"{route}\",outcome=\"{outcome}\"");
                let _ = writeln!(text, "tierstore_oci_requests_total{{{labels}}} {count}");
            }
        }
        let bytes = self.bytes_sent();
        let _ = writeln!(text, "# TYPE tierstore_oci_bytes_served_total counter");
        let _ = writeln!(text, "tierstore_oci_bytes_served_total {bytes}");
        let mismatches = self.mismatches();
        let _ = writeln!(text, "# TYPE tierstore_oci_digest_mismatches_total counter");
        let _ = writeln!(text, "tierstore_oci_digest_mismatches_total {mismatches}");
        let rechecks = self.recheck_failures();
        let _ = writeln!(
            text,
            "# TYPE tierstore_oci_tag_recheck_failures_total counter"
        );
        let _ = writeln!(text, "tierstore_oci_tag_recheck_failures_total {rechecks}");
        let late = self.late_requests();
        let _ = writeln!(
            text,
            "# TYPE tierstore_oci_drain_late_requests_total counter"
        );
        let _ = writeln!(text, "tierstore_oci_drain_late_requests_total {late}");
        let mode = self.mode.as_str();
        let _ = writeln!(text, "# TYPE tierstore_oci_front_info gauge");
        let _ = writeln!(text, "tierstore_oci_front_info{{mode=\"{mode}\"}} 1");
        text
    }
}
