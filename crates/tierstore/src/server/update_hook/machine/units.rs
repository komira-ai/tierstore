//! The units of the two instances, through `systemctl`.
//!
//! A unit whose `ConditionPathExists=` fails is skipped by `systemctl start`: systemd.unit(5)
//! says a failing condition leaves the unit out of the `failed` state and the start "(mostly
//! silently) skipped". The manual does not state `systemctl`'s exit status for that case; it is
//! 0 on the systemd versions this was written against, and an exit that is not 0 is reported as
//! a failure here like any other.

use std::time::Duration;

use super::super::ports::{Instance, PortError, PortKind, UnitState, Units};
use super::runner::{Call, Runner, checked, run};

/// Where the Debian and Ubuntu package puts `systemctl`.
pub const DEFAULT_PROGRAM: &str = super::SYSTEMCTL_PROGRAM;
const KIND: PortKind = PortKind::Units;

/// `systemctl`, called through a [`Runner`].
pub struct SystemdUnits<'a> {
    runner: &'a dyn Runner,
    start_timeout: Duration,
    stop_timeout: Duration,
    show_timeout: Duration,
    program: String,
}

impl<'a> SystemdUnits<'a> {
    /// Calls `systemctl` through `runner`. A start that runs past `start_timeout`, a stop
    /// (which waits for the drain) past `stop_timeout` and a look at the state past
    /// `show_timeout` are killed and fail.
    #[must_use]
    pub fn new(
        runner: &'a dyn Runner,
        start_timeout: Duration,
        stop_timeout: Duration,
        show_timeout: Duration,
    ) -> SystemdUnits<'a> {
        SystemdUnits {
            runner,
            start_timeout,
            stop_timeout,
            show_timeout,
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

/// The state `systemctl show --property=ActiveState --value` printed, or `None` for a word that
/// is none of the states.
fn state_of(text: &str) -> Option<UnitState> {
    match text.trim() {
        "active" | "reloading" => Some(UnitState::Active),
        "activating" => Some(UnitState::Activating),
        "deactivating" => Some(UnitState::Deactivating),
        "inactive" => Some(UnitState::Inactive),
        "failed" => Some(UnitState::Failed),
        _ => None,
    }
}

impl Units for SystemdUnits<'_> {
    fn start(&self, instance: Instance) -> Result<(), PortError> {
        let unit = instance.unit();
        let call = Call::new(
            &self.program,
            &["start", "--", unit.as_str()],
            self.start_timeout,
        );
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }

    fn stop(&self, instance: Instance) -> Result<(), PortError> {
        let unit = instance.unit();
        let call = Call::new(
            &self.program,
            &["stop", "--", unit.as_str()],
            self.stop_timeout,
        );
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }

    fn state(&self, instance: Instance) -> Result<UnitState, PortError> {
        let unit = instance.unit();
        let call = Call::new(
            &self.program,
            &[
                "show",
                "--property=ActiveState",
                "--value",
                "--",
                unit.as_str(),
            ],
            self.show_timeout,
        );
        let output = checked(run(self.runner, &call, KIND)?, &call, KIND)?;
        let text = String::from_utf8_lossy(&output.stdout);
        state_of(&text).ok_or_else(|| {
            PortError::failed(
                KIND,
                format!("{}: unknown ActiveState {:?}", call.describe(), text.trim()),
            )
        })
    }
}
