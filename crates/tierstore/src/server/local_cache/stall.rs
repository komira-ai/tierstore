//! What shows a stall of the fills: the requests of the fill socket that wait for a permit, the
//! calls the upstream has not answered, the answers `busy`, and the watchdog that says on stderr
//! when requests wait and nothing moves.
//!
//! The gauges are counted where the wait and the call are, with a guard ([`Tally`]) that counts
//! down when it is dropped, so that a future that is cancelled counts down too. The decisions
//! that are logged ([`Watch::judge`], [`BusyLog::answered`]) are pure functions of the counts, the
//! time of the cache's clock and their own state, so a test drives them without waiting.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, OwnedSemaphorePermit};

use super::metrics::{bump, read};
use super::{Inner, LocalCache, locked};

/// How long requests must have waited with no read of the upstream in flight before the watchdog
/// says so, and the time between its repeats, in milliseconds.
pub(super) const STALL_AFTER: u64 = 30_000;

/// The least time between two lines about requests answered `busy`, in milliseconds.
pub(super) const BUSY_EVERY: u64 = 10_000;

/// How often the watchdog looks.
const WATCH_EVERY: Duration = Duration::from_secs(1);

/// One more in a gauge for as long as it lives.
pub(super) struct Tally<'a>(&'a AtomicU64);

impl<'a> Tally<'a> {
    /// Counts one more in `gauge`, until the guard is dropped.
    pub(super) fn new(gauge: &'a AtomicU64) -> Self {
        let _ = gauge.fetch_add(1, Ordering::Relaxed);
        Self(gauge)
    }
}

impl Drop for Tally<'_> {
    fn drop(&mut self) {
        let _ = self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What the watchdog looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Counts {
    /// Requests of the fill socket that wait for a permit.
    pub waiting: u64,
    /// Fills that hold a permit of the fill socket's pool.
    pub running: u64,
    /// Calls to the upstream that have not been answered.
    pub in_flight: u64,
}

/// What the watchdog says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Report {
    /// Requests have waited for [`STALL_AFTER`] (or another multiple of it) with no call in
    /// flight; said at warn.
    Stalled(String),
    /// The stall that was said has cleared; said at info.
    Moved(String),
}

/// The watchdog's memory.
#[derive(Debug, Default)]
pub(super) struct Watch {
    /// When the stall began, on the cache's clock, if one is going on.
    since: Option<u64>,
    /// When the stall is said next, if it has been said.
    next: Option<u64>,
}

impl Watch {
    /// What to say now, given the `counts` and `now` (milliseconds of the cache's clock). A stall
    /// is requests that wait while no call is in flight: it begins when that first holds, and is
    /// said once it has lasted [`STALL_AFTER`], and again each time that long passes. When it ends
    /// after it was said, the end is said once.
    pub(super) fn judge(&mut self, counts: Counts, now: u64) -> Option<Report> {
        if counts.waiting == 0 || counts.in_flight > 0 {
            self.since = None;
            return self
                .next
                .take()
                .map(|_| Report::Moved("tierstore: local cache: the fills move again".to_owned()));
        }
        let since = *self.since.get_or_insert(now);
        let due = self
            .next
            .unwrap_or_else(|| since.saturating_add(STALL_AFTER));
        if now < due {
            return None;
        }
        self.next = Some(due.saturating_add(STALL_AFTER));
        Some(Report::Stalled(format!(
            "tierstore: local cache: {} fill requests have waited {} s with no upstream read in \
             flight ({} fills hold a permit)",
            counts.waiting,
            (now - since) / 1000,
            counts.running,
        )))
    }
}

/// The answers `busy` that have not been said.
#[derive(Debug, Default)]
pub(super) struct BusyLog {
    /// When a line was last said, if one was.
    said: Option<u64>,
    /// The answers since then, including the one being asked about.
    unsaid: u64,
}

impl BusyLog {
    /// Counts one more answer `busy` at `now`; the line to say if none was said in the last
    /// [`BUSY_EVERY`] milliseconds, with the number of answers it stands for.
    pub(super) fn answered(&mut self, now: u64, limit: usize) -> Option<String> {
        self.unsaid += 1;
        if self
            .said
            .is_some_and(|said| now < said.saturating_add(BUSY_EVERY))
        {
            return None;
        }
        self.said = Some(now);
        let n = std::mem::take(&mut self.unsaid);
        Some(format!(
            "tierstore: local cache: {n} fill requests were answered busy: no permit of \
             fill_socket_concurrency ({limit}) came free within fill_socket_wait"
        ))
    }
}

impl Inner {
    /// The size of the fill socket's pool.
    fn socket_limit(&self) -> usize {
        self.config.fill_socket_concurrency.max(1)
    }

    /// The fills that hold a permit of the fill socket's pool now.
    pub(super) fn socket_running(&self) -> u64 {
        let free = self.source_permits.available_permits();
        self.socket_limit().saturating_sub(free) as u64
    }

    /// The counts the watchdog looks at now.
    pub(super) fn fill_counts(&self) -> Counts {
        Counts {
            waiting: read(&self.counters.socket_waiting),
            running: self.socket_running(),
            in_flight: read(&self.counters.upstream_in_flight),
        }
    }

    /// A permit of the fill socket's pool, waited for up to `fill_socket_wait`; none, counted and
    /// said, if no permit came free in time.
    pub(super) async fn socket_permit(&self) -> Option<OwnedSemaphorePermit> {
        let waiting = Arc::clone(&self.source_permits).acquire_owned();
        let waited = {
            let _counted = Tally::new(&self.counters.socket_waiting);
            tokio::time::timeout(self.config.fill_socket_wait, waiting).await
        };
        if let Ok(Ok(permit)) = waited {
            return Some(permit);
        }
        bump(&self.counters.socket_busy);
        let line = locked(&self.counters.busy_log).answered(self.clock.now(), self.socket_limit());
        if let Some(line) = line {
            tracing::warn!("{line}");
        }
        None
    }

    /// What the watchdog has to say now, if anything.
    pub(super) fn watch_fills(&self, state: &mut Watch) -> Option<Report> {
        state.judge(self.fill_counts(), self.clock.now())
    }
}

impl LocalCache {
    /// Looks once a second until `halt` is notified at the fills of the fill socket, and says on
    /// stderr when requests wait for a permit while the upstream is asked for nothing.
    pub async fn watch_fills(&self, halt: Arc<Notify>) {
        let mut state = Watch::default();
        loop {
            tokio::select! {
                biased;
                () = halt.notified() => break,
                () = tokio::time::sleep(WATCH_EVERY) => {}
            }
            match self.inner.watch_fills(&mut state) {
                Some(Report::Stalled(line)) => tracing::warn!("{line}"),
                Some(Report::Moved(line)) => tracing::info!("{line}"),
                None => {}
            }
        }
    }
}
