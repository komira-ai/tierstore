//! The machine's side of the ports: what the hook does to the host it runs on.
//!
//! Each port of [`super::ports`] has one implementation here, over the real thing: programs
//! run through a [`Runner`] (an argument vector, never a shell), files under a root that tests
//! point at a scratch directory, a loopback socket, the Unix control socket of the instance
//! that serves, and the Unix socket of the node's mount provider. Nothing here reads an environment
//! variable. [`super::Machine::system`] builds the ports of an arm from these.
//!
//! Programs are called by absolute path (`/usr/bin/systemctl`, the Debian and Ubuntu location;
//! `/bin/launchctl` on macOS); the system wiring may name others. Every implementation is built
//! on every system, so that Linux runs the tests of the macOS ones.

mod clock;
mod control;
mod host;
mod instances;
mod launchd;
mod markers;
mod processes;
mod provider;
mod runner;
mod service_binary;
mod state;
mod units;

use std::fmt;
use std::io;

pub use clock::MonotonicClock;
pub use control::ControlSocket;
// The config hook's tests answer the `dscl` call by its program.
pub use host::{CONFIG_MAX, SystemHost};
#[cfg(test)]
pub(crate) use host::{DSCL_PROGRAM, SUDO_PROGRAM};
pub use instances::LoopbackInstances;
pub use launchd::{
    LAUNCHCTL_PROGRAM, LaunchctlJobs, LaunchdTimes, NEXT_IMAGE_SIGNAL,
    NOT_FOUND as LAUNCHCTL_NOT_FOUND, bootout_args, bootstrap_args, disable_args, enable_args,
    kill_args, print_args,
};
pub use markers::MarkerFiles;
pub use processes::ProcFiles;
pub use provider::{ProviderSocket, ProviderTimes};
pub use runner::{Call, Output, RunError, Runner, SystemRunner};
pub use state::{CONFIG_STATE_FILE, LOCK_FILE, STATE_FILE, StateFiles};
pub use units::{GuardUnit, SystemdUnits};

/// Where the Debian and Ubuntu package puts `systemctl`.
pub const SYSTEMCTL_PROGRAM: &str = "/usr/bin/systemctl";

/// What went wrong, as text, before the port that saw it says which port that is.
pub(super) struct Fault(pub(super) String);

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<io::Error> for Fault {
    fn from(error: io::Error) -> Fault {
        Fault(error.to_string())
    }
}

impl From<&str> for Fault {
    fn from(text: &str) -> Fault {
        Fault(text.to_owned())
    }
}

impl From<String> for Fault {
    fn from(text: String) -> Fault {
        Fault(text)
    }
}

#[cfg(test)]
mod clock_tests;
#[cfg(test)]
mod control_tests;
#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod instances_tests;
#[cfg(test)]
mod launchd_tests;
#[cfg(test)]
mod markers_tests;
#[cfg(test)]
mod processes_tests;
#[cfg(test)]
mod provider_tests;
#[cfg(test)]
mod runner_tests;
#[cfg(test)]
mod service_binary_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod units_tests;
