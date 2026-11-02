//! The state of a front's drain, which the S3 front ([`Server`](super::Server)) and the OCI front
//! ([`OciListener`](super::oci::OciListener)) each keep one of, and the close of a connection at
//! the end of the settle.
//!
//! A drain has three phases: serving; draining, from the moment the drain begins, when every
//! answer carries `Connection: close`; and settled, from the end of the settle, when the front
//! takes no new connection and closes the idle ones. A connection that the front closes during
//! the drain gets a lingering close ([`Lingering`](super::linger::Lingering)): the front shuts
//! its end for writing and reads, and drops, what the peer still sends, until the peer closes or
//! [`Drain::linger_bound`] passes. The bytes read then are a request that came too late, and the
//! connection is counted in the drain's late counter.
//!
//! The two properties of the drain, `Connection: close` from its start (P1, [`Drain::closes_answers`])
//! and the lingering close (P2, [`Drain::lingers`]), are each one function here, which the S3 and
//! the OCI front both ask. A build with the test-only feature `test-drain-off` answers both with
//! no: it is the server as it was before them, which the hand-over test under load
//! (`tests/handover`) runs to show that it fails without them. Only that test's CI job builds it;
//! no release, `cargo install` or test run of the crate turns it on.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, watch};

use super::Server;
use super::linger::Wire;
use super::oci::OciDrain;

/// The front is serving.
pub(super) const SERVING: u8 = 0;
/// The drain began: `/readyz` says 503, and every answer says `Connection: close`.
pub(super) const NOT_READY: u8 = 1;
/// The settle is over: the front takes no new connection and closes the idle ones.
pub(super) const SETTLED: u8 = 2;

/// The drain of one front: its phase, and the counts of its connections that the drain waits
/// for or reports.
pub(crate) struct Drain {
    pub(super) phase: AtomicU8,
    /// The connections with a request on the wire: read, and not answered in full.
    pub(super) unanswered: AtomicUsize,
    /// The connections the drain has shut for writing that are waiting for their peer to close.
    pub(super) lingering: AtomicUsize,
    /// The lingering closes that have begun to read: the connection's HTTP is over, and its stream
    /// is being read to the peer's end.
    pub(super) reading: AtomicUsize,
    /// The connections whose peer wrote after the front shut its end, in the drain. Shared, so
    /// that the counters of a front can show it.
    late: Arc<AtomicU64>,
    /// Notified whenever a count of the drain changes, and whenever a count that the front keeps
    /// beside them and waits for with [`Drain::wait_for_zero`] does.
    pub(super) idle: Notify,
    /// Set once, when the settle is over: a connection with no request on the wire closes, and
    /// one with a request on the wire closes when it is answered.
    pub(super) settled: watch::Sender<bool>,
    /// How long a lingering close waits for the peer to close its end.
    linger_bound: Duration,
}

impl Drain {
    /// The drain of a front that has not begun to drain. A lingering close waits `linger_bound`
    /// at most, and the connections whose peer writes after the front shut its end are counted in
    /// `late`.
    pub(super) fn new(linger_bound: Duration, late: Arc<AtomicU64>) -> Self {
        #[cfg(feature = "test-drain-off")]
        eprintln!(
            "tierstore: a test build (test-drain-off): no Connection: close and no lingering close in the drain"
        );
        Self {
            phase: AtomicU8::new(SERVING),
            unanswered: AtomicUsize::new(0),
            lingering: AtomicUsize::new(0),
            reading: AtomicUsize::new(0),
            late,
            idle: Notify::new(),
            settled: watch::channel(false).0,
            linger_bound,
        }
    }

    pub(super) fn phase(&self) -> u8 {
        self.phase.load(Ordering::SeqCst)
    }

    /// Whether the drain has not begun.
    pub(super) fn is_ready(&self) -> bool {
        self.phase() == SERVING
    }

    /// Whether an answer says `Connection: close` (P1): from the moment the drain begins, so that
    /// a client that has its answer opens its next connection to whatever now serves the port.
    #[cfg(not(feature = "test-drain-off"))]
    pub(super) fn closes_answers(&self) -> bool {
        !self.is_ready()
    }

    /// Whether a connection the front shuts gets a lingering close (P2): in the drain, so that a
    /// client reads a clean end of stream and not a reset.
    #[cfg(not(feature = "test-drain-off"))]
    pub(super) fn lingers(&self) -> bool {
        !self.is_ready()
    }

    /// The test build without P1: no answer says `Connection: close`.
    #[cfg(feature = "test-drain-off")]
    pub(super) fn closes_answers(&self) -> bool {
        false
    }

    /// The test build without P2: every close is a plain close.
    #[cfg(feature = "test-drain-off")]
    pub(super) fn lingers(&self) -> bool {
        false
    }

    /// Begins the drain: every answer says `Connection: close` from now on. Once; calling it again,
    /// or after the settle, does nothing.
    pub(super) fn begin(&self) {
        let _ = self
            .phase
            .compare_exchange(SERVING, NOT_READY, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Ends the settle (and begins the drain, if it had not begun): the connections with no request
    /// on the wire close now, with a lingering close, and the others once they are answered.
    pub(super) fn settle(&self) {
        self.phase.store(SETTLED, Ordering::SeqCst);
        let _ = self.settled.send_replace(true);
    }

    /// How long a lingering close waits for the peer to close its end.
    pub(super) fn linger_bound(&self) -> Duration {
        self.linger_bound
    }

    /// The connections the drain closed that still wait for their peer to close its end.
    pub(super) fn lingering(&self) -> usize {
        self.lingering.load(Ordering::SeqCst)
    }

    /// The connections whose peer wrote after the front shut its end.
    pub(super) fn late(&self) -> u64 {
        self.late.load(Ordering::SeqCst)
    }

    /// Counts a connection whose peer wrote after the front shut its end.
    pub(super) fn count_late(&self) {
        self.late.fetch_add(1, Ordering::SeqCst);
        self.idle.notify_waiters();
    }

    /// The requests that were read and whose answer was not written in full.
    pub(super) fn unsent(&self) -> usize {
        self.unanswered.load(Ordering::SeqCst)
    }

    /// Resolves when `count` is zero: one of the counts of this drain, or one that the front keeps
    /// beside them and notifies [`Drain::idle`] for.
    pub(super) async fn wait_for_zero(&self, count: &AtomicUsize) {
        loop {
            let ended = self.idle.notified();
            tokio::pin!(ended);
            let _ = ended.as_mut().enable();
            if count.load(Ordering::SeqCst) == 0 {
                return;
            }
            ended.await;
        }
    }
}

/// Resolves when a connection is to be closed: once the settle is over (`settled` turns true), at
/// once if no request is on its `wire`, and else once that request is answered in full. The
/// front's connection task polls its connection beside this, and when it resolves tells the
/// connection to close once it is idle (hyper's `graceful_shutdown`): its stream, a
/// [`Lingering`](super::linger::Lingering) one, then lingers.
pub(super) async fn time_to_close(wire: &Wire, mut settled: watch::Receiver<bool>) {
    let _ = settled.wait_for(|settled| *settled).await;
    wire.wait_clear().await;
}

/// What a node says on stderr as it exits of the requests that came too late in the drain of its
/// fronts (`tierstore_drain_late_requests_total` and `tierstore_oci_drain_late_requests_total`):
/// a line for each front that counted any, nothing when none did. The counters cannot be scraped
/// by then: a late request is counted after the settle, when the S3 listener that serves
/// `/metricz` is closed. A node without the OCI front counts none there.
pub(super) fn late_note(s3: u64, oci: u64) -> String {
    let mut note = String::new();
    for (front, late) in [("S3", s3), ("OCI", oci)] {
        if late > 0 {
            note.push_str(&format!(
                "tierstore: {late} requests came too late in the drain of the {front} front\n"
            ));
        }
    }
    note
}

/// [`late_note`] of the S3 front `server` and of the OCI front `oci`, if the node has one.
pub(super) fn late_of(server: &Server, oci: Option<&OciDrain>) -> String {
    late_note(server.late_requests(), oci.map_or(0, OciDrain::late))
}
