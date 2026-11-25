//! How long the opens of files of the trees take, and how many wait now: what the mount tells the
//! node for `tierstore_fs_open_seconds` and `tierstore_fs_opens_waiting`
//! (`local_cache/opens.rs`).
//!
//! An open is timed from the arrival of its request to its answer, on the core's clock: the
//! frontend takes a [`Timing`] when the request arrives ([`Core::open_begins`]) and gives it back
//! when the open is answered ([`Core::open_ends`]). For FUSE that is an OPEN, whether it is
//! answered on the reader that read it or parked and answered by a thread of the fetches
//! (`node_fs.rs`); NFS has no open, and its open is the fill of the descriptor a first read of a
//! file asks for, from that read's arrival to the descriptor or its failure (`nfs/fs.rs`). The
//! open is `filled` when it asked the node for the file's bytes (a fill of the blob, or the span
//! of a pack) and `cached` when it did not; `ok` when it was answered with a handle, `error` when
//! it was not.
//!
//! An open waits from its [`Timing`] to the drop of it, which comes when it is answered, and also
//! when it is never answered (its request was dropped, or the thread that held it panicked), so
//! the count of the opens waiting falls back to zero whatever becomes of them.
//!
//! No open takes a lock or allocates for this: a timing is a count more of the opens waiting and a
//! clone of an [`Arc`]; its end is a count more in one bucket, the milliseconds added, and a count
//! fewer waiting, each on an atomic of its own. The thread that looks for idle trees reads the
//! atomics into a report at each look, and so does the stop of the readers for a hand-over: the
//! running totals since the process started, so that a report lost loses nothing the next one
//! carries (`Core::look_around`, `NodeFs::drain`, `Running::hand_to_image`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::server::local_cache::attaches::{BUCKETS, bucket_of};
use crate::server::local_cache::opens::{KINDS, Opens, Outcome, Source, Spans, kind};

use super::super::fills::Report;
use super::Core;

/// The opens of this process, by kind of [`KINDS`], and the opens waiting now. Every number is
/// an atomic of its own, read and written with no order to the others: a report may hold an open
/// in its bucket and not yet in its milliseconds, which the next report carries.
#[derive(Debug, Default)]
pub(in crate::server::treefs) struct OpenTimes {
    /// The milliseconds of the opens of each kind, in all.
    ms: [AtomicU64; KINDS.len()],
    /// How many opens of each kind fell in each bucket, not cumulated.
    buckets: [[AtomicU64; BUCKETS]; KINDS.len()],
    /// The opens begun and not answered.
    waiting: AtomicU64,
}

impl OpenTimes {
    /// The opens waiting now, and the totals of each kind.
    fn totals(&self) -> (u64, [Spans; KINDS.len()]) {
        let mut spans = [Spans::default(); KINDS.len()];
        for ((spans, ms), buckets) in spans.iter_mut().zip(&self.ms).zip(&self.buckets) {
            spans.ms = ms.load(Ordering::Relaxed);
            for (count, bucket) in spans.buckets.iter_mut().zip(buckets) {
                *count = bucket.load(Ordering::Relaxed);
            }
        }
        (self.waiting.load(Ordering::Relaxed), spans)
    }
}

/// One open, from the arrival of its request: waiting until it is dropped.
#[derive(Debug)]
pub(in crate::server::treefs) struct Timing {
    times: Arc<OpenTimes>,
    /// When the request arrived, on the core's clock: the open's fills are answered by
    /// `fs.fetch_timeout` after it (`core/blob.rs`).
    pub begun: u64,
    /// Whether the open asked the node for the file's bytes: set by the open.
    pub source: Source,
}

impl Drop for Timing {
    fn drop(&mut self) {
        let _ = self.times.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Core {
    /// An open whose request arrives now: it waits until the timing is dropped, and is counted
    /// when it is given back to [`Core::open_ends`].
    pub(in crate::server::treefs) fn open_begins(&self) -> Timing {
        let _ = self.opens.waiting.fetch_add(1, Ordering::Relaxed);
        Timing {
            times: Arc::clone(&self.opens),
            begun: self.now(),
            source: Source::Cached,
        }
    }

    /// The open of `timing` is answered now, with a handle if `ok`: its span is counted under its
    /// outcome and source, and it no longer waits.
    pub(in crate::server::treefs) fn open_ends(&self, timing: Timing, ok: bool) {
        let ms = self.now().saturating_sub(timing.begun);
        let outcome = if ok { Outcome::Ok } else { Outcome::Error };
        let at = kind(outcome, timing.source);
        let _ = self.opens.ms[at].fetch_add(ms, Ordering::Relaxed);
        let _ = self.opens.buckets[at][bucket_of(ms)].fetch_add(1, Ordering::Relaxed);
    }

    /// What this process tells the node of its opens: the opens waiting now and the totals since
    /// it started.
    pub(in crate::server::treefs) fn opens_report(&self) -> Opens {
        let (waiting, spans) = self.opens.totals();
        Opens {
            mount: self.settings.mount_id.clone(),
            process: u64::from(std::process::id()),
            waiting,
            spans,
        }
    }

    /// Tells the node [`Core::opens_report`]; the caller does not wait.
    pub(in crate::server::treefs) fn report_opens(&self) {
        self.fills.report(Report::Opens(self.opens_report()));
    }
}

#[cfg(test)]
#[path = "opens_tests.rs"]
mod tests;
