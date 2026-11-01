//! The steps on a machine: builds the [`Ports`](super::Ports) from the machine's implementations
//! and runs the step over them.

use std::path::PathBuf;
use std::time::Duration;

use rustix::process::{getegid, geteuid};

use super::args::{HostArgs, Invocation};
use super::declaration::Declaration;
use super::machine::{
    Account, LoopbackInstances, MonotonicClock, NftTool, Runner, SlotLinks, StateFiles, SystemHost,
    SystemRunner, SystemdUnits,
};
use super::params::Params;
use super::ports::{Host, Ports};
use super::report::{Done, Reason};
use super::steps::run_step;

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

/// Where the machine is and who owns its state directory.
pub(super) struct Config<'a> {
    /// The root the host's files are read under (`/` on a host).
    pub root: PathBuf,
    /// The user id that must own the state directory and the record.
    pub owner: u32,
    /// The group id that must own the state directory.
    pub group: u32,
    /// The user id that must own the binary of a staged release.
    pub release_owner: u32,
    /// The user id that must own the declaration.
    pub declaration_owner: u32,
    /// The `nft` program.
    pub nft: PathBuf,
    /// The `systemctl` program.
    pub systemctl: PathBuf,
    /// What runs the programs.
    pub runner: &'a dyn Runner,
}

/// The declaration of the host, read through the host port. It names the state directory, so it
/// is read before the state port exists; the steps read it again, under the lock, and check
/// the record against that reading.
fn declaration(host: &SystemHost<'_>) -> Result<Declaration, Done> {
    Ok(Declaration::parse(&host.declaration()?)?)
}

/// Runs the step on the machine described by `config`: the ports are the machine's, with the
/// state directory and the links taken from the declaration, and the step is run over them.
pub(super) fn run_on(
    config: &Config<'_>,
    invocation: &Invocation,
    params: Option<&Params>,
) -> Done {
    let runner = config.runner;
    let host = SystemHost::new(config.root.clone(), runner, CAPABILITIES_TIMEOUT)
        .with_release_owner(config.release_owner)
        .with_declaration_owner(config.declaration_owner);
    let decl = match declaration(&host) {
        Ok(decl) => decl,
        Err(done) => return done,
    };
    let Some(service) = decl.service.clone() else {
        return Done::fail(
            Reason::StateInvalid,
            "the declaration names no service account (uid)",
        );
    };
    let nft = NftTool::new(runner, NFT_TIMEOUT).with_program(&config.nft.to_string_lossy());
    let units = SystemdUnits::new(runner, START_TIMEOUT, STOP_TIMEOUT)
        .with_program(&config.systemctl.to_string_lossy());
    let instances = LoopbackInstances::new(CONNECT_TIMEOUT, READ_TIMEOUT);
    let clock = MonotonicClock::new();
    let account = Account {
        owner: config.owner,
        group: config.group,
        service,
        group_file: config.root.join("etc/group"),
    };
    let state = StateFiles::new(decl.state_dir.clone(), account);
    let slots = SlotLinks::new(&state);
    let ports = Ports {
        nft: &nft,
        units: &units,
        slots: &slots,
        instances: &instances,
        host: &host,
        clock: &clock,
        state: &state,
    };
    run_step(invocation.step, params, &ports)
}

/// The machine this process runs on: the host's own files under the root the command line
/// names (`/` unless `--root` says otherwise), the state directory owned by the user the hook runs
/// as (its own account, which owns the directory), a declaration and a staged release owned by
/// root (or the user `--root-uid` names), and the programs `--nft` and `--systemctl` name (the
/// host's own unless they say otherwise).
pub(super) fn system_config<'a>(runner: &'a dyn Runner, host: &HostArgs) -> Config<'a> {
    Config {
        root: host.root.clone(),
        owner: geteuid().as_raw(),
        group: getegid().as_raw(),
        release_owner: host.root_uid,
        declaration_owner: host.root_uid,
        nft: host.nft.clone(),
        systemctl: host.systemctl.clone(),
        runner,
    }
}

/// Runs the step on the host the process is running on.
pub(super) fn on_this_machine(invocation: &Invocation, params: Option<&Params>) -> Done {
    run_on(
        &system_config(&SystemRunner, &invocation.host),
        invocation,
        params,
    )
}
