//! The steps on a machine: builds the [`Ports`](super::Ports) from the machine's implementations
//! and runs the step over them.

use std::path::PathBuf;
use std::time::Duration;

use super::args::Invocation;
use super::machine::Runner;
use super::params::Params;
use super::report::{Done, Reason};

/// The longest an `nft` call may run.
pub(super) const NFT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest `systemctl start` may run.
pub(super) const START_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest `systemctl stop` may run: it returns when the drain is over.
pub(super) const STOP_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest `<release>/tierstore --capabilities` may run.
pub(super) const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a connect to an instance may take.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest an instance may leave a request unanswered.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The user that must own the state directory: the updater's, root.
pub(super) const UPDATER_UID: u32 = 0;

/// Where the machine is and who may own its state directory.
pub(super) struct Config<'a> {
    /// The root the host's files are read under (`/` on a host).
    pub root: PathBuf,
    /// The user id that must own the state directory.
    pub owner: u32,
    /// What runs the programs.
    pub runner: &'a dyn Runner,
}

/// Runs the step on the machine described by `config`.
pub(super) fn run_on(
    _config: &Config<'_>,
    invocation: &Invocation,
    _params: Option<&Params>,
) -> Done {
    let _ = (
        NFT_TIMEOUT,
        START_TIMEOUT,
        STOP_TIMEOUT,
        CAPABILITIES_TIMEOUT,
        CONNECT_TIMEOUT,
        READ_TIMEOUT,
    );
    Done::fail(
        Reason::NotImplemented,
        format!("{} is not wired to this machine yet", invocation.step),
    )
}

/// Runs the step on the host the process is running on.
pub(super) fn on_this_machine(invocation: &Invocation, params: Option<&Params>) -> Done {
    let config = Config {
        root: PathBuf::from("/"),
        owner: UPDATER_UID,
        runner: &super::machine::SystemRunner,
    };
    run_on(&config, invocation, params)
}
