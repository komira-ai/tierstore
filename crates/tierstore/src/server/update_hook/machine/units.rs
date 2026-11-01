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

/// Where the Debian and Ubuntu package puts `systemctl`.
pub const DEFAULT_PROGRAM: &str = "/usr/bin/systemctl";
const KIND: PortKind = PortKind::Units;

/// `systemctl`, called through a [`Runner`].
pub struct SystemdUnits<'a> {
    runner: &'a dyn Runner,
    start_timeout: Duration,
    stop_timeout: Duration,
    program: String,
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
            program: DEFAULT_PROGRAM.to_owned(),
        }
    }

    /// The same, calling `program` (an absolute path) instead of the default.
    #[must_use]
    pub fn with_program(mut self, program: &str) -> SystemdUnits<'a> {
        program.clone_into(&mut self.program);
        self
    }
}

/// Whether `unit` is a name the port will pass on: 1 to 255 characters of letters, digits and
/// `: _ . @ -`, starting with a letter or digit. Anything else is refused before a program runs.
fn check_unit(unit: &str) -> Result<(), PortError> {
    let name = unit.len() <= 255
        && unit
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":_.@-".contains(c));
    if name {
        return Ok(());
    }
    Err(PortError::failed(
        KIND,
        format!("{unit:?} is not a unit name"),
    ))
}

impl Units for SystemdUnits<'_> {
    fn start(&self, unit: &str) -> Result<(), PortError> {
        check_unit(unit)?;
        let call = Call::new(&self.program, &["start", "--", unit], self.start_timeout);
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }

    fn stop(&self, unit: &str) -> Result<(), PortError> {
        check_unit(unit)?;
        let call = Call::new(&self.program, &["stop", "--", unit], self.stop_timeout);
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }
}
