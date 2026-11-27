//! One attempt of an op (4.6): the GETs of a run of claimed cells from one source, received
//! into the pool's cell buffers (4.8) as the bytes arrive and verified per cell, with the
//! pre-first-byte and stall timeouts of the injected clock. An attempt reports to its op on the
//! op's channel; it never publishes.
//!
//! An attempt takes a buffer before it reads a cell's first bytes and hands each full cell to its
//! op before it takes the next buffer, so it never waits for a buffer while it holds a full cell.
//! The wait for a buffer is local: it moves the pre-first-byte and stall deadlines on by its
//! length (a drive is not stalled because the node is out of buffers), and is left out of the
//! first-byte and network times the controller is told (4.3: local time is excluded).

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::future::Either;
use tierstore_fetch_api::budget::{Observation, Outcome, Permit};
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, JobId, Rank};
use tierstore_fetch_api::upstream::{DriveSignals, DriveState, GetRequest, Source, UpstreamError};
use tierstore_fetch_api::verify::{CellJob, Verdict, VerifyPriority};
use tierstore_format::ids::Generation;
use tokio::sync::mpsc;

use crate::baseline::timeout;
use crate::engine::{Armed, Inner, runs_of};
use crate::fill::Fill;
use crate::plan::{CELL_BYTES, size_class};
use crate::pool::{CellBuf, Slot};
use crate::stats::{GetKind, TimerKind};

/// Events an op waits for.
pub(crate) enum Event {
    FirstByte(u32),
    Cell {
        source: Source,
        cell: u32,
        bytes: Bytes,
        verdict: Option<Verdict>,
        /// The pool slot of the cell's buffer, held until its bytes are dropped.
        slot: Slot,
    },
    Ended {
        attempt: u32,
        failure: Option<Fail>,
        source: Source,
    },
    HedgeDue,
    RetryDue {
        cells: Vec<u32>,
        avoid: Source,
    },
}

/// Why an attempt ended early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailKind {
    /// No first byte within the pre-first-byte timeout, no bytes within the stall timeout, or the
    /// drive said `stalled`.
    Stall,
    /// The response said the drive is `stalled` (10.7): retried at once, like a transport
    /// failure, also while the drive is in a decrease state.
    DriveStalled,
    /// A reset, a connect error, a node down, a stream that ended early or ran long.
    Transport,
    /// `503` from a drive that is `stalled` or `indexing`: not a load signal, retried at once.
    NotLoad,
    /// `503 Retry-After`.
    Busy(u64),
    /// `412`, or an `ETag` other than the pinned generation.
    OtherGeneration,
    /// `404`.
    NotFound,
    /// The serving node found a crc mismatch at this object offset.
    Corrupt(u64),
}

#[derive(Debug, Clone)]
pub(crate) struct Fail {
    pub kind: FailKind,
    pub source: Source,
}

/// One attempt: GETs of the runs of `cells` from `source`.
pub(crate) struct Attempt {
    pub inner: Arc<Inner>,
    pub fill: Arc<Fill>,
    pub attempt: u32,
    pub cells: Vec<u32>,
    pub source: Source,
    /// The fill's pinned generation: the `If-Match` of every part.
    pub generation: Generation,
    pub kind: GetKind,
    pub class: Class,
    pub job: JobId,
    pub rank: Rank,
    pub permit: Option<Permit>,
    pub tx: mpsc::Sender<Event>,
}

impl Attempt {
    pub(crate) async fn run(mut self) {
        let failure = self.fetch().await.err();
        // The op may have ended; then nobody needs the event.
        let _ = self
            .tx
            .send(Event::Ended {
                attempt: self.attempt,
                failure,
                source: self.source.clone(),
            })
            .await;
    }

    fn fail(&self, kind: FailKind) -> Fail {
        Fail {
            kind,
            source: self.source.clone(),
        }
    }

    async fn fetch(&mut self) -> Result<(), Fail> {
        let _permit = match self.permit.take() {
            Some(p) => p,
            None => {
                let charge = Inner::charge(self.class, &self.source);
                self.inner.parts.budget.acquire(charge).await
            }
        };
        for run in runs_of(&self.cells) {
            self.get(run).await?;
        }
        Ok(())
    }

    fn observe(&self, first_byte_ms: Option<u32>, signals: DriveSignals, outcome: Outcome) {
        self.inner.parts.budget.observe(Observation::Response {
            device: self.source.device,
            size_class: size_class(self.fill.spec.size),
            first_byte_ms,
            signals,
            outcome,
        });
    }

    /// A failure before any byte: told to the controller, then returned.
    fn refused(&self, kind: FailKind, signals: DriveSignals) -> Fail {
        let outcome = match kind {
            FailKind::Busy(_) => Outcome::Busy,
            FailKind::Stall | FailKind::Transport => Outcome::Transport,
            _ => Outcome::NotLoad,
        };
        self.observe(None, signals, outcome);
        self.fail(kind)
    }

    async fn get(&mut self, run: CellRun) -> Result<(), Fail> {
        let inner = self.inner.clone();
        let fill = self.fill.clone();
        let start = fill.cell_range(run.first).0;
        let (last, last_len) = fill.cell_range(run.first + run.count - 1);
        let end = (last + last_len).min(self.source.extent.end().unwrap_or(u64::MAX));
        let range = ByteRange {
            offset: start,
            len: end.saturating_sub(start),
        };
        let class = size_class(fill.spec.size);
        let config = &inner.config;
        let generation = self.generation;
        let (first_byte, stall) = {
            let mut st = inner.lock();
            st.stats.count_get(self.kind);
            let first_byte = timeout(
                config.first_byte_floor_ms,
                config.first_byte_factor,
                st.baselines.first_byte_p99(class),
            );
            let stall = timeout(
                config.stall_floor_ms,
                config.stall_factor,
                st.baselines.ms_per_mib(class),
            );
            (first_byte, stall)
        };
        inner.bump();
        let req = GetRequest {
            key: fill.spec.id.key.clone(),
            source: self.source.clone(),
            range,
            generation,
            class: self.class,
            job: self.job,
            rank: self.rank,
        };
        let t0 = inner.now();
        let answer = race(
            &inner,
            inner.parts.upstream.get(req),
            TimerKind::FirstByte,
            t0 + first_byte,
        )
        .await;
        let response = match answer {
            None => return Err(self.refused(FailKind::Stall, DriveSignals::default())),
            Some(Err(e)) => {
                let (kind, signals) = classify(&e);
                inner.lock().book.signals(self.source.device, signals);
                return Err(self.refused(kind, signals));
            }
            Some(Ok(r)) => r,
        };
        let signals = response.signals;
        inner.lock().book.signals(self.source.device, signals);
        if signals.drive_state == Some(DriveState::Stalled) {
            return Err(self.refused(FailKind::DriveStalled, signals));
        }
        if response.generation != generation {
            return Err(self.refused(FailKind::OtherGeneration, signals));
        }
        // Headers that say the request queued: the drive is serving now, so the pre-first-byte
        // clock starts again.
        let mut deadline = match signals.queue_wait_ms {
            Some(_) => inner.now() + first_byte,
            None => t0 + first_byte,
        };
        let mut cells = Cells::new(&fill, run);
        let mut first_at = None;
        // Time spent waiting for buffers, in all and before the first byte.
        let mut local = 0;
        let mut local_before_first = 0;
        let mut body = response.body;
        loop {
            // The buffer of the next cell, before its bytes are read.
            if cells.buf.is_none() & !cells.complete() {
                let asked = inner.now();
                cells.buf = Some(inner.pool.take(self.class).await);
                let waited = inner.now() - asked;
                local += waited;
                deadline += waited;
            }
            let kind = match first_at {
                None => TimerKind::FirstByte,
                Some(_) => TimerKind::Stall,
            };
            let next = race(&inner, body.next(), kind, deadline).await;
            let chunk = match next {
                Some(Some(Ok(chunk))) => chunk,
                Some(None) => break,
                ended => {
                    let kind = match ended {
                        Some(Some(Err(e))) => classify(&e).0,
                        _ => FailKind::Stall,
                    };
                    let buffered = cells.buffered();
                    inner.with_stats(|s| s.bytes_wasted += buffered);
                    return Err(match first_at {
                        None => self.refused(kind, signals),
                        Some(_) => self.fail(kind),
                    });
                }
            };
            let now = inner.now();
            if first_at.is_none() {
                first_at = Some(now);
                local_before_first = local;
                let ms = (now - t0).saturating_sub(local);
                self.observe(
                    Some(u32::try_from(ms).unwrap_or(u32::MAX)),
                    signals,
                    Outcome::Served,
                );
                inner.lock().baselines.first_byte(class, ms);
                // An op that ended aborts its attempts: a failed send needs nothing more.
                let _ = self.tx.send(Event::FirstByte(self.attempt)).await;
            }
            let len = chunk.len() as u64;
            inner.with_stats(|s| s.bytes_fetched += len);
            if len > cells.left() {
                // A body longer than its part: nothing of this chunk is used.
                let buffered = cells.buffered();
                inner.with_stats(|s| s.bytes_wasted += len + buffered);
                return Err(self.fail(FailKind::Transport));
            }
            let mut chunk = chunk;
            while !chunk.is_empty() {
                // A chunk that runs into the next cell takes its buffer after the last cell is
                // handed over.
                let mut buf = match cells.buf.take() {
                    Some(b) => b,
                    None => {
                        let asked = inner.now();
                        let b = inner.pool.take(self.class).await;
                        local += inner.now() - asked;
                        b
                    }
                };
                let take = (cells.len - buf.buf.len()).min(chunk.len());
                buf.buf.extend_from_slice(&chunk.split_to(take));
                if buf.buf.len() < cells.len {
                    cells.buf = Some(buf);
                    continue;
                }
                let cell = cells.advance();
                let (verdict, bytes) = verify(&inner, &fill, cell, buf.buf.freeze()).await;
                let event = Event::Cell {
                    source: self.source.clone(),
                    cell,
                    bytes,
                    verdict,
                    slot: buf.slot,
                };
                let _ = self.tx.send(event).await;
            }
            deadline = inner.now() + stall;
        }
        if !cells.complete() {
            let buffered = cells.buffered();
            inner.with_stats(|s| s.bytes_wasted += buffered);
            return Err(self.fail(FailKind::Transport));
        }
        let elapsed =
            (inner.now() - first_at.unwrap_or(t0)).saturating_sub(local - local_before_first);
        let ms_per_mib = elapsed.saturating_mul(CELL_BYTES) / range.len.max(1);
        inner.parts.budget.observe(Observation::Network {
            node: Some(self.source.device.node()),
            ms_per_mib: u32::try_from(ms_per_mib).unwrap_or(u32::MAX),
        });
        inner.lock().baselines.network(class, ms_per_mib);
        Ok(())
    }
}

/// `fut`, or `None` once the timer of `kind` reaches `deadline`.
async fn race<F: Future>(
    inner: &Arc<Inner>,
    fut: F,
    kind: TimerKind,
    deadline: u64,
) -> Option<F::Output> {
    let sleep = Armed::new(inner, kind, deadline);
    match futures::future::select(Box::pin(fut), sleep).await {
        Either::Left((v, _)) => Some(v),
        Either::Right(((), _)) => None,
    }
}

/// The failure kind of an upstream error, and the drive's signals it carried.
fn classify(e: &UpstreamError) -> (FailKind, DriveSignals) {
    match e {
        UpstreamError::NotFound { .. } => (FailKind::NotFound, DriveSignals::default()),
        UpstreamError::PreconditionFailed { .. } => {
            (FailKind::OtherGeneration, DriveSignals::default())
        }
        UpstreamError::Unavailable {
            retry_after_ms,
            signals,
            ..
        } => {
            let not_load = signals.drive_state.is_some_and(|s| !s.is_load_signal());
            let kind = match not_load {
                true => FailKind::NotLoad,
                false => FailKind::Busy(*retry_after_ms),
            };
            (kind, *signals)
        }
        UpstreamError::Corrupt { offset, .. } => {
            (FailKind::Corrupt(*offset), DriveSignals::default())
        }
        UpstreamError::Transport { .. } => (FailKind::Transport, DriveSignals::default()),
    }
}

/// The verdict on one cell's bytes: `None` for a pack without cell digests (the serving node
/// checked its crc, section 9); a cell the list has no digest for fails.
async fn verify(
    inner: &Arc<Inner>,
    fill: &Arc<Fill>,
    cell: u32,
    bytes: Bytes,
) -> (Option<Verdict>, Bytes) {
    let Some(d) = &fill.spec.digests else {
        return (None, bytes);
    };
    let Some(expected) = d.list.cell_digest(d.pack, cell) else {
        return (Some(Verdict::Mismatch), bytes);
    };
    let job = CellJob {
        cell,
        expected,
        bytes,
        priority: VerifyPriority::Fill,
    };
    let (verdict, bytes) = inner.parts.verifier.verify(job).await;
    (Some(verdict), bytes)
}

/// Where a part's bytes are in its cells.
struct Cells {
    /// The cell the next bytes belong to.
    next: u32,
    end: u32,
    /// The length of cell `next`.
    len: usize,
    /// Bytes from the start of cell `next` to the end of the part's last cell.
    left: u64,
    /// The buffer of cell `next`, once its first bytes are on the way.
    buf: Option<CellBuf>,
    fill_size: u64,
}

impl Cells {
    fn new(fill: &Fill, run: CellRun) -> Self {
        let (start, _) = fill.cell_range(run.first);
        let (last, last_len) = fill.cell_range(run.first + run.count - 1);
        let mut c = Self {
            next: run.first,
            end: run.first + run.count,
            len: 0,
            left: last + last_len - start,
            buf: None,
            fill_size: fill.spec.size,
        };
        c.len = c.cell_len(start);
        c
    }

    fn cell_len(&self, offset: u64) -> usize {
        // A cell is at most 1 MiB.
        (CELL_BYTES.min(self.fill_size.saturating_sub(offset))) as usize
    }

    /// Cell `next` is full: the cell, and the next one is current.
    fn advance(&mut self) -> u32 {
        let cell = self.next;
        self.left -= self.len as u64;
        self.next += 1;
        self.len = self.cell_len(u64::from(self.next) * CELL_BYTES);
        cell
    }

    /// The bytes the part can still take.
    fn left(&self) -> u64 {
        self.left - self.buffered()
    }

    fn complete(&self) -> bool {
        self.next >= self.end
    }

    fn buffered(&self) -> u64 {
        self.buf.as_ref().map_or(0, |b| b.buf.len() as u64)
    }
}
