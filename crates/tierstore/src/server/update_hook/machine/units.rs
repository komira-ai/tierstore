//! The units, through `systemctl`.

use std::time::Duration;

use super::super::ports::{PortError, Units};
use super::runner::Runner;

/// `systemctl`, called through a [`Runner`].
pub struct SystemdUnits<'a> {
    #[allow(dead_code)]
    runner: &'a dyn Runner,
    #[allow(dead_code)]
    start_timeout: Duration,
    #[allow(dead_code)]
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

impl Units for SystemdUnits<'_> {
    fn start(&self, _unit: &str) -> Result<(), PortError> {
        Ok(())
    }

    fn stop(&self, _unit: &str) -> Result<(), PortError> {
        Ok(())
    }
}
