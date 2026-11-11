//! The counters of the local cache, shown by `GET /metricz` in the Prometheus text format.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Error;

use super::prefetch::Prefetched;

/// Why a fill did not leave an object in the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailedFill {
    /// A key named by its content whose bytes hash to another name.
    HashMismatch,
    /// The upstream did not give the object (down, refused, an error).
    UpstreamError,
    /// The upstream gave fewer bytes than the size it had said.
    ShortBody,
    /// The cache directory could not be written.
    LocalIo,
    /// An object that was written over while it was read.
    Changed,
    /// The task of the fill panicked or was cancelled before it ended.
    Aborted,
}

/// The label of each [`FailedFill`], in the order of the enum.
const FAILED: [&str; 6] = [
    "hash_mismatch",
    "upstream_error",
    "short_body",
    "local_io",
    "changed",
    "aborted",
];

/// Why a create-only put that the upstream took was not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SkippedKeep {
    /// No room: the object (or a body of no declared length, which cannot be reserved for) does
    /// not fit the budget or the critical mark.
    Budget,
    /// The stage directory or the cache directory could not be written.
    Io,
    /// Every keep the cache may run was running, or a fill of the key was.
    Busy,
    /// The upstream's HEAD after the put failed or did not describe the bytes that were sent, or
    /// the upstream did not read the body to its end.
    Head,
    /// The object was deleted while it was being kept.
    Deleted,
    /// The disk did not keep up with the body: the copy fell too far behind and was dropped,
    /// so that the upstream was never made to wait for the disk.
    Slow,
    /// The keep did not end within `keep_deadline` (a hung upstream or disk) and was abandoned.
    Deadline,
    /// The cache is read-only, or lost the lock of its directory while the keep ran: it writes
    /// nothing to a directory that another process holds.
    ReadOnly,
}

/// The label of each [`SkippedKeep`], in the order of the enum.
const SKIPPED_KEEP: [&str; 8] = [
    "budget",
    "io",
    "busy",
    "head",
    "deleted",
    "slow",
    "deadline",
    "read_only",
];

/// Why an object was evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Evicted {
    /// The cache was over its high water mark.
    Budget,
    /// The file system was below its free-space floor.
    Floor,
}

/// The label of each [`Evicted`], in the order of the enum.
const EVICTED: [&str; 2] = ["budget", "floor"];

/// What the check of an old hit against the upstream found (see `revalidate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Revalidated {
    /// The upstream holds the version the entry stands for.
    Same,
    /// The upstream holds another version: the entry was dropped.
    Changed,
    /// The upstream has no such object: the entry was dropped.
    Gone,
    /// The upstream could not be asked: the cached copy was served.
    Failed,
}

/// The label of each [`Revalidated`], in the order of the enum.
const REVALIDATED: [&str; 4] = ["same", "changed", "gone", "failed"];

/// Why a put or a completion of an upload failed at the upstream, or at the HEAD before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PutFailed {
    /// The upstream found the object: its create-only put or completion answered 412.
    Exists,
    /// The upstream did not answer.
    Unavailable,
    /// The upstream had no room (507).
    Full,
    /// The body did not hash to the name of the object.
    BadDigest,
    /// Any other failure.
    Other,
}

impl PutFailed {
    /// The reason an error of a put counts under.
    pub(super) fn of(error: &Error) -> Self {
        match error {
            Error::AlreadyExists(_) => Self::Exists,
            Error::Unavailable(_) => Self::Unavailable,
            Error::Full(_) => Self::Full,
            Error::BadDigest(_) => Self::BadDigest,
            _ => Self::Other,
        }
    }
}

/// The label of each [`PutFailed`], in the order of the enum.
const PUT_FAILED: [&str; 5] = ["exists", "unavailable", "full", "bad_digest", "other"];

/// The kind of call the cache sends the upstream that `upstream_reads_total` counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Call {
    /// A GET, whole or of a range.
    Get,
    /// A HEAD.
    Head,
    /// A listing, or one page of one.
    List,
    /// A page of the listing of open uploads.
    ListUploads,
}

/// The label of each [`Call`], in the order of the enum.
const CALLS: [&str; 4] = ["get", "head", "list", "list_uploads"];

/// Why a fill of the prefetch queue left nothing in the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrefetchFailed {
    /// The upstream failed.
    Upstream,
    /// The upstream has no such object.
    NotFound,
    /// The bytes are not the digest.
    BadDigest,
    /// No room: the cache would pass its high water mark, or its critical mark, for the blob.
    NoSpace,
    /// The object was written over or deleted while it was read.
    Changed,
    /// The blob waited in the queue for longer than `prefetch_queue_ttl`.
    Expired,
    /// Any other end of a fill: a short body, the disk, the lock lost, a task that did not end.
    Other,
}

/// The label of each [`PrefetchFailed`], in the order of the enum.
const PREFETCH_FAILED: [&str; 7] = [
    "upstream",
    "not_found",
    "bad_digest",
    "no_space",
    "changed",
    "expired",
    "other",
];

/// The label of each [`Prefetched`] (what became of a blob handed to the queue), in the order of
/// the enum, then of each [`Turned`] (a blob the expansion of a tree did not hand to the queue).
const BLOBS: [&str; 11] = [
    "present",
    "joined",
    "queued",
    "too_large",
    "busy",
    "off",
    "read_only",
    "too_many_blobs",
    "too_many_bytes",
    "reserved_key",
    "access_denied",
];

/// How many of the labels of [`BLOBS`] are those of [`Prefetched`]; the rest are [`Turned`]'s.
const HANDED: usize = 7;

/// Why a blob of a tree was not handed to the queue at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Turned {
    /// The call had queued `hydrate_max_blobs` blobs already.
    TooManyBlobs,
    /// The blob would take the call past `hydrate_max_bytes`.
    TooManyBytes,
    /// The blob's `source.key` is under a prefix the proxy keeps for itself.
    ReservedKey,
    /// The blob's source key is one the key that made the call may not read.
    AccessDenied,
}

/// How the expansion of a tree ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Walked {
    /// Every blob of the tree was handed to the queue.
    Expanded,
    /// The tree was not expanded: a manifest was refused or could not be fetched.
    Refused,
    /// The walk stopped at a bound of the call, or because its hold ended, or never began for it.
    Cut,
    /// The key that made the call may not read a manifest of the tree: it was not expanded.
    AccessDenied,
}

/// The label of each [`Walked`], in the order of the enum.
const TREES: [&str; 4] = ["expanded", "refused", "cut", "access_denied"];

/// The label of the path of each read that joined a fill that was running, in the order of
/// [`Counters::joined`]'s argument: the fill socket (the mount), then the S3 front.
const JOIN_PATHS: [&str; 2] = ["mount", "s3"];

/// The kind of a family that only goes up (or is set once, when the cache is opened).
const COUNTER: &str = "counter";

/// The kind of a family that is measured when `/metricz` is read, and goes up and down.
const GAUGE: &str = "gauge";

// The full name of each family, written out so that a search of the source for a name the
// metrics page shows finds it.
const HITS: &str = "tierstore_cache_hits_total";
const MISSES: &str = "tierstore_cache_misses_total";
const BYTES_SERVED_LOCAL: &str = "tierstore_cache_bytes_served_local_total";
const BYTES_SERVED_UPSTREAM: &str = "tierstore_cache_bytes_served_upstream_total";
const FILLS: &str = "tierstore_cache_fills_total";
const FILLS_SKIPPED: &str = "tierstore_cache_fills_skipped_total";
const FILLS_SKIPPED_BUSY: &str = "tierstore_cache_fills_skipped_busy_total";
const UPSTREAM_BYTES_READ: &str = "tierstore_cache_upstream_bytes_read_total";
const HEAD_HITS: &str = "tierstore_cache_head_hits_total";
const HEAD_MISSES: &str = "tierstore_cache_head_misses_total";
const UPSTREAM_PUTS: &str = "tierstore_cache_upstream_puts_total";
const UPSTREAM_PUT_BYTES: &str = "tierstore_cache_upstream_put_bytes_total";
const WRITES_THROUGH: &str = "tierstore_cache_writes_through_total";
const WRITE_EXISTS: &str = "tierstore_cache_write_exists_total";
const KEEPS_EXPIRED: &str = "tierstore_cache_keeps_expired_total";
const PINNED_SKIPS: &str = "tierstore_cache_pinned_skips_total";
const PIN_SKIPS: &str = "tierstore_cache_pin_skips_total";
const SCRUBBED_OBJECTS: &str = "tierstore_cache_scrubbed_objects_total";
const SCRUBBED_BYTES: &str = "tierstore_cache_scrubbed_bytes_total";
const SCRUB_MISMATCHES: &str = "tierstore_cache_scrub_mismatches_total";
const SCRUB_ERRORS: &str = "tierstore_cache_scrub_errors_total";
const DISCARDED_AT_START: &str = "tierstore_cache_discarded_at_start_total";
const BYTES: &str = "tierstore_cache_bytes";
const OBJECTS: &str = "tierstore_cache_objects";
const FS_FREE_BYTES: &str = "tierstore_cache_fs_free_bytes";
const STAGING_FILES: &str = "tierstore_cache_staging_files";
const FILL_FAILURES: &str = "tierstore_cache_fill_failures_total";
const UPSTREAM_READS: &str = "tierstore_cache_upstream_reads_total";
const UPSTREAM_PUT_FAILURES: &str = "tierstore_cache_upstream_put_failures_total";
const WRITE_KEEP_SKIPPED: &str = "tierstore_cache_write_keep_skipped_total";
const EVICTIONS: &str = "tierstore_cache_evictions_total";
const REVALIDATIONS: &str = "tierstore_cache_revalidations_total";
const MODE: &str = "tierstore_cache_mode";
const PREFETCH_FILLS: &str = "tierstore_cache_prefetch_fills_total";
const PREFETCH_PROMOTIONS: &str = "tierstore_cache_prefetch_promotions_total";
const PREFETCH_QUEUE_DEPTH: &str = "tierstore_cache_prefetch_queue_depth";
const HOLDS_OPEN: &str = "tierstore_cache_holds_open";
const HYDRATE_REQUESTS: &str = "tierstore_cache_hydrate_requests_total";
const HYDRATE_BLOBS: &str = "tierstore_cache_hydrate_blobs_total";
const HYDRATE_TREES: &str = "tierstore_cache_hydrate_trees_total";
const PREFETCH_FILL_FAILURES: &str = "tierstore_cache_prefetch_fill_failures_total";
const FILL_JOINS: &str = "tierstore_cache_fill_joins_total";

/// The families of one sample, with their kinds, in the order `/metricz` shows them.
pub(super) const PLAIN: [(&str, &str); 31] = [
    (HITS, COUNTER),
    (MISSES, COUNTER),
    (BYTES_SERVED_LOCAL, COUNTER),
    (BYTES_SERVED_UPSTREAM, COUNTER),
    (FILLS, COUNTER),
    (FILLS_SKIPPED, COUNTER),
    (FILLS_SKIPPED_BUSY, COUNTER),
    (UPSTREAM_BYTES_READ, COUNTER),
    (HEAD_HITS, COUNTER),
    (HEAD_MISSES, COUNTER),
    (UPSTREAM_PUTS, COUNTER),
    (UPSTREAM_PUT_BYTES, COUNTER),
    (WRITES_THROUGH, COUNTER),
    (WRITE_EXISTS, COUNTER),
    (KEEPS_EXPIRED, COUNTER),
    (PINNED_SKIPS, COUNTER),
    (PIN_SKIPS, COUNTER),
    (SCRUBBED_OBJECTS, COUNTER),
    (SCRUBBED_BYTES, COUNTER),
    (SCRUB_MISMATCHES, COUNTER),
    (SCRUB_ERRORS, COUNTER),
    (DISCARDED_AT_START, COUNTER),
    (BYTES, GAUGE),
    (OBJECTS, GAUGE),
    (FS_FREE_BYTES, GAUGE),
    (STAGING_FILES, GAUGE),
    (PREFETCH_FILLS, COUNTER),
    (PREFETCH_PROMOTIONS, COUNTER),
    (PREFETCH_QUEUE_DEPTH, GAUGE),
    (HOLDS_OPEN, GAUGE),
    (HYDRATE_REQUESTS, COUNTER),
];

/// The families of one sample for each value of a label: the name, the kind, the name of the
/// label and its values, in the order `/metricz` shows them, after [`PLAIN`].
pub(super) const LABELLED: [(&str, &str, &str, &[&str]); 11] = [
    (FILL_FAILURES, COUNTER, "reason", &FAILED),
    (UPSTREAM_READS, COUNTER, "op", &CALLS),
    (UPSTREAM_PUT_FAILURES, COUNTER, "reason", &PUT_FAILED),
    (WRITE_KEEP_SKIPPED, COUNTER, "reason", &SKIPPED_KEEP),
    (EVICTIONS, COUNTER, "reason", &EVICTED),
    (REVALIDATIONS, COUNTER, "result", &REVALIDATED),
    (MODE, GAUGE, "mode", &MODES),
    (HYDRATE_BLOBS, COUNTER, "outcome", &BLOBS),
    (HYDRATE_TREES, COUNTER, "outcome", &TREES),
    (PREFETCH_FILL_FAILURES, COUNTER, "reason", &PREFETCH_FAILED),
    (FILL_JOINS, COUNTER, "path", &JOIN_PATHS),
];

/// The label of each mode of the cache, in the order `/metricz` shows them.
const MODES: [&str; 2] = ["read_only", "read_write"];

/// What the cache has done since it started.
#[derive(Debug, Default)]
pub struct Counters {
    /// The calls the cache sent the upstream, by [`Call`] (a fill reads in ranged chunks, so the
    /// GETs are calls, not objects).
    upstream_reads: [AtomicU64; CALLS.len()],
    /// The bytes the cache's upstream GETs that returned brought back.
    pub(super) upstream_bytes_read: AtomicU64,
    /// The HEADs the cache is asked that the index answered: the HEADs of clients, and the S3
    /// front's HEAD before a GET.
    pub(super) head_hits: AtomicU64,
    /// The same HEADs, sent to the upstream.
    pub(super) head_misses: AtomicU64,
    /// The puts (create-only or plain) and the completions of uploads the upstream accepted.
    pub(super) upstream_puts: AtomicU64,
    /// The bytes of the puts the upstream accepted, and of each part it accepted.
    pub(super) upstream_put_bytes: AtomicU64,
    put_failures: [AtomicU64; PUT_FAILED.len()],
    pub(super) hits: AtomicU64,
    pub(super) misses: AtomicU64,
    pub(super) bytes_served_local: AtomicU64,
    pub(super) bytes_served_upstream: AtomicU64,
    pub(super) fills: AtomicU64,
    /// The fills not started or not kept for a reason that is not an error (no room, or busy).
    pub(super) fills_skipped: AtomicU64,
    /// Of `fills_skipped`, those skipped because every fill the cache may run was running.
    pub(super) fills_skipped_busy: AtomicU64,
    /// The create-only puts the upstream took.
    pub(super) writes_through: AtomicU64,
    /// The create-only puts answered `AlreadyExists`.
    pub(super) write_exists: AtomicU64,
    /// The puts answered before their keep ended, because it took longer than `keep_wait`.
    pub(super) keeps_expired: AtomicU64,
    keep_skips: [AtomicU64; SKIPPED_KEEP.len()],
    failures: [AtomicU64; FAILED.len()],
    evictions: [AtomicU64; EVICTED.len()],
    revalidations: [AtomicU64; REVALIDATED.len()],
    pub(super) pinned_skips: AtomicU64,
    /// The objects an eviction pass passed over because a tree pins them.
    pub(super) pin_skips: AtomicU64,
    /// The objects the scrub hashed again, and the bytes it read for them.
    pub(super) scrubbed_objects: AtomicU64,
    pub(super) scrubbed_bytes: AtomicU64,
    /// The objects the scrub dropped because their bytes did not match.
    pub(super) scrub_mismatches: AtomicU64,
    /// The objects the scrub could not judge because a read failed.
    pub(super) scrub_errors: AtomicU64,
    pub(super) discarded_at_start: AtomicU64,
    /// The files in the stage directory now.
    pub(super) staging_files: AtomicU64,
    /// The fills of the prefetch queue that left their blob in the cache.
    pub(super) prefetch_fills: AtomicU64,
    /// The queued blobs that a read started before a worker did.
    pub(super) prefetch_promotions: AtomicU64,
    /// The holds open now (a gauge, set when a hold opens or ends).
    pub(super) holds_open: AtomicU64,
    /// The hydrate calls the cache accepted.
    pub(super) hydrate_requests: AtomicU64,
    /// The blobs handed to the queue, by [`Prefetched`].
    blobs: [AtomicU64; BLOBS.len()],
    /// The trees expanded, by [`Walked`].
    trees: [AtomicU64; TREES.len()],
    /// The fills of the prefetch queue that left nothing, by [`PrefetchFailed`].
    prefetch_failures: [AtomicU64; PREFETCH_FAILED.len()],
    /// The reads that joined a fill that was running instead of starting one, by path.
    joins: [AtomicU64; JOIN_PATHS.len()],
}

/// One more of `counter`.
pub(super) fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_add(1, Ordering::Relaxed);
}

/// `by` more of `counter`.
pub(super) fn add(counter: &AtomicU64, by: u64) {
    let _ = counter.fetch_add(by, Ordering::Relaxed);
}

/// The value of `counter`.
pub(super) fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

impl Counters {
    /// The files removed at start: those the scan did not find whole, and what a fill that was
    /// running at the last stop left in the stage directory.
    #[must_use]
    pub fn discarded_at_start(&self) -> u64 {
        read(&self.discarded_at_start)
    }

    /// Counts a fill that failed for `why`.
    pub(super) fn failed(&self, why: FailedFill) {
        bump(&self.failures[why as usize]);
    }

    /// Counts a call of `kind` sent to the upstream.
    pub(super) fn called(&self, kind: Call) {
        bump(&self.upstream_reads[kind as usize]);
    }

    /// The calls of `kind` sent to the upstream.
    #[cfg(test)]
    pub(super) fn calls(&self, kind: Call) -> u64 {
        read(&self.upstream_reads[kind as usize])
    }

    /// Counts a put or a completion that failed with `error`, under the reason it maps to.
    pub(super) fn put_failed(&self, error: &Error) {
        bump(&self.put_failures[PutFailed::of(error) as usize]);
    }

    /// The puts and completions that failed for `why`.
    #[cfg(test)]
    pub(super) fn put_failures(&self, why: PutFailed) -> u64 {
        read(&self.put_failures[why as usize])
    }

    /// Counts a put that was taken by the upstream and not kept, for `why`.
    pub(super) fn keep_skipped(&self, why: SkippedKeep) {
        bump(&self.keep_skips[why as usize]);
    }

    /// The puts that were not kept for `why`.
    #[cfg(test)]
    pub(super) fn keeps_skipped(&self, why: SkippedKeep) -> u64 {
        read(&self.keep_skips[why as usize])
    }

    /// Counts a check of an old hit that found `found`.
    pub(super) fn revalidated(&self, found: Revalidated) {
        bump(&self.revalidations[found as usize]);
    }

    /// The checks of old hits that found `found`.
    #[cfg(test)]
    pub(super) fn revalidations(&self, found: Revalidated) -> u64 {
        read(&self.revalidations[found as usize])
    }

    /// Counts an eviction for `why`.
    pub(super) fn evicted(&self, why: Evicted) {
        bump(&self.evictions[why as usize]);
    }

    /// The fills that failed for `why`.
    #[cfg(test)]
    pub(super) fn failures(&self, why: FailedFill) -> u64 {
        read(&self.failures[why as usize])
    }

    /// The evictions for `why`.
    #[cfg(test)]
    pub(super) fn evictions(&self, why: Evicted) -> u64 {
        read(&self.evictions[why as usize])
    }

    /// Counts a blob handed to the queue that became `outcome`.
    pub(super) fn blob(&self, outcome: Prefetched) {
        bump(&self.blobs[outcome as usize]);
    }

    /// The blobs handed to the queue that became `outcome`.
    #[cfg(test)]
    pub(super) fn blobs(&self, outcome: Prefetched) -> u64 {
        read(&self.blobs[outcome as usize])
    }

    /// Counts a blob of a tree that was not handed to the queue, for `why`.
    pub(super) fn turned(&self, why: Turned) {
        bump(&self.blobs[HANDED + why as usize]);
    }

    /// The blobs of trees that were not handed to the queue, for `why`.
    #[cfg(test)]
    pub(super) fn turned_count(&self, why: Turned) -> u64 {
        read(&self.blobs[HANDED + why as usize])
    }

    /// Counts a hydrate call that was accepted.
    pub(super) fn hydrated(&self) {
        bump(&self.hydrate_requests);
    }

    /// Counts a tree whose expansion ended as `how`.
    pub(super) fn walked(&self, how: Walked) {
        bump(&self.trees[how as usize]);
    }

    /// The trees whose expansion ended as `how`.
    #[cfg(test)]
    pub(super) fn trees(&self, how: Walked) -> u64 {
        read(&self.trees[how as usize])
    }

    /// Counts a fill of the queue that left nothing, for `why`.
    pub(super) fn prefetch_failed(&self, why: PrefetchFailed) {
        bump(&self.prefetch_failures[why as usize]);
    }

    /// The fills of the queue that left nothing, for `why`.
    #[cfg(test)]
    pub(super) fn prefetch_failures(&self, why: PrefetchFailed) -> u64 {
        read(&self.prefetch_failures[why as usize])
    }

    /// Counts a read that joined a fill that was running, of the S3 front if `s3`, else of the
    /// fill socket.
    pub(super) fn joined(&self, s3: bool) {
        bump(&self.joins[usize::from(s3)]);
    }

    /// The reads of the S3 front (`s3`) or of the fill socket that joined a running fill.
    #[cfg(test)]
    pub(super) fn joins(&self, s3: bool) -> u64 {
        read(&self.joins[usize::from(s3)])
    }

    /// The text of `/metricz`, with the gauges the caller measured now. Each family has one
    /// `# TYPE` line, before its first sample, as the OCI front's have.
    pub(super) fn render(
        &self,
        cache_bytes: u64,
        cache_objects: u64,
        fs_free_bytes: u64,
        read_write: bool,
        queue_depth: u64,
    ) -> String {
        let mut text = String::new();
        // In the order of `PLAIN`, which holds the name and the kind of each.
        let plain = [
            read(&self.hits),
            read(&self.misses),
            read(&self.bytes_served_local),
            read(&self.bytes_served_upstream),
            read(&self.fills),
            read(&self.fills_skipped),
            read(&self.fills_skipped_busy),
            read(&self.upstream_bytes_read),
            read(&self.head_hits),
            read(&self.head_misses),
            read(&self.upstream_puts),
            read(&self.upstream_put_bytes),
            read(&self.writes_through),
            read(&self.write_exists),
            read(&self.keeps_expired),
            read(&self.pinned_skips),
            read(&self.pin_skips),
            read(&self.scrubbed_objects),
            read(&self.scrubbed_bytes),
            read(&self.scrub_mismatches),
            read(&self.scrub_errors),
            // Set once, when the cache is opened (and when it takes the lock), and the same after.
            read(&self.discarded_at_start),
            cache_bytes,
            cache_objects,
            fs_free_bytes,
            read(&self.staging_files),
            read(&self.prefetch_fills),
            read(&self.prefetch_promotions),
            queue_depth,
            read(&self.holds_open),
            read(&self.hydrate_requests),
        ];
        for ((name, kind), value) in PLAIN.iter().zip(plain) {
            let _ = writeln!(text, "# TYPE {name} {kind}");
            let _ = writeln!(text, "{name} {value}");
        }
        // One series for each mode: 1 for the one the cache is in, 0 for the other.
        let modes = [u64::from(!read_write), u64::from(read_write)];
        let values: [Vec<u64>; LABELLED.len()] = [
            self.failures.iter().map(read).collect(),
            self.upstream_reads.iter().map(read).collect(),
            self.put_failures.iter().map(read).collect(),
            self.keep_skips.iter().map(read).collect(),
            self.evictions.iter().map(read).collect(),
            self.revalidations.iter().map(read).collect(),
            modes.to_vec(),
            self.blobs.iter().map(read).collect(),
            self.trees.iter().map(read).collect(),
            self.prefetch_failures.iter().map(read).collect(),
            self.joins.iter().map(read).collect(),
        ];
        for ((name, kind, label, labels), values) in LABELLED.iter().zip(values) {
            let _ = writeln!(text, "# TYPE {name} {kind}");
            for (value_label, value) in labels.iter().zip(values) {
                let _ = writeln!(text, "{name}{{{label}=\"{value_label}\"}} {value}");
            }
        }
        text
    }
}
