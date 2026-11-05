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
/// How many looks a step gives the service to answer `/readyz`.
pub const READY_ATTEMPTS: u32 = 120;
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

    /// Up to [`READY_ATTEMPTS`] looks at `/readyz`, a pause between them.
    pub fn wait_ready(&self) -> Option<CacheMode> {
        for _ in 0..READY_ATTEMPTS {
            if let Some(mode) = self.ready() {
                return Some(mode);
            }
            self.clock.pause(POLL_INTERVAL);
        }
        None
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

    /// Waits for `/readyz`, then probes for `version`: what a step does after it started a
    /// release. `NotReady` or `ProbeFailed` when it does not serve.
    pub fn serves(&self, version: &str) -> Result<CacheMode, Done> {
        let mode = self.wait_ready().ok_or_else(|| {
            Done::fail(
                Reason::NotReady,
                format!("port {PUBLIC_PORT} does not answer /readyz"),
            )
        })?;
        if !self.probe(version) {
            return Err(Done::fail(
                Reason::ProbeFailed,
                format!("no request to port {PUBLIC_PORT} was answered by {version}"),
            ));
        }
        Ok(mode)
    }

    /// One look at `/readyz`, then the probe for `version`: what `verify` and `cleanup` observe.
    /// `NotReady` or `WrongBuild` when it is not seen.
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
