//! The machine's side of the ports: what the hook does to the host it runs on.
//!
//! Each port of [`super::ports`] has one implementation here, over the real thing: programs
//! run through a [`Runner`] (an argument vector, never a shell), files under a root that tests
//! point at a scratch directory, and a loopback socket. Nothing here reads an environment
//! variable. [`super::Machine::system`] builds a [`super::Ports`] from these.
//!
//! Programs are called by absolute path (`/usr/sbin/nft`, `/usr/bin/systemctl`, the Debian and
//! Ubuntu locations); the command line may name others (`--nft`, `--systemctl`).

mod clock;
mod host;
mod instances;
mod nft;
mod runner;
mod slots;
mod state;
mod units;

use std::fmt;
use std::io;

pub use clock::MonotonicClock;
pub use host::{RELEASE_BINARY, SystemHost};
pub use instances::LoopbackInstances;
pub use nft::{DEFAULT_PROGRAM as NFT_PROGRAM, NftTool};
pub use runner::{Call, Output, RunError, Runner, SystemRunner};
pub use slots::SlotLinks;
pub use state::{Account, LOCK_FILE, STATE_FILE, StateFiles};
pub use units::{DEFAULT_PROGRAM as SYSTEMCTL_PROGRAM, SystemdUnits};

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
mod host_tests;
#[cfg(test)]
mod instances_tests;
#[cfg(test)]
mod nft_tests;
#[cfg(test)]
mod runner_tests;
#[cfg(test)]
mod slots_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod units_tests;
