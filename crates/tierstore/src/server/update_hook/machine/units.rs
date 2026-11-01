//! The units, through `systemctl`.
//!
//! A unit whose `ConditionPathExists=` fails is skipped by `systemctl start`: systemd.unit(5)
//! says a failing condition leaves the unit out of the `failed` state and the start "(mostly
//! silently) skipped". The manual does not state `systemctl`'s exit status for that case; it is
//! 0 on the systemd versions this was written against, and an exit that is not 0 is reported as
//! a failure here like any other.

use std::time::Duration;

use super::super::ports::{PortError, PortKind, Units};
use super::runner::{Call, Runner, checked, run};

const PROGRAM: &str = "systemctl";
const KIND: PortKind = PortKind::Units;

/// `systemctl`, called through a [`Runner`].
pub struct SystemdUnits<'a> {
    runner: &'a dyn Runner,
    start_timeout: Duration,
    stop_timeout: Duration,
}

impl<'a> SystemdUnits<'a> {
    /// Calls `systemctl` through `runner`. A start that runs past `start_timeout` and a stop
    /// (which waits for the drain) past `stop_timeout` are killed and fail.
    #[must_use]
    pub fn new(
        runner: &'a dyn Runner,
        start_timeout: Duration,
        stop_timeout: Duration,
    ) -> SystemdUnits<'a> {
        SystemdUnits {
            runner,
            start_timeout,
            stop_timeout,
        }
    }
}

impl<'a> SystemdUnits<'a> {
    /// Not yet: the program is still found by name.
    #[must_use]
    pub fn with_program(self, _program: &str) -> SystemdUnits<'a> {
        self
    }
}

impl Units for SystemdUnits<'_> {
    fn start(&self, unit: &str) -> Result<(), PortError> {
        let call = Call::new(PROGRAM, &["start", unit], self.start_timeout);
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }

    fn stop(&self, unit: &str) -> Result<(), PortError> {
        let call = Call::new(PROGRAM, &["stop", unit], self.stop_timeout);
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }
}
