//! The looks at an instance that the steps of every machine make, over the instances' answers
//! and the clock alone: `/readyz` of one port, the build gauge of `/metricz` through a port, and
//! the wait for the promotion. The two-slot steps of Linux and the single-instance steps of
//! launchd both look through these, so the two read an instance the same way.

use super::{POLL_INTERVAL, PROBE_ATTEMPTS, PROBE_HITS};
use crate::server::update_hook::ports::{Clock, Instances};
use crate::server::update_hook::probe::{CacheMode, build_of, readiness};
use crate::server::update_hook::report::{Done, Reason};

/// The instances' answers and the clock that paces the looks.
pub(in crate::server::update_hook) struct Look<'a> {
    /// The instances' answers.
    pub instances: &'a dyn Instances,
    /// The clock that waits between two looks.
    pub clock: &'a dyn Clock,
}

impl Look<'_> {
    /// One look at `/readyz` of the instance on `port`.
    pub fn ready(&self, port: u16) -> Option<CacheMode> {
        readiness(&self.instances.get(port, "/readyz"))
    }

    /// Up to `attempts` looks at `/readyz`, a pause between them.
    pub fn wait_ready(&self, port: u16, attempts: u32) -> Option<CacheMode> {
        for _ in 0..attempts {
            if let Some(mode) = self.ready(port) {
                return Some(mode);
            }
            self.clock.pause(POLL_INTERVAL);
        }
        None
    }

    /// Whether one request to `through` was answered by the instance on `slot` running `version`.
    fn answered_by(&self, through: u16, slot: u16, version: &str) -> bool {
        match self.instances.get(through, "/metricz") {
            Ok(reply) if reply.status == 200 => {
                build_of(&reply.body).is_some_and(|build| build.is(slot, version))
            }
            _ => false,
        }
    }

    /// Whether [`PROBE_HITS`] requests in a row to `through` were answered by the instance on
    /// `slot` running `version`, within [`PROBE_ATTEMPTS`] looks.
    pub fn probe(&self, through: u16, slot: u16, version: &str) -> bool {
        let mut streak = 0;
        for _ in 0..PROBE_ATTEMPTS {
            streak = if self.answered_by(through, slot, version) {
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

    /// Polls `/readyz` of `port` until it is not read-only (true) or `bound_secs` have passed on
    /// the clock (false); an instance that does not answer is an error.
    pub fn wait_promoted(&self, port: u16, bound_secs: u64) -> Result<bool, Done> {
        let deadline = self
            .clock
            .now_ms()
            .saturating_add(bound_secs.saturating_mul(1000));
        loop {
            match self.ready(port) {
                None => {
                    return Err(Done::fail(
                        Reason::NotReady,
                        format!("the instance on {port} does not answer /readyz"),
                    ));
                }
                Some(CacheMode::ReadOnly) => {}
                Some(_) => return Ok(true),
            }
            if self.clock.now_ms() >= deadline {
                return Ok(false);
            }
            self.clock.pause(POLL_INTERVAL);
        }
    }
}

/// A success that says whether the promotion is still pending. The detail says that it is, and
/// not who holds the cache lock: no instance says that, and after the commit the previous one
/// may be gone while the lock is still not taken.
pub(in crate::server::update_hook) fn observed(promoted: bool, port: u16) -> Done {
    let mut done = Done::ok(if promoted {
        format!("{port} is read-write")
    } else {
        format!("{port} serves read-only: the promotion is pending")
    });
    done.report.promotion_pending = !promoted;
    done
}
