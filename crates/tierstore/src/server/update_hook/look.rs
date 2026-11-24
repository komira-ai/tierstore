//! The looks at the service that both arms make, over the instances' answers and the clock
//! alone: `/readyz` of the public port, and the build gauge of `/metricz` through it. hostkeeper
//! names the same health URL in the manifest (`http://127.0.0.1:9300/readyz`).

use std::time::Duration;

use super::PUBLIC_PORT;
use super::ports::{Clock, Instances};
use super::probe::{CacheMode, build_of, readiness};
use super::report::{Done, Reason};

/// The time between two looks.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How many looks a step gives a release it started to answer: `/readyz`, and, after a start,
/// the build gauge naming the release. A new instance answers only once it has opened its local
/// cache, and in a hand-over the old instance answers until then, so this is the bound of the
/// start itself. A build node's read-only open of its cache took up to 664 s (161457 objects on
/// a SATA SSD under constant I/O pressure) and 89 s on the other nodes of the lab; 1000 looks of
/// a second leave a margin of half again over the slowest. The manifest's `cutover`, `rollback`
/// and `cleanup` bounds are above this and [`PROBE_ATTEMPTS`] together
/// (`crates/tierstore/tests/packaging_hostkeeper.rs`).
pub const READY_ATTEMPTS: u32 = 1000;
/// How many looks `cutover` gives a new store that serves read-only to take the cache lock,
/// before it starts the fs instance on it ([`Look::read_write`]). In a hand-over the new store
/// takes the lock only once the old one has drained and exited, which is at most its settle and
/// its drain deadline (10 s and 15 min by default: `server.drain_lb_wait`,
/// `server.drain_deadline`), and has then scanned its cache again, which takes about as long as a
/// read-only open (up to 664 s, see [`READY_ATTEMPTS`]): 1574 s at the longest. 2400 looks a
/// second apart leave a margin of half again. The manifest's `cutover` bound is above this,
/// [`READY_ATTEMPTS`] and [`PROBE_ATTEMPTS`] together (`look_tests.rs`).
pub(super) const READ_WRITE_ATTEMPTS: u32 = 2400;
/// How many looks a probe of the build makes.
pub const PROBE_ATTEMPTS: u32 = 60;
/// How many answers in a row by the expected version a probe needs.
pub const PROBE_HITS: u32 = 3;

/// The instances' answers and the clock that paces the looks.
pub(super) struct Look<'a> {
    /// The instances' answers.
    pub instances: &'a dyn Instances,
    /// The clock that waits between two looks.
    pub clock: &'a dyn Clock,
}

impl Look<'_> {
    /// One look at `/readyz` of the public port.
    pub fn ready(&self) -> Option<CacheMode> {
        readiness(&self.instances.get(PUBLIC_PORT, "/readyz"))
    }

    /// Whether one request to the public port was answered by a build of `version`.
    fn answered_by(&self, version: &str) -> bool {
        match self.instances.get(PUBLIC_PORT, "/metricz") {
            Ok(reply) if reply.status == 200 => {
                build_of(&reply.body).is_some_and(|build| build.is(version))
            }
            _ => false,
        }
    }

    /// Whether [`PROBE_HITS`] requests in a row to the public port were answered by a build of
    /// `version`, within [`PROBE_ATTEMPTS`] looks.
    pub fn probe(&self, version: &str) -> bool {
        let mut streak = 0;
        for _ in 0..PROBE_ATTEMPTS {
            streak = if self.answered_by(version) {
                streak + 1
            } else {
                0
            };
            if streak >= PROBE_HITS {
                return true;
            }
            self.clock.pause(POLL_INTERVAL);
        }
        false
    }

    /// Waits for `version` to serve, then probes for it: what a step does after it started a
    /// release, and what `cleanup` confirms before the commit. The wait is up to
    /// [`READY_ATTEMPTS`] looks for one that `/readyz` answers and the build gauge names
    /// `version`: in a hand-over the old instance answers `/readyz` until the new one has
    /// started and taken the sockets, so an answer of `/readyz` alone does not end it. Then the
    /// probe needs [`PROBE_HITS`] answers in a row by `version`. `NotReady` when `/readyz` was
    /// never answered, `ProbeFailed` when it was and `version` was not seen to serve.
    pub fn serves(&self, version: &str) -> Result<CacheMode, Done> {
        let mut answered = false;
        for _ in 0..READY_ATTEMPTS {
            if let Some(mode) = self.ready() {
                answered = true;
                if self.answered_by(version) {
                    return if self.probe(version) {
                        Ok(mode)
                    } else {
                        Err(Self::not_answered_by(version))
                    };
                }
            }
            self.clock.pause(POLL_INTERVAL);
        }
        Err(if answered {
            Self::not_answered_by(version)
        } else {
            Done::fail(
                Reason::NotReady,
                format!("port {PUBLIC_PORT} does not answer /readyz"),
            )
        })
    }

    /// Looks at `/readyz` until it says a mode other than read-only, up to
    /// [`READ_WRITE_ATTEMPTS`] looks a [`POLL_INTERVAL`] apart, the first at once: what `cutover`
    /// does before it starts the fs instance on a store that serves read-only. A look that is
    /// answered read-only, or not ready (no answer, or 503 from the old instance that still drains
    /// on the same sockets), is waited out. The mode `/readyz` then said, or `None` when no look
    /// said another.
    pub fn read_write(&self) -> Option<CacheMode> {
        for attempt in 0..READ_WRITE_ATTEMPTS {
            if attempt > 0 {
                self.clock.pause(POLL_INTERVAL);
            }
            match self.ready() {
                None | Some(CacheMode::ReadOnly) => {}
                mode => return mode,
            }
        }
        None
    }

    /// The failure of a start whose release was not seen to serve.
    fn not_answered_by(version: &str) -> Done {
        Done::fail(
            Reason::ProbeFailed,
            format!("no request to port {PUBLIC_PORT} was answered by {version}"),
        )
    }

    /// One look at `/readyz`, then the probe for `version`, with no wait for `/readyz`: what
    /// `verify` observes (it defers a `NotReady`), and the checks of whether a release already
    /// serves. `NotReady` or `WrongBuild` when it is not seen.
    pub fn observed(&self, version: &str) -> Result<CacheMode, Done> {
        let mode = self.ready().ok_or_else(|| {
            Done::fail(
                Reason::NotReady,
                format!("port {PUBLIC_PORT} does not answer /readyz"),
            )
        })?;
        if !self.probe(version) {
            return Err(Done::fail(
                Reason::WrongBuild,
                format!("port {PUBLIC_PORT} is not answered by {version}"),
            ));
        }
        Ok(mode)
    }
}

/// The success of a `verify` that observed the new release in `mode`: a build node that still
/// serves read-only (it has not taken the cache lock yet) says `promotion_pending`, and
/// hostkeeper runs `verify` again until it does not, within its promotion ceiling.
pub(super) fn verified(mode: CacheMode, version: &str) -> Done {
    let pending = mode == CacheMode::ReadOnly;
    let mut done = Done::ok(if pending {
        format!("{version} serves on port {PUBLIC_PORT} read-only: the promotion is pending")
    } else {
        format!("{version} serves on port {PUBLIC_PORT}")
    });
    done.report.promotion_pending = pending;
    done
}
