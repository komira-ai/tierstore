//! The machine's side of the ports: what the hook does to the host it runs on.
//!
//! Each port of [`super::ports`] has one implementation here, over the real thing: programs
//! run through a [`Runner`] (an argument vector, never a shell), files under a root that tests
//! point at a scratch directory, and a loopback socket. Nothing here reads an environment
//! variable. [`super::Machine::system`] builds a [`super::Ports`] from these.
//!
//! Programs are called by absolute path (`/usr/sbin/nft`, `/usr/bin/systemctl`, the Debian and
//! Ubuntu locations; `/bin/launchctl` on macOS); the command line may name others (`--nft`,
//! `--systemctl`). `nft`, `systemctl` and the slot links of the two-slot hand-over exist on
//! Linux alone, and so are built there alone; the launchd service, the release links and
//! everything else are built on both systems, so that Linux runs their tests.

mod clock;
mod host;
mod instances;
mod launchd;
#[cfg(target_os = "linux")]
mod nft;
mod releases;
mod runner;
#[cfg(target_os = "linux")]
mod slots;
mod state;
#[cfg(target_os = "linux")]
mod units;

use std::fmt;
use std::io;

pub use clock::MonotonicClock;
pub use host::{RELEASE_BINARY, SystemHost};
pub use instances::LoopbackInstances;
pub use launchd::{
    LAUNCHCTL_PROGRAM, LAUNCHD_LABEL, LAUNCHD_LINKS_DIR, LAUNCHD_PLIST, LAUNCHD_PROGRAM,
    LaunchdService, LaunchdTimes, NOT_FOUND as LAUNCHCTL_NOT_FOUND, bootout_args, bootstrap_args,
    kickstart_args, print_args,
};
#[cfg(target_os = "linux")]
pub use nft::NftTool;
pub use releases::ReleaseLinks;
pub use runner::{Call, Output, RunError, Runner, SystemRunner};
#[cfg(target_os = "linux")]
pub use slots::SlotLinks;
pub use state::{Account, LOCK_FILE, STATE_FILE, StateFiles};
#[cfg(target_os = "linux")]
pub use units::SystemdUnits;

/// Where the Debian and Ubuntu package puts `nft`.
pub const NFT_PROGRAM: &str = "/usr/sbin/nft";
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
mod host_tests;
#[cfg(test)]
mod instances_tests;
#[cfg(test)]
mod launchd_tests;
#[cfg(test)]
#[cfg(target_os = "linux")]
mod nft_tests;
#[cfg(test)]
mod releases_tests;
#[cfg(test)]
mod runner_tests;
#[cfg(test)]
#[cfg(target_os = "linux")]
mod slots_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
#[cfg(target_os = "linux")]
mod units_tests;
