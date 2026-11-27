//! The families of `/metricz` that the front counts itself, whatever it serves from: the requests
//! that came too late in a drain, and the puts of an object whose body it streams to the backend
//! (`put_streamed`, in `ops`) that failed, by the reason the answer to the client gives.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Server;
use super::reply::{Answer, S3Error};

/// The family of the failed puts.
const PUT_FAILURES: &str = "tierstore_s3_put_failures_total";

/// Why a put failed, by the code of the answer, in the order of [`REASONS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PutFailure {
    /// The body stopped coming (`RequestTimeout`: no byte within `body_idle_timeout`, or slower
    /// than `put_min_bytes_per_sec`).
    Timeout,
    /// The body ended before its length, or could not be read (`IncompleteBody`): a client that
    /// went away.
    Incomplete,
    /// The body is not the digest its request named (`BadDigest`, `XAmzContentSHA256Mismatch`,
    /// `InvalidDigest`).
    BadDigest,
    /// The body is longer than the limit (`EntityTooLarge`).
    TooLarge,
    /// The server took as many bodies as it may, or the store had no room (`SlowDown`).
    Busy,
    /// The key exists, for a create-only put (`PreconditionFailed`).
    Exists,
    /// The store did not answer, or failed (`ServiceUnavailable`, `InternalError`).
    Upstream,
    /// Any other refusal.
    Other,
}

/// The label of each [`PutFailure`], in the order of the enum.
const REASONS: [&str; 8] = [
    "timeout",
    "incomplete",
    "bad_digest",
    "too_large",
    "busy",
    "exists",
    "upstream",
    "other",
];

impl PutFailure {
    /// The reason of an answer with the S3 error code `code`.
    pub(super) fn of(code: &str) -> Self {
        match code {
            "RequestTimeout" => Self::Timeout,
            "IncompleteBody" => Self::Incomplete,
            "BadDigest" | "XAmzContentSHA256Mismatch" | "InvalidDigest" => Self::BadDigest,
            "EntityTooLarge" => Self::TooLarge,
            "SlowDown" => Self::Busy,
            "PreconditionFailed" => Self::Exists,
            "ServiceUnavailable" | "InternalError" => Self::Upstream,
            _ => Self::Other,
        }
    }
}

/// The counts of the front.
#[derive(Debug, Default)]
pub(super) struct FrontCounts {
    put_failures: [AtomicU64; REASONS.len()],
}

impl FrontCounts {
    /// The failed puts of `why` so far.
    #[cfg(test)]
    pub(super) fn put_failures(&self, why: PutFailure) -> u64 {
        self.put_failures[why as usize].load(Ordering::Relaxed)
    }
}

impl Server {
    /// `put`, the answer to a streamed put, counted if it is a failure.
    pub(super) fn counted_put(&self, put: Result<Answer, S3Error>) -> Result<Answer, S3Error> {
        if let Err(error) = &put {
            let why = PutFailure::of(error.code());
            self.counts.put_failures[why as usize].fetch_add(1, Ordering::Relaxed);
        }
        put
    }

    /// The connections whose peer wrote after the server shut its end, in the drain: the
    /// requests that came too late to be answered.
    pub(super) fn late_requests(&self) -> u64 {
        self.drain.late()
    }

    /// The front's families of `/metricz` (module docs): one series of the failed puts for each
    /// reason.
    pub(super) fn front_families(&self) -> String {
        let late = self.late_requests();
        let mut text = String::from("# TYPE tierstore_drain_late_requests_total counter\n");
        let _ = writeln!(text, "tierstore_drain_late_requests_total {late}");
        let _ = writeln!(text, "# TYPE {PUT_FAILURES} counter");
        for (reason, count) in REASONS.iter().zip(&self.counts.put_failures) {
            let count = count.load(Ordering::Relaxed);
            let _ = writeln!(text, "{PUT_FAILURES}{{reason=\"{reason}\"}} {count}");
        }
        text
    }
}
