//! How long the requests of the frontend take, by kind: what the mount tells the node for
//! `tierstore_fs_request_seconds` (`local_cache/requests.rs`).
//!
//! A request is timed from its arrival to its answer, on the core's clock: the frontend takes an
//! [`Arrival`] when the request arrives ([`Core::request_begins`]) and gives it back with the
//! request's kind when the request is answered ([`Core::request_ends`]). Over FUSE that is every
//! request the session hands the frontend, by its opcode, whether it is answered on the reader
//! that read it or parked and answered by a thread of the fetches (`node_fs.rs`); over NFS it is
//! every call of the protocol layer into the file system (`nfs/fs.rs`). A request that is never
//! answered (its parked job dropped) is not counted.
//!
//! No request takes a lock or allocates for this: an arrival is a reading of the clock, and its
//! end is a count more in one bucket, the milliseconds added and a count more of the requests
//! answered, each on an atomic of its own. The thread that looks for idle trees reads the atomics
//! into a report at each look, and so does the stop of the readers for a hand-over, but only when
//! a request was answered since the report before: the running totals since the process started,
//! so that a report lost loses nothing the next one carries (`Core::look_around`,
//! `NodeFs::drain`, `Running::hand_to_image`).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::server::local_cache::attaches::{BUCKETS, bucket_of};
use crate::server::local_cache::opens::Spans;
use crate::server::local_cache::requests::{KINDS, RequestKind, Requests};

use super::super::fills::Report;
use super::Core;

/// The requests of this process, by kind of [`KINDS`]. The totals of a kind are atomics read and
/// written with no order to one another; `answered` is counted after them and read before them,
/// so a report holds at least the requests `answered` counts.
#[derive(Debug, Default)]
pub(in crate::server::treefs) struct RequestTimes {
    /// The milliseconds of the requests of each kind, in all.
    ms: [AtomicU64; KINDS.len()],
    /// How many requests of each kind fell in each bucket, not cumulated.
    buckets: [[AtomicU64; BUCKETS]; KINDS.len()],
    /// The requests answered, of every kind.
    answered: AtomicU64,
    /// `answered` when the node was last told.
    told: AtomicU64,
}

impl RequestTimes {
    /// The totals of each kind.
    fn totals(&self) -> [Spans; KINDS.len()] {
        let mut spans = [Spans::default(); KINDS.len()];
        for ((spans, ms), buckets) in spans.iter_mut().zip(&self.ms).zip(&self.buckets) {
            spans.ms = ms.load(Ordering::Relaxed);
            for (count, bucket) in spans.buckets.iter_mut().zip(buckets) {
                *count = bucket.load(Ordering::Relaxed);
            }
        }
        spans
    }
}

/// When a request arrived, on the core's clock.
#[derive(Clone, Copy, Debug)]
pub(in crate::server::treefs) struct Arrival {
    begun: u64,
}

impl Core {
    /// A request that arrives now: it is counted when it is given back to [`Core::request_ends`].
    pub(in crate::server::treefs) fn request_begins(&self) -> Arrival {
        Arrival { begun: self.now() }
    }

    /// The request that arrived at `arrival` is answered now: its span is counted under `kind`.
    pub(in crate::server::treefs) fn request_ends(&self, arrival: Arrival, kind: RequestKind) {
        let ms = self.now().saturating_sub(arrival.begun);
        let times = &self.requests;
        let at = kind.index();
        let _ = times.ms[at].fetch_add(ms, Ordering::Relaxed);
        let _ = times.buckets[at][bucket_of(ms)].fetch_add(1, Ordering::Relaxed);
        let _ = times.answered.fetch_add(1, Ordering::Release);
    }

    /// What this process tells the node of its requests: the totals since it started.
    pub(in crate::server::treefs) fn requests_report(&self) -> Requests {
        Requests {
            mount: self.settings.mount_id.clone(),
            process: u64::from(std::process::id()),
            spans: self.requests.totals(),
        }
    }

    /// Tells the node [`Core::requests_report`] if a request was answered since it was last told,
    /// and nothing otherwise; the caller does not wait.
    pub(in crate::server::treefs) fn report_requests(&self) {
        let times = &self.requests;
        let answered = times.answered.load(Ordering::Acquire);
        if times.told.swap(answered, Ordering::AcqRel) == answered {
            return;
        }
        self.fills
            .report(Report::Requests(Box::new(self.requests_report())));
    }
}

#[cfg(test)]
#[path = "requests_tests.rs"]
mod tests;
