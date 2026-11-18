//! The trace of one demand read, end to end (section 12): handler, wait sent, budget acquired,
//! `LOCATE`, device chosen, GET start, device queue wait, first byte, cell received, hash done,
//! write, reply.
//!
//! Each stage is marked with a reading of the injected clock; stages a read does not pass are
//! not marked (a read of present cells goes from the handler to the reply). A finished trace is
//! one `tracing` event, at debug level with the target [`TARGET`], whose fields are the trace's
//! id, its class and each marked stage's offset from the handler in milliseconds. Like the
//! metrics, a trace carries no pack and no key: its id is the caller's (the FUSE request's
//! `unique`, for example).

use std::fmt;

use tierstore_fetch_api::clock::Clock;
use tierstore_fetch_api::ids::Class;

/// The `tracing` target of a finished trace's event.
pub const TARGET: &str = "tierstore::demand_read";

/// A stage of a demand read, in the order a read passes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// The FUSE handler received the read.
    Handler,
    /// The mount sent its `wait` to tierstore.
    WaitSent,
    /// The fetch of the cells acquired its budget permits.
    BudgetAcquired,
    /// The `LOCATE` of the pack was answered.
    Locate,
    /// The engine chose the device (hot replica, drive, or a degraded read).
    DeviceChosen,
    /// The GET was sent.
    GetStart,
    /// The GET's wait in the device's queue ended: GET start plus the drive's reported
    /// `x-ts-queue-wait-ms`, marked with [`DemandTrace::mark_at`].
    DeviceQueueWait,
    /// The GET's first byte arrived.
    FirstByte,
    /// The cell the read needs was received whole.
    CellReceived,
    /// The cell's hash was checked.
    HashDone,
    /// The cell was written to the cache.
    Write,
    /// The read was answered.
    Reply,
}

impl Stage {
    /// Every stage, in order.
    pub const ALL: [Self; 12] = [
        Self::Handler,
        Self::WaitSent,
        Self::BudgetAcquired,
        Self::Locate,
        Self::DeviceChosen,
        Self::GetStart,
        Self::DeviceQueueWait,
        Self::FirstByte,
        Self::CellReceived,
        Self::HashDone,
        Self::Write,
        Self::Reply,
    ];

    /// The stage's name, as the event's field names it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Handler => "handler",
            Self::WaitSent => "wait_sent",
            Self::BudgetAcquired => "budget_acquired",
            Self::Locate => "locate",
            Self::DeviceChosen => "device_chosen",
            Self::GetStart => "get_start",
            Self::DeviceQueueWait => "device_queue_wait",
            Self::FirstByte => "first_byte",
            Self::CellReceived => "cell_received",
            Self::HashDone => "hash_done",
            Self::Write => "write",
            Self::Reply => "reply",
        }
    }
}

/// The id of a trace, the caller's: never a pack or a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceId(pub u64);

/// A mark that breaks the order of the stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    /// The stage is not after the last one marked.
    #[error("demand read trace: {stage:?} marked after {last:?}")]
    OutOfOrder {
        /// The stage marked.
        stage: Stage,
        /// The last stage marked before it.
        last: Stage,
    },
    /// The stage's time is before the last mark's.
    #[error("demand read trace: {stage:?} at {at_ms} ms, before the last mark at {last_ms} ms")]
    BeforeLast {
        /// The stage marked.
        stage: Stage,
        /// Its time.
        at_ms: u64,
        /// The time of the last mark.
        last_ms: u64,
    },
}

/// The trace of one demand read while it runs. It starts at [`Stage::Handler`] and ends with
/// [`DemandTrace::finish`] at [`Stage::Reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemandTrace {
    id: TraceId,
    class: Class,
    /// Each marked stage and its time, in stage order.
    marks: Vec<(Stage, u64)>,
}

impl DemandTrace {
    /// A trace whose handler stage is now.
    #[must_use]
    pub fn begin(id: TraceId, class: Class, clock: &dyn Clock) -> Self {
        Self {
            id,
            class,
            marks: vec![(Stage::Handler, clock.now_ms())],
        }
    }

    /// Marks `stage` now.
    pub fn mark(&mut self, stage: Stage, clock: &dyn Clock) -> Result<(), TraceError> {
        self.mark_at(stage, clock.now_ms())
    }

    /// Marks `stage` at `at_ms`, a time of the clock the trace began with. The stage must come
    /// after the last one marked, at or after its time; otherwise nothing is marked.
    pub fn mark_at(&mut self, stage: Stage, at_ms: u64) -> Result<(), TraceError> {
        // A trace always holds its handler mark, so `last` is always there.
        let (last, last_ms) = self.marks[self.marks.len() - 1];
        if stage <= last {
            return Err(TraceError::OutOfOrder { stage, last });
        }
        if at_ms < last_ms {
            return Err(TraceError::BeforeLast {
                stage,
                at_ms,
                last_ms,
            });
        }
        self.marks.push((stage, at_ms));
        Ok(())
    }

    /// Marks [`Stage::Reply`] now, emits the trace's event, and returns the trace.
    pub fn finish(mut self, clock: &dyn Clock) -> Result<Trace, TraceError> {
        self.mark(Stage::Reply, clock)?;
        let trace = Trace {
            id: self.id,
            class: self.class,
            marks: self.marks,
        };
        tracing::debug!(
            target: TARGET,
            trace = trace.id.0,
            class = trace.class.header(),
            total_ms = trace.total_ms(),
            stages = %Offsets(&trace),
            "tierstore: demand read"
        );
        Ok(trace)
    }
}

/// A finished trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    id: TraceId,
    class: Class,
    marks: Vec<(Stage, u64)>,
}

impl Trace {
    /// The trace's id.
    #[must_use]
    pub fn id(&self) -> TraceId {
        self.id
    }

    /// The read's class.
    #[must_use]
    pub fn class(&self) -> Class {
        self.class
    }

    /// The milliseconds from the handler to `stage`, if the read passed it.
    #[must_use]
    pub fn offset_ms(&self, stage: Stage) -> Option<u64> {
        let start = self.marks[0].1;
        self.marks
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, at)| at - start)
    }

    /// The milliseconds from the handler to the reply.
    #[must_use]
    pub fn total_ms(&self) -> u64 {
        self.marks[self.marks.len() - 1].1 - self.marks[0].1
    }

    /// Each stage the read passed, with its offset from the handler, in order.
    #[must_use]
    pub fn stages(&self) -> Vec<(Stage, u64)> {
        let start = self.marks[0].1;
        self.marks.iter().map(|(s, at)| (*s, at - start)).collect()
    }
}

/// The `stages` field of a trace's event: `handler=0 wait_sent=1 ... reply=9`.
struct Offsets<'a>(&'a Trace);

impl fmt::Display for Offsets<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stages: Vec<String> = self
            .0
            .stages()
            .into_iter()
            .map(|(stage, offset)| format!("{}={offset}", stage.as_str()))
            .collect();
        f.write_str(&stages.join(" "))
    }
}
