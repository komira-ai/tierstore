//! `DriveSched`: the per-drive I/O scheduler of a hard drive (10.7). T-S5 implements it.
//!
//! A drive's scheduler runs on dedicated blocking I/O threads, `drive.streams x drive.depth` of
//! them (2 x 2 = 4 by default), never the async runtime's blocking pool. It admits at most
//! `drive.streams` streams at once, of which at most `drive.write_streams` write; a free slot goes
//! to `demand` > `write` > `prepare` > `background`, deficit round robin across jobs within a
//! class, then `(rank, arrival)`; a waiting write gets at least one slice in four
//! (`drive.write_min_share`); a stream keeps its slot for `slice_bytes` and yields only to an
//! equal or higher class; `demand` preempts a lower class at a chunk boundary, at most
//! `demand_preempt_max` per second; one small record read runs on an idle thread or at the next
//! chunk boundary without a slot (the small lane). Each read stream has a two-chunk read-ahead
//! buffer; a stream whose buffer is full and whose socket accepted nothing for
//! `slot_release_after` parks and gives up its slot. A watchdog puts the drive in `stalled` when
//! any call is outstanding past `stall_after` (on the injected clock); `eio_limit` errors on
//! distinct files in 10 minutes, or three stalls in an hour, make it `failed`.

use bytes::Bytes;
use futures::stream::BoxStream;
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, Rank};
use tierstore_fetch_api::upstream::DriveState;
use tierstore_format::names::ColdName;

/// A class of the drive's queues (10.7): `demand` > `write` > `prepare` > `background`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DriveClass {
    /// A read a job waits for.
    Demand,
    /// A shard write of destage or rebuild.
    Write,
    /// Prepare reads, and the rebuild of a stripe with no spare shard.
    Prepare,
    /// Scrub, GC and promote reads, non-urgent rebuild reads.
    Background,
}

impl From<Class> for DriveClass {
    fn from(c: Class) -> Self {
        match c {
            Class::Demand => Self::Demand,
            Class::Prepare => Self::Prepare,
            Class::Background => Self::Background,
        }
    }
}

/// What a request does on the drive.
pub enum IoKind {
    /// A sequential read of a range of one file: a stream.
    Read {
        /// The file.
        file: ColdName,
        /// The bytes of the file.
        range: ByteRange,
    },
    /// A read of one small record: the small lane, no stream slot.
    SmallRead {
        /// The file.
        file: ColdName,
        /// The record's bytes.
        range: ByteRange,
    },
    /// A whole-file sequential write (`.up`, `fallocate`, 4 MiB `O_DIRECT` chunks, footer, `fsync`,
    /// rename, directory `fsync`).
    Write {
        /// The file's final name.
        file: ColdName,
        /// Its length.
        len: u64,
        /// The bytes, streamed once.
        body: BoxStream<'static, Result<Bytes, std::io::Error>>,
    },
}

impl std::fmt::Debug for IoKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { file, range } => f
                .debug_struct("Read")
                .field("file", file)
                .field("range", range)
                .finish(),
            Self::SmallRead { file, range } => f
                .debug_struct("SmallRead")
                .field("file", file)
                .field("range", range)
                .finish(),
            Self::Write { file, len, .. } => f
                .debug_struct("Write")
                .field("file", file)
                .field("len", len)
                .finish_non_exhaustive(),
        }
    }
}

/// A request to the drive.
#[derive(Debug)]
pub struct IoRequest {
    /// Its class.
    pub class: DriveClass,
    /// The job (`x-ts-job`), for deficit round robin.
    pub job: JobId,
    /// The rank (`x-ts-rank`).
    pub rank: Rank,
    /// What it does.
    pub kind: IoKind,
}

/// What an admitted request yields, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoChunk {
    /// Milliseconds the request waited for its slot (`x-ts-queue-wait-ms`): the first item of
    /// every stream.
    Admitted {
        /// The queue wait.
        queue_wait_ms: u32,
    },
    /// Bytes read.
    Data(Bytes),
    /// A write is durable and renamed.
    Written {
        /// Bytes written.
        bytes: u64,
    },
}

/// Why the drive failed a request after admitting it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriveError {
    /// The drive went `stalled`: queued requests fail at once, admitted streams at their next
    /// chunk boundary.
    #[error("the drive is stalled")]
    Stalled,
    /// An I/O error on the file: the shard is quarantined, the stripe queued for repair.
    #[error("I/O error on {file:?}")]
    Eio {
        /// The file.
        file: ColdName,
    },
    /// The stream was parked past `drive.parked_max` and ended; the client retries.
    #[error("the parked stream was ended")]
    ParkedOut,
}

/// The stream of an admitted request.
pub type IoStream = BoxStream<'static, Result<IoChunk, DriveError>>;

/// Why a request was not admitted (`503`, never `404`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The class queue is at `drive.queue_max`.
    #[error("queue full, retry after {retry_after_ms} ms")]
    QueueFull {
        /// `Retry-After`.
        retry_after_ms: u64,
    },
    /// The drive is in a state that serves nothing (`stalled`, `failed`, `indexing`).
    #[error("drive is {0:?}")]
    State(DriveState),
}

/// The scheduler's own report, the answer of `GET /store/v1/ping` for this drive: liveness is the
/// scheduler's, not the process's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriveReport {
    /// The drive's state; `None` before the first report (read as `up`).
    pub state: Option<DriveState>,
    /// The age of the oldest in-flight blocking call, in milliseconds of the injected clock; a
    /// viewer treats a drive whose oldest call is older than `stall_after` as `stalled`.
    pub oldest_call_ms: Option<u64>,
    /// Queued requests by class: demand, write, prepare, background.
    pub queued: [u32; 4],
    /// Streams admitted.
    pub streams: u32,
    /// Of which writing.
    pub writes: u32,
    /// Parked streams (not counted in `queue_max`).
    pub parked: u32,
    /// I/O threads blocked in the kernel past `stall_after` (`drive_io_threads_stuck`).
    pub stuck_threads: u32,
}

/// One hard drive's scheduler.
///
/// Section 14 rows: "Drive scheduler (fake drive, injected clock)"; "Hung drive"; "Slow
/// consumer"; "I/O errors"; "View states on scripted pings"; "Degraded read: ... the
/// reconstructor admits it as `k` stream charges"; "Pack write on a cold drive".
pub trait DriveSched: Send + Sync {
    /// Queues `req`, or refuses it at once. The stream's first item is [`IoChunk::Admitted`], once
    /// the request has a slot (or a small-lane turn).
    fn admit(&self, req: IoRequest) -> Result<IoStream, Refused>;

    /// The scheduler's report.
    fn state(&self) -> DriveReport;
}
